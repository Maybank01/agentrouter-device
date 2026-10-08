//! Local IPC between `agentrouter-device mcp` (started by an AI client on this computer) and the running
//! AgentRouter app (LINKED-DEVICES.md §14.3, DEVICE-PROTOCOL.md §15). Current user only, never the
//! network:
//!
//! - Windows: a named pipe `\\.\pipe\agentrouter-device-<user SID>` whose security descriptor admits
//!   only this user's SID, with `PIPE_REJECT_REMOTE_CLIENTS`; the client checks that the pipe belongs
//!   to the process named in the token file (no squatting) and connects at identification level.
//! - macOS / Linux: a Unix socket (0600) in a directory only this user may enter (0700); every peer's
//!   uid is checked.
//! - A token file (`mcp.token`, readable only by this user) holds a 32-byte random token the app makes
//!   at every start; the first line on a connection must present it.
//!
//! Frames are one JSON value per line. No TCP port is opened, so nothing on the LAN can reach it.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::util::{data_dir, hex, log, random_bytes};

/// The longest line accepted on the connection (tool calls carry files up to about 8 MiB in base64).
const LINE_MAX: u64 = 16 << 20;

/// The AI client on the other end, as its MCP `initialize` reported it (said to be, not proven).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default)]
    pub version: String,
}

/// One accepted connection.
#[derive(Debug, Clone)]
pub struct Peer {
    pub id: u64,
    pub client: ClientInfo,
    /// The connecting process (Windows), for the record.
    pub pid: Option<u32>,
}

/// What the running app does with the connections.
pub trait Handler: Send + Sync + 'static {
    fn opened(&self, peer: &Peer);
    /// One JSON-RPC message; the answer (if any) goes back on the same connection.
    fn message(&self, peer: &Peer, message: Value, cancel: &AtomicBool) -> Option<Value>;
    fn closed(&self, peer: &Peer);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenFile {
    token: String,
    pipe: String,
    pid: u32,
}

pub fn token_path() -> PathBuf {
    data_dir().join("mcp.token")
}

fn read_line_limited(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut line = String::new();
    let n = reader.by_ref().take(LINE_MAX).read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') && n as u64 >= LINE_MAX {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
    }
    Ok(Some(line))
}

fn same(a: &str, b: &str) -> bool {
    // Constant time for equal lengths (the token is always 64 hex characters).
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// The running app's side.
pub struct Server {
    endpoint: String,
}

impl Server {
    /// Open the endpoint, write a fresh token file, and serve connections on background threads.
    pub fn start(handler: Arc<dyn Handler>) -> io::Result<Server> {
        let endpoint = platform::endpoint()?;
        let mut listener = platform::Listener::bind(&endpoint)?;
        let token = hex(&random_bytes::<32>());
        let file = TokenFile {
            token: token.clone(),
            pipe: endpoint.clone(),
            pid: std::process::id(),
        };
        platform::write_private(
            &token_path(),
            serde_json::to_string(&file).unwrap_or_default().as_bytes(),
        )?;
        let next = Arc::new(AtomicU64::new(1));
        std::thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((stream, pid)) => {
                        let (handler, token, next) = (handler.clone(), token.clone(), next.clone());
                        std::thread::spawn(move || {
                            let id = next.fetch_add(1, Ordering::SeqCst);
                            if let Err(e) = serve_one(stream, pid, id, &token, handler.as_ref()) {
                                log(&format!("local MCP connection {id} ended: {e}"));
                            }
                        });
                    }
                    Err(e) => {
                        log(&format!("local MCP: accept failed: {e}"));
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                }
            }
        });
        Ok(Server { endpoint })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

fn serve_one(
    stream: platform::Stream,
    pid: Option<u32>,
    id: u64,
    token: &str,
    handler: &dyn Handler,
) -> io::Result<()> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let Some(first) = read_line_limited(&mut reader)? else {
        return Ok(());
    };
    let hello: Value = serde_json::from_str(&first).unwrap_or(Value::Null);
    let presented = hello.get("token").and_then(Value::as_str).unwrap_or("");
    if hello.get("op").and_then(Value::as_str) != Some("hello") || !same(presented, token) {
        let _ = writeln!(writer, "{}", json!({"op": "error", "code": "TOKEN"}));
        return Ok(());
    }
    let client: ClientInfo = hello
        .get("client")
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default();
    // Registered before the client hears "welcome", so it is listed as soon as it is connected.
    let peer = Peer { id, client, pid };
    handler.opened(&peer);
    if writeln!(writer, "{}", json!({"op": "welcome"}))
        .and_then(|_| writer.flush())
        .is_err()
    {
        handler.closed(&peer);
        return Ok(());
    }
    let result = (|| -> io::Result<()> {
        while let Some(line) = read_line_limited(&mut reader)? {
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(message) => handler.message(&peer, message, &AtomicBool::new(false)),
                Err(_) => Some(
                    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}}),
                ),
            };
            if let Some(reply) = reply {
                writeln!(writer, "{reply}")?;
                writer.flush()?;
            }
        }
        Ok(())
    })();
    handler.closed(&peer);
    result
}

