use std::io::{Read, Write};

use prost::{Enumeration, Message, Oneof};

use crate::{
    COLLECTOR_RUN_ID_BYTES, IpcError, MAX_APP_USER_MODEL_ID_BYTES, MAX_BATCH_EVENTS,
    MAX_EXECUTABLE_PATH_BYTES, MAX_FRAME_BYTES, MAX_IDENTITY_BYTES, MAX_PACKAGE_IDENTITY_BYTES,
    MAX_VIRTUAL_DESKTOP_ID_BYTES, NONCE_BYTES, PROTOCOL_VERSION, Result,
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
    #[prost(bytes = "vec", tag = "1")]
    pub collector_run_id: Vec<u8>,
    #[prost(uint64, tag = "2")]
    pub first_sequence: u64,
    #[prost(message, repeated, tag = "3")]
    pub events: Vec<CollectorEvent>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CollectorEvent {
    #[prost(int64, tag = "2")]
    pub observed_at_utc_ms: i64,
    #[prost(uint64, tag = "3")]
    pub monotonic_ms: u64,
    #[prost(oneof = "collector_event::Body", tags = "4")]
    pub body: Option<collector_event::Body>,
}

pub mod collector_event {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Body {
        #[prost(message, tag = "4")]
        WindowTransition(WindowTransition),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct WindowTransition {
    #[prost(enumeration = "WindowTransitionKind", tag = "1")]
    pub kind: i32,
    #[prost(message, optional, tag = "2")]
    pub window: Option<WindowObservation>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WindowObservation {
    #[prost(uint64, tag = "1")]
    pub window_id: u64,
    #[prost(uint32, tag = "2")]
    pub process_id: u32,
    #[prost(uint64, tag = "3")]
    pub process_started_at_100ns: u64,
    #[prost(string, tag = "4")]
    pub application_identity: String,
    #[prost(enumeration = "IdentitySource", tag = "5")]
    pub identity_source: i32,
    #[prost(string, optional, tag = "6")]
    pub executable_path: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub app_user_model_id: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub package_identity: Option<String>,
    #[prost(bool, tag = "9")]
    pub displayed: bool,
    #[prost(bool, tag = "10")]
    pub focused: bool,
    #[prost(bool, optional, tag = "11")]
    pub on_current_virtual_desktop: Option<bool>,
    #[prost(string, optional, tag = "12")]
    pub virtual_desktop_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Enumeration)]
#[repr(i32)]
pub enum WindowTransitionKind {
    Unspecified = 0,
    Opened = 1,
    Updated = 2,
    Closed = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Enumeration)]
#[repr(i32)]
pub enum IdentitySource {
    Unspecified = 0,
    ExecutablePath = 1,
    Package = 2,
    ProcessAppUserModelId = 3,
    WindowAppUserModelId = 4,
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
            envelope::Body::EventBatch(batch) => validate_event_batch(batch),
            envelope::Body::Heartbeat(_) | envelope::Body::Ack(_) => Ok(()),
        }
    }
}

impl EventBatch {
    pub fn last_sequence(&self) -> Result<u64> {
        let event_offset = self
            .events
            .len()
            .checked_sub(1)
            .ok_or_else(|| IpcError::InvalidMessage("event batch is empty".to_owned()))?;
        self.first_sequence
            .checked_add(event_offset as u64)
            .ok_or_else(|| IpcError::InvalidMessage("event batch sequence overflow".to_owned()))
    }
}

fn validate_event_batch(batch: &EventBatch) -> Result<()> {
    if batch.collector_run_id.len() != COLLECTOR_RUN_ID_BYTES {
        return Err(IpcError::InvalidMessage(format!(
            "collector run ID has {} bytes; expected {COLLECTOR_RUN_ID_BYTES}",
            batch.collector_run_id.len()
        )));
    }
    if batch.first_sequence == 0 {
        return Err(IpcError::InvalidMessage(
            "event batch sequence must start at one or greater".to_owned(),
        ));
    }
    if batch.events.is_empty() || batch.events.len() > MAX_BATCH_EVENTS {
        return Err(IpcError::InvalidMessage(format!(
            "event batch contains {} events; expected 1..={MAX_BATCH_EVENTS}",
            batch.events.len()
        )));
    }
    batch.last_sequence()?;

    for event in &batch.events {
        if event.observed_at_utc_ms <= 0 || event.monotonic_ms > i64::MAX as u64 {
            return Err(IpcError::InvalidMessage(
                "event timestamp is outside the supported range".to_owned(),
            ));
        }
        let transition = match event.body.as_ref() {
            Some(collector_event::Body::WindowTransition(transition)) => transition,
            None => {
                return Err(IpcError::InvalidMessage(
                    "collector event body is missing".to_owned(),
                ));
            }
        };
        let kind = WindowTransitionKind::try_from(transition.kind).map_err(|_| {
            IpcError::InvalidMessage("window transition kind is invalid".to_owned())
        })?;
        if kind == WindowTransitionKind::Unspecified {
            return Err(IpcError::InvalidMessage(
                "window transition kind is unspecified".to_owned(),
            ));
        }
        let window = transition.window.as_ref().ok_or_else(|| {
            IpcError::InvalidMessage("window transition facts are missing".to_owned())
        })?;
        validate_window(window)?;
    }
    Ok(())
}

