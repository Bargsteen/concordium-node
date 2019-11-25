use byteorder::{NetworkEndian, WriteBytesExt};
use bytesize::ByteSize;
use failure::Fallible;
use mio::tcp::TcpStream;
use noiseexplorer_xx::consts::{DHLEN, MAC_LENGTH};

use super::{
    noise_impl::{
        finalize_handshake, start_noise_session, NoiseSession, NOISE_MAX_MESSAGE_LEN,
        NOISE_MAX_PAYLOAD_LEN,
    },
    Connection, DeduplicationQueues,
};
use crate::{common::counter::TOTAL_MESSAGES_SENT_COUNTER, network::PROTOCOL_MAX_MESSAGE_SIZE};
use concordium_common::hybrid_buf::HybridBuf;

use std::{
    cmp,
    collections::VecDeque,
    convert::TryInto,
    io::{Cursor, ErrorKind, Read, Write},
    mem,
    pin::Pin,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

/// The size of the noise message payload.
type PayloadSize = u32;
const PAYLOAD_SIZE: usize = mem::size_of::<PayloadSize>();
/// The size of the initial socket write queue allocation.
const WRITE_QUEUE_ALLOC: usize = 1024 * 1024;

/// A single encrypted message currently being read from the socket.
#[derive(Default)]
struct IncomingMessage {
    /// Contains bytes comprising the length of the message.
    size_bytes: Vec<u8>,
    /// The number of bytes remaining to be read in order to complete the
    /// current message.
    pending_bytes: PayloadSize,
    /// The encrypted message currently being read.
    message: HybridBuf,
}

/// The buffers used to encrypt/decrypt noise messages and contain reads from
/// the socket.
struct Buffers {
    /// The default buffer.
    main: Box<[u8]>,
    /// A buffer used when we've read more data than needed for the currently
    /// read message; it preserves that data so that the currenlt message
    /// can be decrypted using the default buffer.
    secondary: Box<[u8]>,
    /// The length of the data in the secondary buffer (if there is any data of
    /// interest).
    secondary_len: Option<usize>,
}

impl Buffers {
    fn new(socket_read_size: usize) -> Self {
        Self {
            main:          vec![0u8; socket_read_size].into_boxed_slice(),
            secondary:     vec![0u8; socket_read_size].into_boxed_slice(),
            secondary_len: None,
        }
    }
}

impl IncomingMessage {
    /// Checks whether the length of the currently read message is known.
    fn is_size_known(&mut self) -> Fallible<bool> {
        if self.pending_bytes != 0 {
            Ok(true)
        } else if self.size_bytes.len() == PAYLOAD_SIZE {
            let expected_size =
                PayloadSize::from_be_bytes((&self.size_bytes[..]).try_into().unwrap());
            self.size_bytes.clear();

            // check if the expected size doesn't exceed the protocol limit
            if expected_size > PROTOCOL_MAX_MESSAGE_SIZE as PayloadSize {
                bail!(
                    "expected message size ({}) exceeds the maximum protocol size ({})",
                    ByteSize(expected_size as u64).to_string_as(true),
                    ByteSize(PROTOCOL_MAX_MESSAGE_SIZE as u64).to_string_as(true)
                );
            }

            trace!(
                "Expecting a {} message",
                ByteSize(expected_size as u64).to_string_as(true)
            );
            self.pending_bytes = expected_size;

            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// A type used to indicate what the result of the current read from the socket
/// is.
enum ReadResult {
    /// A single message was fully read.
    Complete(HybridBuf),
    /// The currently read message is incomplete - further reads are needed.
    Incomplete,
    /// The current attempt to read from the socket would be blocking.
    WouldBlock,
}

pub struct ConnectionLowLevel {
    pub conn_ref: Option<Pin<Arc<Connection>>>,
    pub socket: TcpStream,
    noise_session: NoiseSession,
    buffers: Buffers,
    incoming_msg: IncomingMessage,
    /// A queue for bytes waiting to be written to the socket
    output_queue: VecDeque<u8>,
}

macro_rules! recv_xx_msg {
    ($self:ident, $data:expr, $idx:expr) => {
        let mut msg = vec![0u8; $data.len()? as usize];
        $data.read_exact(&mut msg)?;
        $self.noise_session.recv_message(&mut msg)?;
        trace!("I got message {}", $idx);
    };
}

macro_rules! send_xx_msg {
    ($self:ident, $size:expr, $idx:expr) => {
        let mut msg = vec![];
        // prepend the plaintext message length
        msg.write_u32::<NetworkEndian>($size as u32)?;
        // provide buffer space for the handshake message
        msg.append(&mut vec![0u8; $size]);
        // write the message into the buffer
        $self.noise_session.send_message(&mut msg[PAYLOAD_SIZE..])?;
        // queue and send the message
        trace!("Sending message {}", $idx);
        $self.output_queue.extend(msg);
        $self.flush_socket()?;
    };
}

impl ConnectionLowLevel {
    pub fn conn(&self) -> &Connection {
        &self.conn_ref.as_ref().unwrap() // safe; always available
    }

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
            noise_session: start_noise_session(is_initiator),
            buffers: Buffers::new(socket_read_size),
            incoming_msg: IncomingMessage::default(),
            output_queue: VecDeque::with_capacity(WRITE_QUEUE_ALLOC),
        }
    }

    // the XX handshake

    pub fn send_handshake_message_a(&mut self) -> Fallible<()> {
        let pad = if cfg!(feature = "snow_noise") { 0 } else { 16 };
        send_xx_msg!(self, DHLEN + pad, "A");
        Ok(())
    }

    fn process_msg_a(&mut self, mut data: HybridBuf) -> Fallible<()> {
        recv_xx_msg!(self, data, "A");
        send_xx_msg!(self, DHLEN * 2 + MAC_LENGTH * 2, "B");
        Ok(())
    }

    fn process_msg_b(&mut self, mut data: HybridBuf) -> Fallible<()> {
        recv_xx_msg!(self, data, "B");
        send_xx_msg!(self, DHLEN + MAC_LENGTH * 2, "C");
        if cfg!(feature = "snow_noise") {
            finalize_handshake(&mut self.noise_session)?;
        }
        Ok(())
    }

    fn process_msg_c(&mut self, mut data: HybridBuf) -> Fallible<()> {
        recv_xx_msg!(self, data, "C");
        if cfg!(feature = "snow_noise") {
            finalize_handshake(&mut self.noise_session)?;
        }

        // send the high-level handshake request
        self.conn().send_handshake_request()?;
        self.flush_socket()?;

        Ok(())
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
            match self.read_from_socket() {
                Ok(ReadResult::Complete(msg)) => self.conn().process_message(msg, dedup_queues)?,
                Ok(ReadResult::Incomplete) => {} // continue reading from the socket
                Ok(ReadResult::WouldBlock) => return Ok(()), // stop reading for now
                Err(e) => bail!("Can't read from the socket: {}", e),
            }
        }
    }

    /// Attempts to read a complete message from the socket.
    #[inline]
    fn read_from_socket(&mut self) -> Fallible<ReadResult> {
        // if there's any bytes to be read from the secondary buffer, process them
        // before reading from the socket again
        let mut read_bytes = if let Some(len) = self.buffers.secondary_len.take() {
            self.buffers.main[..len].copy_from_slice(&self.buffers.secondary[..len]);
            len
        } else {
            let len = self.buffers.main.len();
            match self.socket.read(&mut self.buffers.main[..len]) {
                Ok(num_bytes) => {
                    trace!(
                        "Read {} from the socket",
                        ByteSize(num_bytes as u64).to_string_as(true)
                    );
                    num_bytes
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(ReadResult::WouldBlock),
                Err(e) => return Err(e.into()),
            }
        };

        // if we don't know the length of the incoming message, read it from the
        // collected bytes; that number of bytes needs to be accounted for later
        let offset = if !self.incoming_msg.is_size_known()? {
            let curr_offset = self.incoming_msg.size_bytes.len() as usize;
            let read_size = cmp::min(read_bytes, PAYLOAD_SIZE - curr_offset);
            let written = self
                .incoming_msg
                .size_bytes
                .write(&self.buffers.main[..read_size])?;
            read_bytes -= written;
            curr_offset + written
        } else {
            0
        };

        // check if we can know the size of the message now
        if self.incoming_msg.is_size_known()? {
            let expected_size = self.incoming_msg.pending_bytes;

            // pre-allocate if we've not been reading the message yet
            if self.incoming_msg.message.is_empty()? {
                self.incoming_msg.message = HybridBuf::with_capacity(expected_size as usize)?;
            }

            let to_read = cmp::min(self.incoming_msg.pending_bytes as usize, read_bytes);
            self.incoming_msg
                .message
                .write_all(&self.buffers.main[offset..][..to_read])?;
            self.incoming_msg.pending_bytes -= to_read as PayloadSize;

            // if the socket read was greater than the number of bytes remaining to read the
            // current message, preserve those bytes in the secondary buffer
            if read_bytes > to_read {
                let len = read_bytes - to_read;
                self.buffers.secondary[..len]
                    .copy_from_slice(&self.buffers.main[offset + to_read..][..len]);
                self.buffers.secondary_len = Some(len);
            }

            if self.incoming_msg.pending_bytes == 0 {
                trace!("The message was fully read");
                self.incoming_msg.message.rewind()?;
                let msg =
                    mem::replace(&mut self.incoming_msg.message, HybridBuf::with_capacity(0)?);

                if !self.is_post_handshake() {
                    match self.noise_session.get_message_count() {
                        0 if !self.noise_session.is_initiator() => self.process_msg_a(msg),
                        1 if self.noise_session.is_initiator() => self.process_msg_b(msg),
                        2 if !self.noise_session.is_initiator() => self.process_msg_c(msg),
                        _ => bail!("invalid XX handshake"),
                    }?;
                    Ok(ReadResult::WouldBlock)
                } else {
                    Ok(ReadResult::Complete(self.decrypt(msg)?))
                }
            } else {
                Ok(ReadResult::Incomplete)
            }
        } else {
            Ok(ReadResult::Incomplete)
        }
    }

    /// Decrypt a full message read from the socket.
    #[inline]
    fn decrypt(&mut self, mut input: HybridBuf) -> Fallible<HybridBuf> {
        // calculate the number of full-sized chunks
        let len = input.len()? as usize;
        let num_full_chunks = len / NOISE_MAX_MESSAGE_LEN;
        // calculate the number of the last, incomplete chunk (if there is one)
        let last_chunk_size = len % NOISE_MAX_MESSAGE_LEN;
        let num_all_chunks = num_full_chunks + if last_chunk_size > 0 { 1 } else { 0 };

        let mut decrypted_msg =
            HybridBuf::with_capacity(NOISE_MAX_PAYLOAD_LEN * num_full_chunks + last_chunk_size)?;

        // decrypt the chunks
        for _ in 0..num_all_chunks {
            self.decrypt_chunk(&mut input, &mut decrypted_msg)?;
        }

        decrypted_msg.rewind()?;

        Ok(decrypted_msg)
    }

    /// Decrypt a single chunk of the received encrypted message.
    #[inline]
    fn decrypt_chunk<W: Write>(&mut self, input: &mut HybridBuf, output: &mut W) -> Fallible<()> {
        let read_size = cmp::min(NOISE_MAX_MESSAGE_LEN, input.remaining_len()? as usize);
        input.read_exact(&mut self.buffers.main[..read_size])?;

        if let Err(err) = self
            .noise_session
            .recv_message(&mut self.buffers.main[..read_size])
        {
            error!(
                "{} Chunk size: {}/{}B, exhausted: {}",
                err,
                read_size,
                input.len()?,
                input.remaining_len()? == 0
            );
            Err(err.into())
        } else {
            output.write_all(&self.buffers.main[..read_size - MAC_LENGTH])?;
            Ok(())
        }
    }

    // output

    /// Enqueue a message to be written to the socket.
    #[inline]
    pub fn write_to_socket(&mut self, input: Arc<[u8]>) -> Fallible<()> {
        TOTAL_MESSAGES_SENT_COUNTER.fetch_add(1, Ordering::Relaxed);
        self.conn()
            .stats
            .messages_sent
            .fetch_add(1, Ordering::Relaxed);
        if let Some(ref stats) = self.conn().handler().stats_export_service {
            stats.pkt_sent_inc();
        }

        if cfg!(feature = "network_dump") {
            self.conn().send_to_dump(input.clone(), false);
        }

        self.encrypt_and_enqueue(&input)
    }

    /// Writes enequeued messages to the socket until the queue is exhausted
    /// or the write would be blocking.
    #[inline]
    pub fn flush_socket(&mut self) -> Fallible<()> {
        while !self.output_queue.is_empty() {
            match self.flush_socket_once() {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }
        }

        Ok(())
    }

    #[inline]
    fn flush_socket_once(&mut self) -> Fallible<usize> {
        let write_size = cmp::min(self.write_size(), self.output_queue.len());

        let (front, back) = self.output_queue.as_slices();

        let front_len = cmp::min(front.len(), write_size);
        self.buffers.main[..front_len].copy_from_slice(&front[..front_len]);

        let back_len = write_size - front_len;
        if back_len > 0 {
            self.buffers.main[front_len..][..back_len].copy_from_slice(&back[..back_len]);
        }

        let written = match self.socket.write(&self.buffers.main[..write_size]) {
            Ok(num_bytes) => num_bytes,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(0),
            Err(e) => return Err(e.into()),
        };

        self.output_queue.drain(..written);

        trace!(
            "Written {} to the socket",
            ByteSize(written as u64).to_string_as(true)
        );

        Ok(written)
    }

    /// It encrypts `input` and enqueues the encrypted chunks preceded by the
    /// length for later sending.
    #[inline]
    fn encrypt_and_enqueue(&mut self, input: &[u8]) -> Fallible<()> {
        let num_full_chunks = input.len() / NOISE_MAX_PAYLOAD_LEN;
        let last_chunk_len = input.len() % NOISE_MAX_PAYLOAD_LEN + MAC_LENGTH;
        let full_msg_len = num_full_chunks * NOISE_MAX_MESSAGE_LEN + last_chunk_len;

        self.output_queue
            .extend(&(full_msg_len as PayloadSize).to_be_bytes());

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
        input.read_exact(&mut self.buffers.main[..chunk_size])?;
        let encrypted_len = chunk_size + MAC_LENGTH;

        self.noise_session
            .send_message(&mut self.buffers.main[..encrypted_len])?;

        self.output_queue
            .extend(&self.buffers.main[..encrypted_len]);

        Ok(())
    }

    #[inline]
    fn write_size(&self) -> usize { self.conn().handler().config.socket_write_size }
}
