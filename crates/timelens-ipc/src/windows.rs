use std::{
    ffi::{OsStr, c_void},
    io::{self, Read, Write},
    mem::size_of,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE,
        GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom},
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FlushFileBuffers, OPEN_EXISTING,
        PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
    },
    System::{
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId,
            GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_TYPE_BYTE, PIPE_WAIT, WaitNamedPipeW,
        },
        RemoteDesktop::ProcessIdToSessionId,
        Threading::{
            CreateMutexW, GetCurrentProcessId, OpenProcess, OpenProcessToken,
            PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        },
    },
};

use crate::{
    Ack, COLLECTOR_RUN_ID_BYTES, ClientHello, Envelope, EventBatch, HandshakeComplete, Heartbeat,
    IpcError, MAX_FRAME_BYTES, NONCE_BYTES, Result, ServerHello, envelope, protocol,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerVerification {
    ProtectedSiblingPath,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandshakeReport {
    pub peer_process_id: u32,
    pub peer_session_id: u32,
    pub peer_path: PathBuf,
    pub verification: PeerVerification,
}

pub struct SingleInstanceGuard {
    handle: OwnedHandle,
}

impl SingleInstanceGuard {
    pub fn acquire_core() -> Result<Self> {
        Self::acquire("Core")
    }

    pub fn acquire_collector() -> Result<Self> {
        Self::acquire("Collector")
    }

    fn acquire(component: &str) -> Result<Self> {
        let sid = current_user_sid()?;
        let session = current_session_id()?;
        let name = wide(format!("Local\\Timelens.{component}.{sid}.{session}.v1"));
        let handle = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
        let handle = OwnedHandle::new(handle)?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return Err(IpcError::PeerAuthentication(format!(
                "another Timelens {} already owns this user session",
                component.to_ascii_lowercase()
            )));
        }
        Ok(Self { handle })
    }

    pub fn raw_handle(&self) -> HANDLE {
        self.handle.0
    }
}

pub fn current_pipe_name() -> Result<String> {
    let sid = current_user_sid()?;
    let session = current_session_id()?;
    Ok(format!(r"\\.\pipe\Timelens.{sid}.{session}.collector.v4"))
}

pub fn run_server_probe(pipe_name: &str, expected_peer_names: &[&str]) -> Result<HandshakeReport> {
    let (mut pipe, report) = accept_authenticated_server(pipe_name, expected_peer_names)?;
    let heartbeat = protocol::read_frame(&mut pipe)?;
    let sequence = match heartbeat.body {
        Some(envelope::Body::Heartbeat(Heartbeat { sequence, .. })) => sequence,
        _ => {
            return Err(IpcError::InvalidMessage(
                "the first post-handshake message must be Heartbeat".to_owned(),
            ));
        }
    };
    protocol::write_frame(
        &mut pipe,
        &Envelope::new(envelope::Body::Ack(Ack {
            through_sequence: sequence,
            accepted: true,
        })),
    )?;
    Ok(report)
}

pub fn run_client_probe(pipe_name: &str, expected_peer_names: &[&str]) -> Result<HandshakeReport> {
    let (mut pipe, report) = connect_authenticated_client(pipe_name, expected_peer_names)?;
    protocol::write_frame(
        &mut pipe,
        &Envelope::new(envelope::Body::Heartbeat(Heartbeat {
            sequence: 1,
            sent_at_unix_ms: unix_time_ms(),
        })),
    )?;
    let ack = protocol::read_frame(&mut pipe)?;
    match ack.body {
        Some(envelope::Body::Ack(Ack {
            through_sequence: 1,
            accepted: true,
        })) => Ok(report),
        _ => Err(IpcError::InvalidMessage(
            "server did not acknowledge the heartbeat".to_owned(),
        )),
    }
}

pub fn run_server_collector_message<F, E>(
    pipe_name: &str,
    expected_peer_names: &[&str],
    persist: F,
) -> Result<HandshakeReport>
where
    F: FnOnce(&EventBatch) -> std::result::Result<(), E>,
    E: std::fmt::Display,
{
    let (mut pipe, report) = accept_authenticated_server(pipe_name, expected_peer_names)?;
    let message = protocol::read_frame(&mut pipe)?;
    let batch = match message.body {
        Some(envelope::Body::Heartbeat(Heartbeat { sequence, .. })) => {
            protocol::write_frame(
                &mut pipe,
                &Envelope::new(envelope::Body::Ack(Ack {
                    through_sequence: sequence,
                    accepted: true,
                })),
            )?;
            return Ok(report);
        }
        Some(envelope::Body::EventBatch(batch)) => batch,
        _ => {
            return Err(IpcError::InvalidMessage(
                "the first post-handshake message must be Heartbeat or EventBatch".to_owned(),
            ));
        }
    };
    let through_sequence = batch.last_sequence()?;
    if let Err(error) = persist(&batch) {
        protocol::write_frame(
            &mut pipe,
            &Envelope::new(envelope::Body::Ack(Ack {
                through_sequence,
                accepted: false,
            })),
        )?;
        return Err(IpcError::BatchRejected(error.to_string()));
    }
    protocol::write_frame(
        &mut pipe,
        &Envelope::new(envelope::Body::Ack(Ack {
            through_sequence,
            accepted: true,
        })),
    )?;
    Ok(report)
}

pub fn run_client_event_batch(
    pipe_name: &str,
    expected_peer_names: &[&str],
    batch: &EventBatch,
) -> Result<HandshakeReport> {
    let (mut pipe, report) = connect_authenticated_client(pipe_name, expected_peer_names)?;
    let through_sequence = batch.last_sequence()?;
    protocol::write_frame(
        &mut pipe,
        &Envelope::new(envelope::Body::EventBatch(batch.clone())),
    )?;
    let ack = protocol::read_frame(&mut pipe)?;
    match ack.body {
        Some(envelope::Body::Ack(Ack {
            through_sequence: acknowledged,
            accepted: true,
        })) if acknowledged == through_sequence => Ok(report),
        Some(envelope::Body::Ack(Ack {
            through_sequence: acknowledged,
            accepted: false,
        })) if acknowledged == through_sequence => Err(IpcError::BatchRejected(
            "the Timelens core did not persist the batch".to_owned(),
        )),
        _ => Err(IpcError::InvalidMessage(
            "server did not acknowledge the event batch sequence".to_owned(),
        )),
    }
}

pub fn new_collector_run_id() -> Result<Vec<u8>> {
    random_bytes(COLLECTOR_RUN_ID_BYTES)
}

fn accept_authenticated_server(
    pipe_name: &str,
    expected_peer_names: &[&str],
) -> Result<(OwnedHandle, HandshakeReport)> {
    validate_pipe_name(pipe_name)?;
    let mut pipe = create_server_pipe(pipe_name)?;
    connect_server(&pipe)?;

    let peer_process_id = named_pipe_client_process_id(&pipe)?;
    let hello = protocol::read_frame(&mut pipe)?;
    let client = match hello.body {
        Some(envelope::Body::ClientHello(client)) => client,
        _ => {
            return Err(IpcError::InvalidMessage(
                "the first client message must be ClientHello".to_owned(),
            ));
        }
    };
    if client.process_id != peer_process_id {
        return Err(IpcError::PeerAuthentication(format!(
            "client claimed process {}, but Windows reports {}",
            client.process_id, peer_process_id
        )));
    }

    let report = verify_peer(peer_process_id, client.session_id, expected_peer_names)?;
    let server_nonce = random_nonce()?;
    protocol::write_frame(
        &mut pipe,
        &Envelope::new(envelope::Body::ServerHello(ServerHello {
            process_id: unsafe { GetCurrentProcessId() },
            session_id: current_session_id()?,
            client_nonce: client.nonce,
            server_nonce: server_nonce.clone(),
        })),
    )?;
    let completed = protocol::read_frame(&mut pipe)?;
    match completed.body {
        Some(envelope::Body::HandshakeComplete(HandshakeComplete {
            server_nonce: echoed_nonce,
        })) if echoed_nonce == server_nonce => Ok((pipe, report)),
        _ => Err(IpcError::PeerAuthentication(
            "client did not return the server nonce".to_owned(),
        )),
    }
}

fn connect_authenticated_client(
    pipe_name: &str,
    expected_peer_names: &[&str],
) -> Result<(OwnedHandle, HandshakeReport)> {
    validate_pipe_name(pipe_name)?;
    let mut pipe = connect_client(pipe_name)?;
    let peer_process_id = named_pipe_server_process_id(&pipe)?;
    let nonce = random_nonce()?;
    protocol::write_frame(
        &mut pipe,
        &Envelope::new(envelope::Body::ClientHello(ClientHello {
            process_id: unsafe { GetCurrentProcessId() },
            session_id: current_session_id()?,
            nonce: nonce.clone(),
        })),
    )?;
    let server = protocol::read_frame(&mut pipe)?;
    let server = match server.body {
        Some(envelope::Body::ServerHello(server)) => server,
        _ => {
            return Err(IpcError::InvalidMessage(
                "the first server message must be ServerHello".to_owned(),
            ));
        }
    };
    if server.process_id != peer_process_id {
        return Err(IpcError::PeerAuthentication(format!(
            "server claimed process {}, but Windows reports {}",
            server.process_id, peer_process_id
        )));
    }
    if server.client_nonce != nonce {
        return Err(IpcError::PeerAuthentication(
            "server did not return the client nonce".to_owned(),
        ));
    }
    let report = verify_peer(peer_process_id, server.session_id, expected_peer_names)?;
    protocol::write_frame(
        &mut pipe,
        &Envelope::new(envelope::Body::HandshakeComplete(HandshakeComplete {
            server_nonce: server.server_nonce,
        })),
    )?;
    Ok((pipe, report))
}

fn create_server_pipe(pipe_name: &str) -> Result<OwnedHandle> {
    let sid = current_user_sid()?;
    let sddl = wide(format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})"));
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(last_os_error().into());
    }
    let descriptor = LocalAllocation(descriptor.cast());
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let name = wide(pipe_name);
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            (MAX_FRAME_BYTES + 4) as u32,
            (MAX_FRAME_BYTES + 4) as u32,
            1_000,
            &attributes,
        )
    };
    OwnedHandle::new(handle).map_err(Into::into)
}

