//! Graph service lifecycle: one owner per (user, store), a Unix-socket
//! endpoint guarded by filesystem permissions and peer-uid checks, stale
//! recovery by construction (locks and sockets both die with the owner),
//! and an idle lifetime bound.
//!
//! The service holds only rebuildable state — the loaded `EngineGraph`,
//! the digest memo and the pending change journal — so losing it costs
//! one graph load, and `plan_or_fallback` reproduces the same plan
//! in-process whenever the socket is unreachable, owned by someone else,
//! speaking another protocol version, or dead mid-request.
//!
//! Non-Unix platforms have no transport here yet; every entry point
//! reports "unsupported" and the caller falls back to in-process
//! planning, keeping behavior correct on Windows until named pipes land.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::digest_memo::DigestMemo;
use super::graph::{ActionId, ActionKind, EngineGraph, Plan};
use super::protocol::{
    self, ActionEntry, ErrorCode, Message, PROTOCOL_VERSION, path_under_roots,
};

/// Default bound on service lifetime without client activity.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(15 * 60);

/// `<store>/graph-service/` — socket, owner lock and owner record.
pub fn service_dir(store_root: &Path) -> PathBuf {
    store_root.join("graph-service")
}

fn socket_path(dir: &Path) -> PathBuf {
    dir.join("graph.sock")
}

fn lock_path(dir: &Path) -> PathBuf {
    dir.join("owner.lock")
}

fn owner_record_path(dir: &Path) -> PathBuf {
    dir.join("owner.json")
}

/// Held exclusively by the running service; dies with its process, so a
/// crashed owner never leaves the lock held.
pub struct ServiceOwner {
    _lock: File,
    dir: PathBuf,
}

impl ServiceOwner {
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Try to become the (single) service owner for `store_root`.
/// `Ok(None)` means another live process owns it — connect to its socket
/// or fall back, never steal.
pub fn try_acquire(store_root: &Path) -> Result<Option<ServiceOwner>> {
    let dir = service_dir(store_root);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(&dir))
        .context("open service owner lock")?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(ServiceOwner { _lock: lock, dir })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(err @ std::fs::TryLockError::Error(_)) => Err(err).context("try service owner lock"),
    }
}

fn write_owner_record(dir: &Path) -> Result<()> {
    let record = format!(
        "{{\"pid\":{},\"protocol\":{}}}\n",
        std::process::id(),
        PROTOCOL_VERSION
    );
    fs::write(owner_record_path(dir), record).context("write owner record")
}

