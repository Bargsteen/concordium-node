use byteorder::{NetworkEndian, WriteBytesExt};
use bytesize::ByteSize;
use failure::Fallible;
use mio::tcp::TcpStream;
use noiseexplorer_xx::{
    consts::{DHLEN, MAC_LENGTH},
    noisesession::NoiseSession,
    types::Keypair,
};
use priority_queue::PriorityQueue;

use super::{Connection, DeduplicationQueues, PendingPriority};
use crate::network::PROTOCOL_MAX_MESSAGE_SIZE;

use std::{
    cmp,
    collections::VecDeque,
    convert::TryInto,
    io::{Cursor, ErrorKind, Read, Seek, SeekFrom, Write},
    mem,
    sync::{atomic::Ordering, Arc, RwLock},
    time::Duration,
};

/// The size of the noise message payload.
type PayloadSize = u32;
const PAYLOAD_SIZE: usize = mem::size_of::<PayloadSize>();
const PROLOGUE: &[u8] = b"CP2P";
pub const NOISE_MAX_MESSAGE_LEN: usize = 64 * 1024 - 1; // 65535
const NOISE_AUTH_TAG_LEN: usize = 16;
pub const NOISE_MAX_PAYLOAD_LEN: usize = NOISE_MAX_MESSAGE_LEN - NOISE_AUTH_TAG_LEN;
pub const HANDSHAKE_SIZE_LIMIT: usize = 1024;
/// Not really a PSK, but serves a PSK-like function
pub const PSK: &[u8] = b"b6461bd246843f70ac1328401405b2b4e725994d7d144a75bff1a04a247d64b7";
/// The size of the initial socket write queue allocation.
const WRITE_QUEUE_ALLOC: usize = 1024 * 1024;

/// A single encrypted message currently being read from the socket.
#[derive(Default)]
struct IncomingMessage {
    /// Contains bytes comprising the length of the message.
    size_bytes: Vec<u8>,
    /// The number of bytes remaining to be read in order to complete the
    /// current message.
    pending_bytes: usize,
    /// The encrypted message currently being read.
    message: Vec<u8>,
}

/// A buffer used to handle reads/writes to the socket.
struct SocketBuffer {
    /// The socket read/write buffer.
    buf: Box<[u8]>,
    /// The buffer's offset.
    offset: usize,
    /// The bytes remaining from the last read from the socket.
    remaining: usize,
}

impl SocketBuffer {
    fn new(socket_read_size: usize) -> Self {
        Self {
            buf:       vec![0u8; socket_read_size].into_boxed_slice(),
            offset:    0,
            remaining: 0,
        }
    }
}

impl SocketBuffer {
    #[inline]
    fn is_exhausted(&self) -> bool { self.offset == self.buf.len() }

    #[inline]
    fn slice(&self, len: usize) -> &[u8] { &self.buf[self.offset..][..len] }

    #[inline]
    fn slice_mut(&mut self, len: usize) -> &mut [u8] { &mut self.buf[self.offset..][..len] }

    #[inline]
    fn shift(&mut self, offset: usize) {
        self.offset += offset;
        self.remaining -= offset;
    }

    #[inline]
    fn reset(&mut self) {
        self.offset = 0;
        self.remaining = 0;
    }
}

/// A type used to indicate what the result of the current read from the socket
/// is.
enum ReadResult {
    /// A single message was fully read.
    Complete(Vec<u8>),
    /// The currently read message is incomplete - further reads are needed.
    Incomplete,
    /// The current attempt to read from the socket would be blocking.
    WouldBlock,
}

/// The `Connection`'s socket, noise session and some helper objects.
pub struct ConnectionLowLevel {
    /// A reference to the parent `Connection` object.
    pub conn_ref: Option<Arc<Connection>>,
    /// The socket associated with the connection.
    pub socket: TcpStream,
    noise_session: NoiseSession,
    noise_buffer: Box<[u8]>,
    socket_buffer: SocketBuffer,
    incoming_msg: IncomingMessage,
    /// A priority queue for bytes waiting to be written to the socket.
    output_queue: VecDeque<u8>,
}