fn connect_server(pipe: &OwnedHandle) -> Result<()> {
    if unsafe { ConnectNamedPipe(pipe.0, null_mut()) } != 0 {
        return Ok(());
    }
    let error = unsafe { GetLastError() };
    if error == ERROR_PIPE_CONNECTED {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(error as i32).into())
    }
}

fn connect_client(pipe_name: &str) -> Result<OwnedHandle> {
    let name = wide(pipe_name);
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return OwnedHandle::new(handle).map_err(Into::into);
        }
        if Instant::now() >= deadline {
            return Err(last_os_error().into());
        }
        unsafe {
            WaitNamedPipeW(name.as_ptr(), 250);
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn verify_peer(
    process_id: u32,
    claimed_session_id: u32,
    expected_names: &[&str],
) -> Result<HandshakeReport> {
    let actual_session_id = process_session_id(process_id)?;
    if claimed_session_id != actual_session_id || actual_session_id != current_session_id()? {
        return Err(IpcError::PeerAuthentication(format!(
            "peer session {actual_session_id} does not match the current session"
        )));
    }
    let peer_sid = process_user_sid(process_id)?;
    if peer_sid != current_user_sid()? {
        return Err(IpcError::PeerAuthentication(
            "peer Windows user SID does not match the current user".to_owned(),
        ));
    }

    let peer_path = process_path(process_id)?;
    let peer_name = peer_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            IpcError::PeerAuthentication("peer executable has no file name".to_owned())
        })?;
    if !expected_names
        .iter()
        .any(|expected| peer_name.eq_ignore_ascii_case(expected))
    {
        return Err(IpcError::PeerAuthentication(format!(
            "unexpected peer executable name {peer_name}"
        )));
    }

    let own_parent = std::env::current_exe()?
        .canonicalize()?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            IpcError::PeerAuthentication("current executable has no parent".to_owned())
        })?;
    let peer_path = peer_path.canonicalize()?;
    let peer_parent = peer_path
        .parent()
        .ok_or_else(|| IpcError::PeerAuthentication("peer executable has no parent".to_owned()))?;
    if !paths_equal_case_insensitive(&own_parent, peer_parent) {
        return Err(IpcError::PeerAuthentication(
            "peer executable is not in the Timelens installation directory".to_owned(),
        ));
    }

    Ok(HandshakeReport {
        peer_process_id: process_id,
        peer_session_id: actual_session_id,
        peer_path,
        verification: PeerVerification::ProtectedSiblingPath,
    })
}