// ---------------------------------------------------------------- unix

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    /// Effective uid of the process on the other end of `stream`.
    #[cfg(target_os = "linux")]
    fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
        Ok(stream.peer_cred()?.uid())
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
        use std::os::unix::io::AsRawFd;
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: getpeereid fills uid/gid for a connected unix socket.
        let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if rc == 0 { Ok(uid) } else { Err(io::Error::last_os_error()) }
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    fn peer_uid(_stream: &UnixStream) -> io::Result<u32> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no peer uid API"))
    }

    pub(super) fn own_uid() -> u32 {
        // SAFETY: geteuid cannot fail.
        unsafe { libc::geteuid() }
    }

    /// Reject any peer that is not the same user. The socket itself is
    /// mode 0600, so this is belt over suspenders — the check matters on
    /// filesystems that ignore mode bits.
    fn verify_peer(stream: &UnixStream) -> io::Result<()> {
        let uid = peer_uid(stream)?;
        if uid == own_uid() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("peer uid {uid} is not {}", own_uid()),
            ))
        }
    }

    /// Serve the graph engine until `stop`, a Shutdown request, or `idle`
    /// elapses without a completed request. Blocks the calling thread.
    pub fn serve(store_root: &Path, idle: Duration, stop: &AtomicBool) -> Result<()> {
        let Some(owner) = try_acquire(store_root)? else {
            anyhow::bail!("another graph service already owns {}", store_root.display());
        };
        let dir = owner.dir().to_path_buf();
        let sock = socket_path(&dir);
        // A stale socket means a dead owner (its lock was free): remove
        // and rebind. Never remove while the lock is held by someone else.
        let _ = fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock)
            .with_context(|| format!("bind {}", sock.display()))?;
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))
                .context("chmod service socket")?;
        }
        write_owner_record(&dir)?;
        listener
            .set_nonblocking(true)
            .context("nonblocking listener")?;
        let guard = SocketGuard(sock.clone());
        let mut last_activity = Instant::now();
        // One session for every connection: graph, memo and the pending
        // change journal are shared state, serialized by the mutex so
        // responses stay deterministic.
        let session = std::sync::Arc::new(std::sync::Mutex::new(Session::new(store_root)));
        let shutdown = std::sync::Arc::new(AtomicBool::new(false));
        let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
        // Accepted streams kept so shutdown can kick idle connections:
        // without this an open-but-silent client delays exit forever.
        let live = std::sync::Arc::new(std::sync::Mutex::new(Vec::<UnixStream>::new()));
        while !stop.load(Ordering::Relaxed) && !shutdown.load(Ordering::Relaxed) {
            if last_activity.elapsed() > idle {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    // Accepted sockets inherit O_NONBLOCK on BSD/macOS;
                    // request handling is a blocking read/write loop.
                    let _ = stream.set_nonblocking(false);
                    let session = session.clone();
                    let shutdown = shutdown.clone();
                    if let Ok(clone) = stream.try_clone()
                        && let Ok(mut live) = live.lock()
                    {
                        live.push(clone);
                    }
                    workers.push(std::thread::spawn(move || {
                        if handle_conn(stream, &session) {
                            shutdown.store(true, Ordering::Relaxed);
                        }
                    }));
                    last_activity = Instant::now();
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err).context("accept"),
            }
        }
        drop(listener); // unblock lingering accepts, then drain workers
        if let Ok(live) = live.lock() {
            for stream in live.iter() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
        for worker in workers {
            let _ = worker.join();
        }
        drop(guard);
        let _ = fs::remove_file(owner_record_path(&dir));
        Ok(())
    }

    struct SocketGuard(PathBuf);
    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    struct Session {
        store_root: PathBuf,
        project_root: Option<PathBuf>,
        graph: Option<EngineGraph>,
        memo: DigestMemo,
        pending: Vec<PathBuf>,
    }

    impl Session {
        fn new(store_root: &Path) -> Session {
            Session {
                store_root: super::super::graph::canonicalize_lenient(store_root),
                project_root: None,
                graph: None,
                memo: DigestMemo::new(),
                pending: Vec::new(),
            }
        }

        /// `(reply, hang_up, stop_service)` — a refused Hello only drops
        /// that connection; only Shutdown stops the service.
        fn dispatch(&mut self, msg: Message) -> (Message, bool, bool) {
            match msg {
                Message::Load { project_root, store_root } => {
                    let raw_project = PathBuf::from(&project_root);
                    let raw_store = PathBuf::from(&store_root);
                    if !raw_project.is_absolute() || !raw_store.is_absolute() {
                        return (
                            err(ErrorCode::PathOutsideRoots, "roots must be absolute"),
                            true,
                            false,
                        );
                    }
                    let project = super::super::graph::canonicalize_lenient(&raw_project);
                    let store = super::super::graph::canonicalize_lenient(&raw_store);
                    if store != self.store_root {
                        return (
                            err(ErrorCode::PathOutsideRoots, "store root is not this service's"),
                            true,
                            false,
                        );
                    }
                    match EngineGraph::load(&project, &store) {
                        Ok(graph) => {
                            graph.prime(&mut self.memo);
                            let reply = Message::LoadReply {
                                language: language_name(graph.language).to_string(),
                                node_count: graph.node_count() as u32,
                            };
                            self.project_root = Some(project);
                            self.pending.clear();
                            self.graph = Some(graph);
                            (reply, false, false)
                        }
                        Err(e) => (err(ErrorCode::LoadFailed, &format!("{e:#}")), false, false),
                    }
                }
                Message::Changes { paths } => {
                    let Some(root) = &self.project_root else {
                        return (err(ErrorCode::NotLoaded, "load first"), false, false);
                    };
                    for raw in &paths {
                        let path =
                            super::super::graph::canonicalize_lenient(&PathBuf::from(raw));
                        if !path_under_roots(&path, root, &self.store_root) {
                            return (
                                err(ErrorCode::PathOutsideRoots, "change path outside roots"),
                                true,
                                false,
                            );
                        }
                        self.pending.push(path);
                    }
                    (Message::Bye, false, false)
                }
                Message::Plan => {
                    let Some(graph) = &self.graph else {
                        return (err(ErrorCode::NotLoaded, "load first"), false, false);
                    };
                    let plan = graph.plan(&self.pending, &mut self.memo);
                    self.pending.clear();
                    (plan_reply(&plan), false, false)
                }
                Message::Status => {
                    let (language, nodes) = self
                        .graph
                        .as_ref()
                        .map(|g| (language_name(g.language).to_string(), g.node_count() as u32))
                        .unwrap_or_else(|| ("none".to_string(), 0));
                    (Message::StatusReply { language, nodes, uptime_s: 0 }, false, false)
                }
                Message::Shutdown => (Message::Bye, true, true),
                Message::Hello { version } => {
                    if version == PROTOCOL_VERSION {
                        (
                            Message::HelloReply { version: PROTOCOL_VERSION, flags: 0 },
                            false,
                            false,
                        )
                    } else {
                        (
                            err(ErrorCode::VersionMismatch, "unsupported protocol version"),
                            true,
                            false,
                        )
                    }
                }
                _ => (err(ErrorCode::BadMessage, "unexpected message"), false, false),
            }
        }
    }

    /// One client connection: Hello first, then requests until EOF.
    /// Returns true when the client asked for shutdown. A peer that is
    /// not us gets disconnected before it sends a byte of payload. The
    /// session mutex serializes dispatch across connections; socket
    /// reads/writes happen outside the lock so a slow client cannot
    /// starve the others.
    fn handle_conn(stream: UnixStream, session: &std::sync::Mutex<Session>) -> bool {
        if verify_peer(&stream).is_err() {
            return false;
        }
        let mut reader = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return false,
        };
        let mut writer = stream;
        let mut greeted = false;
        loop {
            let msg = match protocol::read_frame(&mut reader) {
                Ok(Some(msg)) => msg,
                Ok(None) => return false,
                Err(_) => return false,
            };
            let is_hello = matches!(msg, Message::Hello { .. });
            if !greeted && !is_hello {
                let _ = protocol::write_frame(
                    &mut writer,
                    &err(ErrorCode::BadMessage, "expected Hello"),
                );
                return false;
            }
            let (reply, hang_up, stop_service) = match session.lock() {
                Ok(mut session) => session.dispatch(msg),
                Err(poisoned) => poisoned.into_inner().dispatch(msg),
            };
            greeted |= is_hello;
            let _ = protocol::write_frame(&mut writer, &reply);
            if hang_up || stop_service {
                return stop_service;
            }
        }
    }
}