/// The `mcp` front end's side: one connection to the running app.
pub struct Client {
    reader: BufReader<platform::Stream>,
    writer: platform::Stream,
}

impl Client {
    /// Connect to the running app; fails when it is not running (no token file, nobody listening).
    pub fn connect(client: &ClientInfo) -> io::Result<Client> {
        let text = std::fs::read_to_string(token_path())?;
        let file: TokenFile = serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let stream = platform::connect(&file.pipe, file.pid)?;
        let mut writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);
        writeln!(
            writer,
            "{}",
            json!({"op": "hello", "token": file.token, "client": client})
        )?;
        writer.flush()?;
        let answer = read_line_limited(&mut reader)?.unwrap_or_default();
        let answer: Value = serde_json::from_str(&answer).unwrap_or(Value::Null);
        if answer.get("op").and_then(Value::as_str) != Some("welcome") {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the app refused the connection",
            ));
        }
        Ok(Client { reader, writer })
    }

    /// Send one JSON-RPC message and read its answer.
    pub fn request(&mut self, message: &Value) -> io::Result<Value> {
        writeln!(self.writer, "{message}")?;
        self.writer.flush()?;
        let line = read_line_limited(&mut self.reader)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "the app closed"))?;
        serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};

    pub type Stream = UnixStream;

    pub fn endpoint() -> io::Result<String> {
        let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(rt) if std::env::var_os("AGENTROUTER_DEVICE_HOME").is_none() => {
                PathBuf::from(rt).join("agentrouter-device")
            }
            _ => crate::util::data_dir().join("run"),
        };
        Ok(dir.join("mcp.sock").display().to_string())
    }

    pub struct Listener(UnixListener);

    impl Listener {
        pub fn bind(endpoint: &str) -> io::Result<Listener> {
            let path = Path::new(endpoint);
            let dir = path.parent().unwrap_or(Path::new("."));
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            if path.exists() {
                if UnixStream::connect(path).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "another AgentRouter is already serving the local MCP",
                    ));
                }
                std::fs::remove_file(path)?;
            }
            let listener = UnixListener::bind(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Listener(listener))
        }

        pub fn accept(&mut self) -> io::Result<(Stream, Option<u32>)> {
            loop {
                let (stream, _) = self.0.accept()?;
                if peer_is_me(&stream) {
                    return Ok((stream, None));
                }
                crate::util::log("local MCP: refused a connection from another user");
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn peer_is_me(stream: &UnixStream) -> bool {
        use std::os::fd::AsRawFd;
        let mut cred = libc::ucred {
            pid: 0,
            uid: u32::MAX,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: getsockopt writes at most `len` bytes into `cred`, which we own.
        let ok = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        } == 0;
        // SAFETY: geteuid has no preconditions.
        ok && cred.uid == unsafe { libc::geteuid() }
    }

    #[cfg(not(target_os = "linux"))]
    fn peer_is_me(stream: &UnixStream) -> bool {
        use std::os::fd::AsRawFd;
        let (mut uid, mut gid) = (u32::MAX, 0);
        // SAFETY: getpeereid writes the two ids we own.
        let ok = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0;
        // SAFETY: geteuid has no preconditions.
        ok && uid == unsafe { libc::geteuid() }
    }

    pub fn connect(endpoint: &str, _pid: u32) -> io::Result<Stream> {
        UnixStream::connect(endpoint)
    }

    pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("token.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(tmp, path)
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::File;
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        SetFileSecurityW, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId,
        GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use crate::consent::wide;

    pub type Stream = File;

    /// This user's SID as text (`S-1-5-21-…`).
    pub fn user_sid() -> io::Result<String> {
        // SAFETY: standard token query; every buffer is sized from the first call and freed below.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u8; len as usize];
            let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
            CloseHandle(token);
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut text: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut n = 0;
            while *text.add(n) != 0 {
                n += 1;
            }
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
            LocalFree(text.cast());
            Ok(sid)
        }
    }

    /// A security descriptor from SDDL, freed on drop.
    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Descriptor {
        fn only_me(rights: &str) -> io::Result<Descriptor> {
            // D:P = a protected DACL (nothing inherited) with one entry: this user.
            let sddl = wide(&format!("D:P(A;;{rights};;;{})", user_sid()?));
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: the SDDL string is NUL-terminated; the descriptor is freed in Drop.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut sd,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Descriptor(sd))
        }
    }

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe { LocalFree(self.0.cast()) };
        }
    }

    pub fn endpoint() -> io::Result<String> {
        let mut name = format!(r"\\.\pipe\agentrouter-device-{}", user_sid()?);
        // A test or portable home gets its own pipe.
        if let Some(home) = std::env::var_os("AGENTROUTER_DEVICE_HOME") {
            let digest = crate::protocol::sha256_hex(home.to_string_lossy().as_bytes());
            name.push('-');
            name.push_str(&digest[..8]);
        }
        Ok(name)
    }

    pub struct Listener {
        name: Vec<u16>,
        sd: Descriptor,
        pending: Option<HANDLE>,
    }

    // SAFETY: the pending pipe handle is only used by the thread that owns the listener.
    unsafe impl Send for Listener {}

    impl Listener {
        fn instance(&self, first: bool) -> io::Result<HANDLE> {
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.sd.0,
                bInheritHandle: 0,
            };
            let open = PIPE_ACCESS_DUPLEX
                | if first {
                    FILE_FLAG_FIRST_PIPE_INSTANCE
                } else {
                    0
                };
            // SAFETY: the name is NUL-terminated and the security attributes outlive the call.
            let h = unsafe {
                CreateNamedPipeW(
                    self.name.as_ptr(),
                    open,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    PIPE_UNLIMITED_INSTANCES,
                    65_536,
                    65_536,
                    0,
                    &sa,
                )
            };
            if h == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            Ok(h)
        }

        pub fn bind(endpoint: &str) -> io::Result<Listener> {
            let mut listener = Listener {
                name: wide(endpoint),
                sd: Descriptor::only_me("GA")?,
                pending: None,
            };
            // The first instance claims the name: if someone else already holds it, this fails.
            listener.pending = Some(listener.instance(true)?);
            Ok(listener)
        }

        pub fn accept(&mut self) -> io::Result<(Stream, Option<u32>)> {
            let h = match self.pending.take() {
                Some(h) => h,
                None => self.instance(false)?,
            };
            // SAFETY: a valid pipe handle, synchronous mode.
            let ok = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) };
            if ok == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
                let e = io::Error::last_os_error();
                // SAFETY: our own handle.
                unsafe { CloseHandle(h) };
                return Err(e);
            }
            // The next instance is ready before this connection is handed over.
            self.pending = self.instance(false).ok();
            let mut pid = 0u32;
            // SAFETY: a connected pipe handle.
            let pid = (unsafe { GetNamedPipeClientProcessId(h, &mut pid) } != 0).then_some(pid);
            // SAFETY: the handle is ours and now owned by the File.
            Ok((unsafe { File::from_raw_handle(h as _) }, pid))
        }
    }

    pub fn connect(endpoint: &str, pid: u32) -> io::Result<Stream> {
        let mut last = io::Error::new(io::ErrorKind::NotFound, "AgentRouter is not running");
        for _ in 0..30 {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                // The server may only identify us, never act as us.
                .security_qos_flags(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION)
                .open(endpoint)
            {
                Ok(file) => {
                    let mut server = 0u32;
                    // SAFETY: a connected pipe handle.
                    let ok = unsafe {
                        GetNamedPipeServerProcessId(file.as_raw_handle() as HANDLE, &mut server)
                    } != 0;
                    if !ok || server != pid {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "the pipe does not belong to the running AgentRouter",
                        ));
                    }
                    return Ok(file);
                }
                // 231 = all instances busy (between two accepts): try again shortly.
                Err(e) if e.raw_os_error() == Some(231) => last = e,
                Err(e) => return Err(e),
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Err(last)
    }

    pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("token.tmp");
        std::fs::write(&tmp, bytes)?;
        let sd = Descriptor::only_me("FA")?;
        let name = wide(&tmp.display().to_string());
        // SAFETY: a NUL-terminated path and a valid descriptor.
        if unsafe { SetFileSecurityW(name.as_ptr(), DACL_SECURITY_INFORMATION, sd.0) } == 0 {
            let e = io::Error::last_os_error();
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        std::fs::rename(tmp, path)
    }
}

#[cfg(windows)]
pub use platform::user_sid;
