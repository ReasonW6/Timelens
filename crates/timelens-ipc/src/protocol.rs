use std::io::{Read, Write};

use prost::{Message, Oneof};

use crate::{
    IpcError, MAX_BATCH_EVENTS, MAX_EXECUTABLE_PATH_BYTES, MAX_FRAME_BYTES, NONCE_BYTES,
    PROTOCOL_VERSION, Result,
};

#[derive(Clone, PartialEq, Message)]
pub struct Envelope {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(oneof = "envelope::Body", tags = "2, 3, 4, 5, 6, 7")]
    pub body: Option<envelope::Body>,
}

pub mod envelope {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Body {
        #[prost(message, tag = "2")]
        ClientHello(ClientHello),
        #[prost(message, tag = "3")]
        ServerHello(ServerHello),
        #[prost(message, tag = "4")]
        HandshakeComplete(HandshakeComplete),
        #[prost(message, tag = "5")]
        Heartbeat(Heartbeat),
        #[prost(message, tag = "6")]
        EventBatch(EventBatch),
        #[prost(message, tag = "7")]
        Ack(Ack),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientHello {
    #[prost(uint32, tag = "1")]
    pub process_id: u32,
    #[prost(uint32, tag = "2")]
    pub session_id: u32,
    #[prost(bytes = "vec", tag = "3")]
    pub nonce: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ServerHello {
    #[prost(uint32, tag = "1")]
    pub process_id: u32,
    #[prost(uint32, tag = "2")]
    pub session_id: u32,
    #[prost(bytes = "vec", tag = "3")]
    pub client_nonce: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub server_nonce: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct HandshakeComplete {
    #[prost(bytes = "vec", tag = "1")]
    pub server_nonce: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Heartbeat {
    #[prost(uint64, tag = "1")]
    pub sequence: u64,
    #[prost(int64, tag = "2")]
    pub sent_at_unix_ms: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct EventBatch {
    #[prost(uint64, tag = "1")]
    pub first_sequence: u64,
    #[prost(message, repeated, tag = "2")]
    pub events: Vec<AggregateEvent>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AggregateEvent {
    #[prost(uint32, tag = "1")]
    pub kind: u32,
    #[prost(int64, tag = "2")]
    pub observed_at_unix_ms: i64,
    #[prost(uint64, tag = "3")]
    pub count: u64,
    #[prost(string, optional, tag = "4")]
    pub executable_path: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Ack {
    #[prost(uint64, tag = "1")]
    pub through_sequence: u64,
    #[prost(bool, tag = "2")]
    pub accepted: bool,
}

impl Envelope {
    pub fn new(body: envelope::Body) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            body: Some(body),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(IpcError::InvalidMessage(format!(
                "protocol version {} is unsupported; expected {}",
                self.protocol_version, PROTOCOL_VERSION
            )));
        }

        let body = self
            .body
            .as_ref()
            .ok_or_else(|| IpcError::InvalidMessage("message body is missing".to_owned()))?;
        match body {
            envelope::Body::ClientHello(message) => validate_nonce(&message.nonce, "client"),
            envelope::Body::ServerHello(message) => {
                validate_nonce(&message.client_nonce, "echoed client")?;
                validate_nonce(&message.server_nonce, "server")
            }
            envelope::Body::HandshakeComplete(message) => {
                validate_nonce(&message.server_nonce, "echoed server")
            }
            envelope::Body::EventBatch(batch) => {
                if batch.events.len() > MAX_BATCH_EVENTS {
                    return Err(IpcError::InvalidMessage(format!(
                        "event batch contains {} events; maximum is {}",
                        batch.events.len(),
                        MAX_BATCH_EVENTS
                    )));
                }
                for event in &batch.events {
                    if event
                        .executable_path
                        .as_ref()
                        .is_some_and(|path| path.len() > MAX_EXECUTABLE_PATH_BYTES)
                    {
                        return Err(IpcError::InvalidMessage(
                            "executable path exceeds the protocol limit".to_owned(),
                        ));
                    }
                }
                Ok(())
            }
            envelope::Body::Heartbeat(_) | envelope::Body::Ack(_) => Ok(()),
        }
    }
}

fn validate_nonce(nonce: &[u8], label: &str) -> Result<()> {
    if nonce.len() != NONCE_BYTES {
        return Err(IpcError::InvalidMessage(format!(
            "{label} nonce has {} bytes; expected {NONCE_BYTES}",
            nonce.len()
        )));
    }
    Ok(())
}

pub(crate) fn write_frame(writer: &mut impl Write, message: &Envelope) -> Result<()> {
    message.validate()?;
    let encoded_len = message.encoded_len();
    if encoded_len == 0 || encoded_len > MAX_FRAME_BYTES {
        return Err(IpcError::InvalidMessage(format!(
            "encoded frame length {encoded_len} is outside 1..={MAX_FRAME_BYTES}"
        )));
    }

    let mut buffer = Vec::with_capacity(encoded_len);
    message.encode(&mut buffer)?;
    writer.write_all(&(buffer.len() as u32).to_le_bytes())?;
    writer.write_all(&buffer)?;
    writer.flush()?;
    Ok(())
}

pub(crate) fn read_frame(reader: &mut impl Read) -> Result<Envelope> {
    let mut length_bytes = [0_u8; 4];
    reader.read_exact(&mut length_bytes)?;
    let length = u32::from_le_bytes(length_bytes) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(IpcError::InvalidMessage(format!(
            "frame length {length} is outside 1..={MAX_FRAME_BYTES}"
        )));
    }

    let mut buffer = vec![0_u8; length];
    reader.read_exact(&mut buffer)?;
    let message = Envelope::decode(buffer.as_slice())?;
    message.validate()?;
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_valid_heartbeat() {
        let message = Envelope::new(envelope::Body::Heartbeat(Heartbeat {
            sequence: 7,
            sent_at_unix_ms: 1234,
        }));
        let mut bytes = Vec::new();

        write_frame(&mut bytes, &message).unwrap();
        let decoded = read_frame(&mut bytes.as_slice()).unwrap();

        assert_eq!(decoded, message);
    }

    #[test]
    fn rejects_an_unsupported_protocol_version() {
        let message = Envelope {
            protocol_version: PROTOCOL_VERSION + 1,
            body: Some(envelope::Body::Heartbeat(Heartbeat {
                sequence: 1,
                sent_at_unix_ms: 0,
            })),
        };

        let error = write_frame(&mut Vec::new(), &message).unwrap_err();
        assert!(error.to_string().contains("unsupported"));
    }

    #[test]
    fn rejects_an_oversized_executable_path() {
        let message = Envelope::new(envelope::Body::EventBatch(EventBatch {
            first_sequence: 1,
            events: vec![AggregateEvent {
                kind: 1,
                observed_at_unix_ms: 0,
                count: 1,
                executable_path: Some("x".repeat(MAX_EXECUTABLE_PATH_BYTES + 1)),
            }],
        }));

        let error = write_frame(&mut Vec::new(), &message).unwrap_err();
        assert!(error.to_string().contains("executable path"));
    }

    #[test]
    fn rejects_a_frame_length_before_allocating_it() {
        let mut bytes = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0_u8; 8]);

        let error = read_frame(&mut bytes.as_slice()).unwrap_err();
        assert!(error.to_string().contains("frame length"));
    }
}