fn process_path(process_id: u32) -> Result<PathBuf> {
    let process = open_process(process_id)?;
    let mut buffer = vec![0_u16; 32_768];
    let mut length = buffer.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process.0, 0, buffer.as_mut_ptr(), &mut length) } == 0 {
        return Err(last_os_error().into());
    }
    buffer.truncate(length as usize);
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer)))
}

fn process_user_sid(process_id: u32) -> Result<String> {
    let process = open_process(process_id)?;
    let mut token = null_mut();
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_os_error().into());
    }
    let token = OwnedHandle::new(token)?;
    let mut required = 0_u32;
    unsafe {
        GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(last_os_error().into());
    }
    let word_bytes = size_of::<usize>();
    let mut buffer = vec![0_usize; (required as usize).div_ceil(word_bytes)];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast::<c_void>(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(last_os_error().into());
    }
    let token_user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
    sid_to_string(token_user.User.Sid)
}

fn current_user_sid() -> Result<String> {
    process_user_sid(unsafe { GetCurrentProcessId() })
}

fn current_session_id() -> Result<u32> {
    process_session_id(unsafe { GetCurrentProcessId() })
}

fn process_session_id(process_id: u32) -> Result<u32> {
    let mut session_id = 0_u32;
    if unsafe { ProcessIdToSessionId(process_id, &mut session_id) } == 0 {
        return Err(last_os_error().into());
    }
    Ok(session_id)
}

