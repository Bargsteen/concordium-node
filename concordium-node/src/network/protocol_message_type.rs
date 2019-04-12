use failure::{Error, Fallible};
use std::{
    convert::TryFrom,
    fmt::{ Display, Formatter, Result }
};

pub const PROTOCOL_MESSAGE_TYPE_LENGTH: usize = 2;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ProtocolMessageType {
    RequestPing = 0,
    RequestFindNode,
    RequestHandshake,
    RequestGetPeers,
    RequestBannode,
    RequestUnbannode,
    RequestJoinNetwork,
    RequestLeaveNetwork,
    ResponsePong,
    ResponseFindNode,
    ResponsePeersList,
    ResponseHandshake,
    DirectMessage,
    BroadcastedMessage,
}

static PROTOCOL_MESSAGE_FROM_INT: &[ProtocolMessageType] = &[
    ProtocolMessageType::RequestPing,
    ProtocolMessageType::RequestFindNode,
    ProtocolMessageType::RequestHandshake,
    ProtocolMessageType::RequestGetPeers,
    ProtocolMessageType::RequestBannode,
    ProtocolMessageType::RequestUnbannode,
    ProtocolMessageType::RequestJoinNetwork,
    ProtocolMessageType::RequestLeaveNetwork,
    ProtocolMessageType::ResponsePong,
    ProtocolMessageType::ResponseFindNode,
    ProtocolMessageType::ResponsePeersList,
    ProtocolMessageType::ResponseHandshake,
    ProtocolMessageType::DirectMessage,
    ProtocolMessageType::BroadcastedMessage,
];

impl TryFrom<u8> for ProtocolMessageType {
    type Error = Error;

    #[inline]
    fn try_from(value: u8) -> Fallible<ProtocolMessageType> {
        let idx: usize = value.into();

        if idx < PROTOCOL_MESSAGE_FROM_INT.len() {
            Ok(PROTOCOL_MESSAGE_FROM_INT[idx])
        } else {
            bail!("Unsupported protocol message type")
        }
    }
}

impl TryFrom<&str> for ProtocolMessageType {
    type Error = Error;

    fn try_from(value: &str) -> Fallible<ProtocolMessageType> {
        let input = &value[..PROTOCOL_MESSAGE_TYPE_LENGTH];
        let output = u8::from_str_radix( input, 16)?;
        ProtocolMessageType::try_from( output)
    }
}

impl Display for ProtocolMessageType {
    fn fmt(&self, f: &mut Formatter) -> Result {
        write!( f, "{:02X}", *self as u8)
    }
}

#[cfg(test)]
mod test {
    use super::{ ProtocolMessageType, PROTOCOL_MESSAGE_TYPE_LENGTH};
    use std::convert::TryFrom;

    #[test]
    fn message_type_from_int() {
        assert_eq!(
            ProtocolMessageType::try_from(0).unwrap(),
            ProtocolMessageType::RequestPing
        );
        assert_eq!(
            ProtocolMessageType::try_from(4).unwrap(),
            ProtocolMessageType::RequestBannode
        );
        assert_eq!(
            ProtocolMessageType::try_from(13).unwrap(),
            ProtocolMessageType::BroadcastedMessage
        );
        assert_eq!(ProtocolMessageType::try_from(14).is_err(), true);
        assert_eq!(ProtocolMessageType::try_from(15).is_err(), true);
    }

    #[test]
    fn message_type_display() {
        let values = [
            ProtocolMessageType::RequestPing,
            ProtocolMessageType::RequestFindNode,
            ProtocolMessageType::RequestHandshake,
            ProtocolMessageType::RequestGetPeers,
            ProtocolMessageType::RequestBannode,
            ProtocolMessageType::RequestUnbannode,
            ProtocolMessageType::RequestJoinNetwork,
            ProtocolMessageType::RequestLeaveNetwork,
            ProtocolMessageType::ResponsePong,
            ProtocolMessageType::ResponseFindNode,
            ProtocolMessageType::ResponsePeersList,
            ProtocolMessageType::ResponseHandshake,
            ProtocolMessageType::DirectMessage,
            ProtocolMessageType::BroadcastedMessage,
        ];

        for value in &values {
            let value_str = format!( "{}", value);
            assert_eq!( value_str.len(), PROTOCOL_MESSAGE_TYPE_LENGTH);
            let value_from_str = ProtocolMessageType::try_from( value_str.as_str()).unwrap();
            assert_eq!( value_from_str, *value);
        }

        assert_eq!( ProtocolMessageType::try_from( "0F").is_err(), true);
        assert_eq!( ProtocolMessageType::try_from( "10").is_err(), true);
    }
}