macro_rules! recv_xx_msg {
    ($self:ident, $len:expr, $idx:expr) => {
        let msg = $self.socket_buffer.slice_mut($len);
        $self.noise_session.recv_message(msg)?;
        trace!("I got message {}", $idx);
    };
}

macro_rules! send_xx_msg {
    ($self:ident, $prefix_len:expr, $payload:expr, $suffix_len:expr, $idx:expr) => {
        let mut msg = vec![];
        // prepend the plaintext message length
        msg.write_u32::<NetworkEndian>(($prefix_len + $payload.len() + $suffix_len) as u32)?;
        // provide buffer space for the handshake prefix
        msg.append(&mut vec![0u8; $prefix_len]);
        // add a payload
        msg.extend($payload);
        // add room for handshake suffix
        msg.append(&mut vec![0u8; $suffix_len]);
        // write the message into the buffer
        $self.noise_session.send_message(&mut msg[PAYLOAD_SIZE..])?;
        // queue and send the message
        trace!("Sending message {}", $idx);
        $self.output_queue.extend(msg);
        $self.flush_socket()?;
    };
}

impl ConnectionLowLevel {
    /// Obtain a reference to the partent object.
    pub fn conn(&self) -> &Connection {
        &self.conn_ref.as_ref().unwrap() // safe; always available
    }

    /// Creates a new `ConnectionLowLevel` object.
    pub fn new(socket: TcpStream, is_initiator: bool, socket_read_size: usize) -> Self {
        if let Err(e) = socket.set_linger(Some(Duration::from_secs(0))) {
            error!("Can't set SOLINGER for socket {:?}: {}", socket, e);
        }

        trace!(
            "Starting a noise session as the {}; handshake mode: XX",
            if is_initiator {
                "initiator"
            } else {
                "responder"
            }
        );

        ConnectionLowLevel {
            conn_ref: None,
            socket,
            noise_session: NoiseSession::init_session(is_initiator, PROLOGUE, Keypair::default()),
            noise_buffer: vec![0u8; NOISE_MAX_MESSAGE_LEN].into_boxed_slice(),
            socket_buffer: SocketBuffer::new(socket_read_size),
            incoming_msg: IncomingMessage::default(),
            output_queue: VecDeque::with_capacity(WRITE_QUEUE_ALLOC),
        }
    }

    // the XX noise handshake

    /// Immediately sends the XX-A handshake message
    pub fn send_handshake_message_a(&mut self) -> Fallible<()> {
        let pad = 16;
        send_xx_msg!(self, DHLEN, PSK, pad, "A");
        self.conn().set_sent_handshake();

        Ok(())
    }

    fn process_msg_a(&mut self, len: usize) -> Fallible<Vec<u8>> {
        recv_xx_msg!(self, len, "A");
        let pad = 16;
        let payload_in = self.socket_buffer.slice(len)[DHLEN..][..len - DHLEN - pad].try_into()?;
        let payload_out = self.conn().handler.produce_handshake_request()?;
        send_xx_msg!(self, DHLEN * 2 + MAC_LENGTH, &payload_out, MAC_LENGTH, "B");
        self.conn().set_sent_handshake();

        Ok(payload_in)
    }

    fn process_msg_b(&mut self, len: usize) -> Fallible<Vec<u8>> {
        recv_xx_msg!(self, len, "B");
        let payload_in = self.socket_buffer.slice(len)[DHLEN * 2 + MAC_LENGTH..]
            [..len - DHLEN * 2 - MAC_LENGTH * 2]
            .try_into()?;
        let payload_out = self.conn().handler.produce_handshake_request()?;
        send_xx_msg!(self, DHLEN + MAC_LENGTH, &payload_out, MAC_LENGTH, "C");
        self.conn().handler.stats.peers_inc();

        Ok(payload_in)
    }

    fn process_msg_c(&mut self, len: usize) -> Fallible<Vec<u8>> {
        recv_xx_msg!(self, len, "C");
        let payload = self.socket_buffer.slice(len)[DHLEN + MAC_LENGTH..]
            [..len - DHLEN - MAC_LENGTH * 2]
            .try_into()?;
        self.conn().handler.stats.peers_inc();

        Ok(payload)
    }