fn sid_to_string(sid: *mut c_void) -> Result<String> {
    let mut value = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut value) } == 0 {
        return Err(last_os_error().into());
    }
    let value = LocalAllocation(value.cast());
    let mut length = 0_usize;
    let pointer = value.0.cast::<u16>();
    while unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(pointer, length) };
    Ok(String::from_utf16_lossy(slice))
}

fn named_pipe_client_process_id(pipe: &OwnedHandle) -> Result<u32> {
    let mut process_id = 0_u32;
    if unsafe { GetNamedPipeClientProcessId(pipe.0, &mut process_id) } == 0 {
        return Err(last_os_error().into());
    }
    Ok(process_id)
}

fn named_pipe_server_process_id(pipe: &OwnedHandle) -> Result<u32> {
    let mut process_id = 0_u32;
    if unsafe { GetNamedPipeServerProcessId(pipe.0, &mut process_id) } == 0 {
        return Err(last_os_error().into());
    }
    Ok(process_id)
}

fn open_process(process_id: u32) -> Result<OwnedHandle> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    OwnedHandle::new(handle).map_err(Into::into)
}

fn random_nonce() -> Result<Vec<u8>> {
    random_bytes(NONCE_BYTES)
}

fn random_bytes(length: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0_u8; length];
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(IpcError::Io(io::Error::from_raw_os_error(status)));
    }
    Ok(bytes)
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn validate_pipe_name(pipe_name: &str) -> Result<()> {
    if !pipe_name.starts_with(r"\\.\pipe\Timelens.") || pipe_name.len() > 256 {
        return Err(IpcError::InvalidMessage(
            "pipe name must be a local Timelens pipe with at most 256 characters".to_owned(),
        ));
    }
    Ok(())
}

fn paths_equal_case_insensitive(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn last_os_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> io::Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(last_os_error())
        } else {
            Ok(Self(handle))
        }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

impl Read for OwnedHandle {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let length = buffer.len().min(u32::MAX as usize) as u32;
        let mut read = 0_u32;
        if unsafe { ReadFile(self.0, buffer.as_mut_ptr(), length, &mut read, null_mut()) } == 0 {
            return Err(last_os_error());
        }
        Ok(read as usize)
    }
}

