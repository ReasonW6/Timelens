#![cfg(windows)]

pub mod privacy;
mod protocol;
mod windows;

pub use protocol::{
    Ack, ClientHello, CollectorEvent, Envelope, EventBatch, HandshakeComplete, Heartbeat,
    IdentitySource, InputMinute, MonitoringGap, MonitoringGapReason, PhysicalKeyCount, ServerHello,
    SystemInterval, TrayTransition, TrayTransitionKind, WindowObservation, WindowTransition,
    WindowTransitionKind, collector_event, envelope,
};
pub use windows::{
    HandshakeReport, PeerVerification, SingleInstanceGuard, current_pipe_name,
    new_collector_run_id, run_client_event_batch, run_client_probe, run_server_collector_message,
    run_server_probe,
};

pub const PROTOCOL_VERSION: u32 = 4;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_BATCH_EVENTS: usize = 512;
pub const MAX_EXECUTABLE_PATH_BYTES: usize = 1024;
pub const MAX_IDENTITY_BYTES: usize = 512;
pub const MAX_APP_USER_MODEL_ID_BYTES: usize = 512;
pub const MAX_PACKAGE_IDENTITY_BYTES: usize = 512;
pub const MAX_VIRTUAL_DESKTOP_ID_BYTES: usize = 128;
pub const MAX_INPUT_KEYS_PER_MINUTE: usize = 1024;
pub const NONCE_BYTES: usize = 32;
pub const COLLECTOR_RUN_ID_BYTES: usize = 16;
pub const COLLECTOR_RESET_REQUEST_FILE: &str = "collector-reset.request";
pub const COLLECTOR_RESET_PAUSED_FILE: &str = "collector-reset.paused";
pub const COLLECTOR_SPOOL_FILE: &str = "collector.spool";
pub const COLLECTOR_SPOOL_KEY_FILE: &str = "collector-spool-key.dpapi";
pub const COLLECTOR_TRAY_STATE_FILE: &str = "collector-tray-state.dpapi";

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
    #[error("authenticated event batch was rejected: {0}")]
    BatchRejected(String),
}

pub type Result<T> = std::result::Result<T, IpcError>;