    #[inline]
    /// Checks whether the low-level noise handshake is complete.
    fn is_post_handshake(&self) -> bool {
        if self.noise_session.is_initiator() {
            self.noise_session.get_message_count() > 1
        } else {
            self.noise_session.get_message_count() > 2
        }
    }

    // input

    /// Keeps reading from the socket as long as there is data to be read
    /// and the operation is not blocking.
    #[inline]
    pub fn read_stream(&mut self, dedup_queues: &DeduplicationQueues) -> Fallible<()> {
        loop {
            match self.read_from_socket()? {
                ReadResult::Complete(msg) => {
                    self.conn().process_message(Arc::from(msg), dedup_queues)?
                }
                ReadResult::Incomplete | ReadResult::WouldBlock => return Ok(()),
            }
        }
    }

    /// Attempts to read a complete message from the socket.
    #[inline]
    fn read_from_socket(&mut self) -> Fallible<ReadResult> {
        if self.socket_buffer.is_exhausted() {
            self.socket_buffer.reset();
        }
        // if there's any carryover bytes to be read from the socket buffer,
        // process them before reading from the socket again
        if self.socket_buffer.remaining == 0 {
            let len = self.read_size() - self.socket_buffer.offset;
            match self.socket.read(self.socket_buffer.slice_mut(len)) {
                Ok(num_bytes) => {
                    // trace!(
                    //     "Read {} from the socket",
                    //     ByteSize(num_bytes as u64).to_string_as(true)
                    // );
                    self.socket_buffer.remaining = num_bytes;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(ReadResult::WouldBlock),
                Err(e) => return Err(e.into()),
            }
        };

        // if we don't know the length of the incoming message, read it from the
        // collected bytes; that number of bytes needs to be accounted for later
        if self.incoming_msg.pending_bytes == 0 {
            self.attempt_to_read_length()?;
        }

        // check if we know the size of the message now
        if self.incoming_msg.pending_bytes != 0 {
            self.process_incoming_msg()
        } else {
            Ok(ReadResult::Incomplete)
        }
    }

    /// Attempt to discover the length of the incoming encrypted message.
    #[inline]
    fn attempt_to_read_length(&mut self) -> Fallible<()> {
        let read_size = cmp::min(
            self.socket_buffer.remaining,
            PAYLOAD_SIZE - self.incoming_msg.size_bytes.len(),
        );
        self.incoming_msg.size_bytes.write_all(self.socket_buffer.slice(read_size))?;
        self.socket_buffer.shift(read_size);

        if self.incoming_msg.size_bytes.len() == PAYLOAD_SIZE {
            let expected_size =
                PayloadSize::from_be_bytes((&self.incoming_msg.size_bytes[..]).try_into()?);
            self.incoming_msg.size_bytes.clear();

            if expected_size == 0 {
                bail!("I got a zero-sized message");
            }

            if !self.is_post_handshake() && expected_size >= HANDSHAKE_SIZE_LIMIT as u32 {
                bail!(
                    "expected message size ({}) exceeds the handshake size limit ({})",
                    ByteSize(expected_size as u64).to_string_as(true),
                    ByteSize(HANDSHAKE_SIZE_LIMIT as u64).to_string_as(true),
                );
            }

            // check if the expected size doesn't exceed the protocol limit
            if expected_size > PROTOCOL_MAX_MESSAGE_SIZE {
                bail!(
                    "expected message size ({}) exceeds the maximum protocol size ({})",
                    ByteSize(expected_size as u64).to_string_as(true),
                    ByteSize(PROTOCOL_MAX_MESSAGE_SIZE as u64).to_string_as(true)
                );
            }

            trace!("Expecting a {} message", ByteSize(expected_size as u64).to_string_as(true));
            self.incoming_msg.pending_bytes = expected_size as usize;
            self.incoming_msg.message = Vec::with_capacity(expected_size as usize);
        }

        Ok(())
    }

    /// As long as the length of the incoming message is already known and there
    /// are bytes pending to be processed, register them as part of the
    /// current message and decrypt it when all bytes have been read.
    #[inline]
    fn process_incoming_msg(&mut self) -> Fallible<ReadResult> {
        let to_read = cmp::min(self.incoming_msg.pending_bytes, self.socket_buffer.remaining);

        self.incoming_msg.message.write_all(self.socket_buffer.slice(to_read))?;
        self.incoming_msg.pending_bytes -= to_read;

        if self.is_post_handshake() {
            self.socket_buffer.shift(to_read);
        }

        if self.incoming_msg.pending_bytes == 0 {
            trace!("The message was fully read");

            if !self.is_post_handshake() {
                let payload = match self.noise_session.get_message_count() {
                    0 if !self.noise_session.is_initiator() => self.process_msg_a(to_read),
                    1 if self.noise_session.is_initiator() => self.process_msg_b(to_read),
                    2 if !self.noise_session.is_initiator() => self.process_msg_c(to_read),
                    _ => bail!("invalid XX handshake"),
                }?;

                if !self.noise_session.is_initiator() {
                    if self.noise_session.get_message_count() == 1 && payload != PSK {
                        bail!("Invalid PSK");
                    } else if self.noise_session.get_message_count() == 2 {
                        // message C doesn't carry a payload; break the reading loop
                        self.socket_buffer.reset();
                        return Ok(ReadResult::Incomplete);
                    }
                }

                self.socket_buffer.reset();
                Ok(ReadResult::Complete(payload))
            } else {
                Ok(ReadResult::Complete(self.decrypt()?))
            }
        } else {
            Ok(ReadResult::Incomplete)
        }
    }

    /// Decrypt a full message read from the socket.
    #[inline]
    fn decrypt(&mut self) -> Fallible<Vec<u8>> {
        let mut msg = Cursor::new(mem::replace(&mut self.incoming_msg.message, Vec::new()));
        // calculate the number of full-sized chunks
        let len = msg.get_ref().len();
        let num_full_chunks = len / NOISE_MAX_MESSAGE_LEN;
        // calculate the number of the last, incomplete chunk (if there is one)
        let last_chunk_size = len % NOISE_MAX_MESSAGE_LEN;
        let num_all_chunks = num_full_chunks
            + if last_chunk_size > 0 {
                1
            } else {
                0
            };

        // decrypt the chunks
        for i in 0..num_all_chunks {
            self.decrypt_chunk(&mut msg, i)?;
        }

        let mut msg = msg.into_inner();
        msg.truncate(len - num_all_chunks * MAC_LENGTH);

        Ok(msg)
    }

    /// Decrypt a single chunk of the received encrypted message.
    #[inline]
    fn decrypt_chunk(&mut self, msg: &mut Cursor<Vec<u8>>, offset_mul: usize) -> Fallible<()> {
        msg.seek(SeekFrom::Start((offset_mul * NOISE_MAX_MESSAGE_LEN) as u64))?;
        let read_size =
            cmp::min(NOISE_MAX_MESSAGE_LEN, msg.get_ref().len() - msg.position() as usize);
        msg.read_exact(&mut self.noise_buffer[..read_size])?;
        msg.seek(SeekFrom::Start((offset_mul * NOISE_MAX_PAYLOAD_LEN) as u64))?;

        if let Err(err) = self.noise_session.recv_message(&mut self.noise_buffer[..read_size]) {
            Err(err.into())
        } else {
            msg.write_all(&self.noise_buffer[..read_size - MAC_LENGTH])?;
            Ok(())
        }
    }

    // output

    /// Enqueue a message to be written to the socket.
    #[inline]
    pub fn write_to_socket(&mut self, input: Arc<[u8]>) -> Fallible<()> {
        self.conn().handler.connection_handler.total_sent.fetch_add(1, Ordering::Relaxed);
        self.conn().stats.messages_sent.fetch_add(1, Ordering::Relaxed);
        self.conn().stats.bytes_sent.fetch_add(input.len() as u64, Ordering::Relaxed);
        self.conn().handler.stats.pkt_sent_inc();

        if cfg!(feature = "network_dump") {
            self.conn().send_to_dump(input.clone(), false);
        }

        self.encrypt_and_enqueue(&input)
    }

    /// Writes enequeued bytes to the socket until the queue is exhausted
    /// or the write would be blocking.
    #[inline]
    pub fn flush_socket(&mut self) -> Fallible<()> {
        while !self.output_queue.is_empty() {
            match self.flush_socket_once() {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }

    /// Writes a single batch of enqueued bytes to the socket.
    #[inline]
    fn flush_socket_once(&mut self) -> Fallible<usize> {
        let write_size = cmp::min(self.write_size(), self.output_queue.len());

        let (front, back) = self.output_queue.as_slices();

        let front_len = cmp::min(front.len(), write_size);
        self.socket_buffer.buf[..front_len].copy_from_slice(&front[..front_len]);

        let back_len = write_size - front_len;
        if back_len > 0 {
            self.socket_buffer.buf[front_len..][..back_len].copy_from_slice(&back[..back_len]);
        }

        let written = match self.socket.write(&self.socket_buffer.buf[..write_size]) {
            Ok(num_bytes) => num_bytes,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(0),
            Err(e) => return Err(e.into()),
        };

        self.output_queue.drain(..written);

        // trace!(
        //     "Written {} to the socket",
        //     ByteSize(written as u64).to_string_as(true)
        // );

        Ok(written)
    }

    /// It encrypts `input` and enqueues the encrypted chunks preceded by the
    /// length for later sending.
    #[inline]
    fn encrypt_and_enqueue(&mut self, input: &[u8]) -> Fallible<()> {
        let num_full_chunks = input.len() / NOISE_MAX_PAYLOAD_LEN;
        let last_chunk_len = {
            let rem = input.len() % NOISE_MAX_PAYLOAD_LEN;
            if rem != 0 {
                rem + MAC_LENGTH
            } else {
                0
            }
        };
        let full_msg_len = num_full_chunks * NOISE_MAX_MESSAGE_LEN + last_chunk_len;

        self.output_queue.extend(&(full_msg_len as PayloadSize).to_be_bytes());

        let mut input = Cursor::new(input);
        let eof = input.get_ref().len() as u64;

        while input.position() != eof {
            self.encrypt_chunk(&mut input)?;

            if self.output_queue.len() >= self.write_size() {
                self.flush_socket_once()?;
            }
        }

        Ok(())
    }

    /// Produces and enqueues a single noise message from `input`, potentially
    /// squeezing it with the previously enqueued chunk.
    #[inline]
    fn encrypt_chunk(&mut self, input: &mut Cursor<&[u8]>) -> Fallible<()> {
        let remaining_len = input.get_ref().len() - input.position() as usize;
        let chunk_size = cmp::min(NOISE_MAX_PAYLOAD_LEN, remaining_len);
        input.read_exact(&mut self.noise_buffer[..chunk_size])?;
        let encrypted_len = chunk_size + MAC_LENGTH;

        self.noise_session.send_message(&mut self.noise_buffer[..encrypted_len])?;

        self.output_queue.extend(&self.noise_buffer[..encrypted_len]);

        Ok(())
    }

    /// Get the desired socket read size.
    #[inline]
    fn read_size(&self) -> usize { self.socket_buffer.buf.len() }

    /// Get the desired socket write size.
    #[inline]
    fn write_size(&self) -> usize { self.conn().handler.config.socket_write_size }

    /// Processes a queue with pending messages, writing them to the socket.
    #[inline]
    pub fn send_pending_messages(
        &mut self,
        pending_messages: &RwLock<PriorityQueue<Arc<[u8]>, PendingPriority>>,
    ) -> Fallible<()> {
        let mut pending_messages = write_or_die!(pending_messages);

        while let Some((msg, _)) = pending_messages.pop() {
            trace!(
                "Attempting to send {} to {}",
                ByteSize(msg.len() as u64).to_string_as(true),
                self.conn()
            );

            if let Err(err) = self.write_to_socket(msg) {
                bail!("Can't send a raw network request: {}", err);
            }
        }

        Ok(())
    }
}