impl Write for OwnedHandle {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let length = buffer.len().min(u32::MAX as usize) as u32;
        let mut written = 0_u32;
        if unsafe { WriteFile(self.0, buffer.as_ptr(), length, &mut written, null_mut()) } == 0 {
            return Err(last_os_error());
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        if unsafe { FlushFileBuffers(self.0) } == 0 {
            Err(last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::{
        CollectorEvent, IdentitySource, WindowObservation, WindowTransition, WindowTransitionKind,
        collector_event,
    };

    static PIPE_COUNTER: AtomicU32 = AtomicU32::new(1);

    fn current_executable_name() -> String {
        std::env::current_exe()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    fn test_pipe_name() -> String {
        format!(
            r"\\.\pipe\Timelens.test.{}.{}",
            unsafe { GetCurrentProcessId() },
            PIPE_COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn test_batch() -> EventBatch {
        EventBatch {
            collector_run_id: vec![9; COLLECTOR_RUN_ID_BYTES],
            first_sequence: 1,
            events: vec![CollectorEvent {
                observed_at_utc_ms: 1_700_000_000_000,
                monotonic_ms: 10,
                body: Some(collector_event::Body::WindowTransition(WindowTransition {
                    kind: WindowTransitionKind::Opened as i32,
                    window: Some(WindowObservation {
                        window_id: 1,
                        process_id: unsafe { GetCurrentProcessId() },
                        process_started_at_100ns: 2,
                        application_identity: "path:c:\\timelens-test.exe".to_owned(),
                        identity_source: IdentitySource::ExecutablePath as i32,
                        executable_path: Some(r"C:\timelens-test.exe".to_owned()),
                        app_user_model_id: None,
                        package_identity: None,
                        displayed: true,
                        focused: false,
                        on_current_virtual_desktop: Some(true),
                        virtual_desktop_id: Some("desktop".to_owned()),
                    }),
                })),
            }],
        }
    }

    #[test]
    fn named_pipe_probe_authenticates_and_acknowledges() {
        let current_name = current_executable_name();
        let pipe_name = test_pipe_name();
        let server_pipe = pipe_name.clone();
        let server_name = current_name.clone();
        let server = thread::spawn(move || run_server_probe(&server_pipe, &[server_name.as_str()]));

        let client = run_client_probe(&pipe_name, &[current_name.as_str()]).unwrap();
        let server = server.join().unwrap().unwrap();

        assert_eq!(client.peer_process_id, unsafe { GetCurrentProcessId() });
        assert_eq!(server.peer_process_id, unsafe { GetCurrentProcessId() });
        assert_eq!(client.verification, PeerVerification::ProtectedSiblingPath);
    }

    #[test]
    fn collector_server_accepts_an_idle_heartbeat() {
        let current_name = current_executable_name();
        let pipe_name = test_pipe_name();
        let server_pipe = pipe_name.clone();
        let server_name = current_name.clone();
        let server = thread::spawn(move || {
            run_server_collector_message(
                &server_pipe,
                &[server_name.as_str()],
                |_| -> std::result::Result<(), &'static str> {
                    panic!("a heartbeat must not invoke event persistence")
                },
            )
        });

        run_client_probe(&pipe_name, &[current_name.as_str()]).unwrap();
        server.join().unwrap().unwrap();
    }

    #[test]
    fn authenticated_event_batch_is_acknowledged_after_persistence() {
        let current_name = current_executable_name();
        let pipe_name = test_pipe_name();
        let server_pipe = pipe_name.clone();
        let server_name = current_name.clone();
        let server = thread::spawn(move || {
            run_server_collector_message(&server_pipe, &[server_name.as_str()], |batch| {
                assert_eq!(batch.first_sequence, 1);
                Ok::<_, &'static str>(())
            })
        });

        let client =
            run_client_event_batch(&pipe_name, &[current_name.as_str()], &test_batch()).unwrap();
        let server = server.join().unwrap().unwrap();

        assert_eq!(client.peer_process_id, unsafe { GetCurrentProcessId() });
        assert_eq!(server.peer_process_id, unsafe { GetCurrentProcessId() });
    }

    #[test]
    fn rejected_event_batch_is_not_acknowledged_as_accepted() {
        let current_name = current_executable_name();
        let pipe_name = test_pipe_name();
        let server_pipe = pipe_name.clone();
        let server_name = current_name.clone();
        let server = thread::spawn(move || {
            run_server_collector_message(&server_pipe, &[server_name.as_str()], |_| {
                Err::<(), _>("storage unavailable")
            })
        });

        let client_error =
            run_client_event_batch(&pipe_name, &[current_name.as_str()], &test_batch())
                .unwrap_err();
        let server_error = server.join().unwrap().unwrap_err();

        assert!(client_error.to_string().contains("did not persist"));
        assert!(server_error.to_string().contains("storage unavailable"));
    }

    #[test]
    fn collector_run_ids_use_the_fixed_random_length() {
        let first = new_collector_run_id().unwrap();
        let second = new_collector_run_id().unwrap();
        assert_eq!(first.len(), COLLECTOR_RUN_ID_BYTES);
        assert_ne!(first, second);
    }

    #[test]
    fn refuses_remote_or_arbitrary_pipe_names() {
        let error = run_client_probe(r"\\server\pipe\Timelens.bad", &["anything.exe"]).unwrap_err();
        assert!(error.to_string().contains("local Timelens pipe"));
    }
}