fn validate_window(window: &WindowObservation) -> Result<()> {
    if window.window_id == 0 || window.window_id > i64::MAX as u64 {
        return Err(IpcError::InvalidMessage(
            "window ID is outside the supported range".to_owned(),
        ));
    }
    if window.process_id == 0 || window.process_started_at_100ns == 0 {
        return Err(IpcError::InvalidMessage(
            "window process identity is incomplete".to_owned(),
        ));
    }
    validate_required_string(
        &window.application_identity,
        MAX_IDENTITY_BYTES,
        "application identity",
    )?;
    validate_optional_string(
        window.executable_path.as_deref(),
        MAX_EXECUTABLE_PATH_BYTES,
        "executable path",
    )?;
    validate_optional_string(
        window.app_user_model_id.as_deref(),
        MAX_APP_USER_MODEL_ID_BYTES,
        "AppUserModelID",
    )?;
    validate_optional_string(
        window.package_identity.as_deref(),
        MAX_PACKAGE_IDENTITY_BYTES,
        "package identity",
    )?;
    validate_optional_string(
        window.virtual_desktop_id.as_deref(),
        MAX_VIRTUAL_DESKTOP_ID_BYTES,
        "virtual desktop ID",
    )?;

    let source = IdentitySource::try_from(window.identity_source).map_err(|_| {
        IpcError::InvalidMessage("application identity source is invalid".to_owned())
    })?;
    let expected_prefix = match source {
        IdentitySource::Unspecified => {
            return Err(IpcError::InvalidMessage(
                "application identity source is unspecified".to_owned(),
            ));
        }
        IdentitySource::ExecutablePath => {
            if window.executable_path.is_none() {
                return Err(IpcError::InvalidMessage(
                    "path identity is missing its executable path".to_owned(),
                ));
            }
            "path:"
        }
        IdentitySource::Package => "package:",
        IdentitySource::ProcessAppUserModelId | IdentitySource::WindowAppUserModelId => "aumid:",
    };
    if !window.application_identity.starts_with(expected_prefix) {
        return Err(IpcError::InvalidMessage(
            "application identity does not match its declared source".to_owned(),
        ));
    }
    Ok(())
}

fn validate_required_string(value: &str, maximum: usize, label: &str) -> Result<()> {
    if value.is_empty() || value.len() > maximum || value.contains('\0') {
        return Err(IpcError::InvalidMessage(format!(
            "{label} is outside the protocol limit"
        )));
    }
    Ok(())
}

fn validate_optional_string(value: Option<&str>, maximum: usize, label: &str) -> Result<()> {
    if let Some(value) = value {
        validate_required_string(value, maximum, label)?;
    }
    Ok(())
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

    fn valid_batch() -> EventBatch {
        EventBatch {
            collector_run_id: vec![7; COLLECTOR_RUN_ID_BYTES],
            first_sequence: 1,
            events: vec![CollectorEvent {
                observed_at_utc_ms: 1_700_000_000_000,
                monotonic_ms: 25,
                body: Some(collector_event::Body::WindowTransition(WindowTransition {
                    kind: WindowTransitionKind::Opened as i32,
                    window: Some(WindowObservation {
                        window_id: 100,
                        process_id: 200,
                        process_started_at_100ns: 300,
                        application_identity: "path:c:\\apps\\sample.exe".to_owned(),
                        identity_source: IdentitySource::ExecutablePath as i32,
                        executable_path: Some(r"C:\Apps\Sample.exe".to_owned()),
                        app_user_model_id: None,
                        package_identity: None,
                        displayed: true,
                        focused: true,
                        on_current_virtual_desktop: Some(true),
                        virtual_desktop_id: Some("desktop".to_owned()),
                    }),
                })),
            }],
        }
    }

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
        let mut batch = valid_batch();
        let Some(collector_event::Body::WindowTransition(transition)) =
            batch.events[0].body.as_mut()
        else {
            unreachable!()
        };
        transition.window.as_mut().unwrap().executable_path =
            Some("x".repeat(MAX_EXECUTABLE_PATH_BYTES + 1));
        let message = Envelope::new(envelope::Body::EventBatch(batch));

        let error = write_frame(&mut Vec::new(), &message).unwrap_err();
        assert!(error.to_string().contains("executable path"));
    }

    #[test]
    fn round_trips_a_valid_window_batch() {
        let message = Envelope::new(envelope::Body::EventBatch(valid_batch()));
        let mut bytes = Vec::new();

        write_frame(&mut bytes, &message).unwrap();
        let decoded = read_frame(&mut bytes.as_slice()).unwrap();

        assert_eq!(decoded, message);
    }

    #[test]
    fn rejects_invalid_run_ids_and_identity_sources() {
        let mut batch = valid_batch();
        batch.collector_run_id.pop();
        let error = write_frame(
            &mut Vec::new(),
            &Envelope::new(envelope::Body::EventBatch(batch)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("run ID"));

        let mut batch = valid_batch();
        let Some(collector_event::Body::WindowTransition(transition)) =
            batch.events[0].body.as_mut()
        else {
            unreachable!()
        };
        transition.window.as_mut().unwrap().application_identity = "aumid:wrong".to_owned();
        let error = write_frame(
            &mut Vec::new(),
            &Envelope::new(envelope::Body::EventBatch(batch)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("declared source"));
    }

    #[test]
    fn rejects_a_frame_length_before_allocating_it() {
        let mut bytes = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0_u8; 8]);

        let error = read_frame(&mut bytes.as_slice()).unwrap_err();
        assert!(error.to_string().contains("frame length"));
    }
}