fn err(code: ErrorCode, message: &str) -> Message {
    Message::Error { code, message: message.to_string() }
}

fn language_name(language: super::graph::Language) -> &'static str {
    match language {
        super::graph::Language::Rust => "rust",
        super::graph::Language::Go => "go",
    }
}

fn plan_reply(plan: &Plan) -> Message {
    Message::PlanReply {
        reload: plan.reload,
        actions: plan
            .actions
            .iter()
            .map(|id| ActionEntry { kind: id.kind.tag() as u8, name: id.name.clone() })
            .collect(),
        ignored: plan.ignored as u32,
    }
}

fn plan_from_reply(reply: Message) -> Result<Plan> {
    let Message::PlanReply { reload, actions, ignored } = reply else {
        anyhow::bail!("unexpected reply to plan: {reply:?}");
    };
    let mut set = std::collections::BTreeSet::new();
    for ActionEntry { kind, name } in actions {
        let Some(kind) = ActionKind::from_tag(kind) else {
            anyhow::bail!("unknown action kind {kind:#x} in plan reply");
        };
        set.insert(ActionId { kind, name });
    }
    Ok(Plan { reload, actions: set, ignored: ignored as usize })
}

/// Serve on Unix; unsupported elsewhere (callers fall back in-process).
#[cfg(unix)]
pub fn serve(store_root: &Path, idle: Duration, stop: &AtomicBool) -> Result<()> {
    imp::serve(store_root, idle, stop)
}

