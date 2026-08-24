#![cfg(windows)]

mod protocol;
mod windows;

pub use protocol::{
    Ack, AggregateEvent, ClientHello, Envelope, EventBatch, HandshakeComplete, Heartbeat,
    ServerHello, envelope,
};
pub use windows::{
    HandshakeReport, PeerVerification, SingleInstanceGuard, current_pipe_name, run_client_probe,
    run_server_probe,
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_BATCH_EVENTS: usize = 512;
pub const MAX_EXECUTABLE_PATH_BYTES: usize = 1024;
pub const NONCE_BYTES: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("Windows IPC error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid IPC message: {0}")]
    InvalidMessage(String),
    #[error("failed to encode IPC message: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("failed to decode IPC message: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("peer authentication failed: {0}")]
    PeerAuthentication(String),
}

pub type Result<T> = std::result::Result<T, IpcError>;