#[cfg(not(unix))]
pub fn serve(_store_root: &Path, _idle: Duration, _stop: &AtomicBool) -> Result<()> {
    anyhow::bail!("graph service transport is unsupported on this platform")
}

// ---------------------------------------------------------------- client

/// One connection to the local graph service.
pub struct Client {
    #[cfg(unix)]
    stream: std::os::unix::net::UnixStream,
    #[cfg(unix)]
    store_root: PathBuf,
}

#[cfg(unix)]
impl Client {
    /// Connect to the service socket for `store_root` and handshake.
    /// Fails — never hangs — when no live same-user service is listening.
    pub fn connect(store_root: &Path) -> Result<Client> {
        use std::os::unix::net::UnixStream;
        let sock = socket_path(&service_dir(store_root));
        let meta = fs::metadata(&sock)
            .with_context(|| format!("no service socket at {}", sock.display()))?;
        {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != imp::own_uid() {
                anyhow::bail!("service socket {} is not ours", sock.display());
            }
        }
        let stream = UnixStream::connect(&sock)
            .with_context(|| format!("connect {}", sock.display()))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .context("read timeout")?;
        let mut client = Client { stream, store_root: store_root.to_path_buf() };
        client.hello()?;
        Ok(client)
    }

    fn hello(&mut self) -> Result<()> {
        match self.request(&Message::Hello { version: PROTOCOL_VERSION })? {
            Message::HelloReply { version, .. } if version == PROTOCOL_VERSION => Ok(()),
            Message::Error { code, message } => {
                anyhow::bail!("service handshake failed ({code:?}): {message}")
            }
            other => anyhow::bail!("unexpected handshake reply {other:?}"),
        }
    }

    fn request(&mut self, msg: &Message) -> Result<Message> {
        protocol::write_frame(&mut self.stream, msg).context("send request")?;
        let reply = protocol::read_frame(&mut self.stream)
            .context("read reply")?
            .context("service closed connection")?;
        Ok(reply)
    }

    /// Load (or reload) the graph for `project_root`, send the change
    /// journal, and return the plan.
    pub fn plan_for(&mut self, project_root: &Path, changes: &[PathBuf]) -> Result<Plan> {
        match self.request(&Message::Load {
            project_root: project_root.to_string_lossy().into_owned(),
            store_root: self.store_root.to_string_lossy().into_owned(),
        })? {
            Message::LoadReply { .. } => {}
            Message::Error { code, message } => {
                anyhow::bail!("service load failed ({code:?}): {message}")
            }
            other => anyhow::bail!("unexpected load reply {other:?}"),
        }
        if !changes.is_empty() {
            match self.request(&Message::Changes {
                paths: changes
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
            })? {
                Message::Bye => {}
                Message::Error { code, message } => {
                    anyhow::bail!("service rejected changes ({code:?}): {message}")
                }
                other => anyhow::bail!("unexpected changes reply {other:?}"),
            }
        }
        plan_from_reply(self.request(&Message::Plan)?)
    }

    /// Ask the service to stop; it replies Bye then exits its loop.
    pub fn shutdown(&mut self) -> Result<()> {
        match self.request(&Message::Shutdown)? {
            Message::Bye => Ok(()),
            other => anyhow::bail!("unexpected shutdown reply {other:?}"),
        }
    }
}

/// Plan via the service when a live same-user owner answers, otherwise
/// in-process. Both paths run `EngineGraph::plan`, so the output is
/// identical by construction.
pub fn plan_or_fallback(project_root: &Path, store_root: &Path, changes: &[PathBuf]) -> Result<Plan> {
    #[cfg(unix)]
    {
        if let Ok(mut client) = Client::connect(store_root)
            && let Ok(plan) = client.plan_for(project_root, changes)
        {
            return Ok(plan);
        }
    }
    in_process_plan(project_root, store_root, changes)
}

/// The fallback path used when the service is absent, crashed,
/// version-mismatched, or unsupported on this platform.
pub fn in_process_plan(
    project_root: &Path,
    store_root: &Path,
    changes: &[PathBuf],
) -> Result<Plan> {
    let mut memo = DigestMemo::new();
    let graph = EngineGraph::load(project_root, store_root)?;
    graph.prime(&mut memo);
    Ok(graph.plan(changes, &mut memo))
}
