//! Versioned, bounded wire protocol between an `ns` client and the graph
//! service. Transport-agnostic: a single frame is `u32le length` + body,
//! and the body is a one-byte message tag followed by fields encoded the
//! same way `action_key` encodes them (`u32le` counts, length-prefixed
//! UTF-8 strings) — never JSON, so golden byte vectors stay canonical.
//!
//! ```text
//! frame      := u32le body_len + body              (body_len ≤ MAX_FRAME)
//! Hello      := 'H' u32le protocol_version         (client → service)
//! HelloReply := 'h' u32le protocol_version u32le flags
//! Load       := 'L' str project_root str store_root
//! LoadReply  := 'l' str language u32le node_count
//! Changes    := 'C' seq<str> abs_paths             (one change journal)
//! Plan       := 'P'                                (compute after Changes)
//! PlanReply  := 'p' u8 reload seq<(u8 kind, str name)> actions u32le ignored
//! Status     := 'S'
//! StatusReply:= 's' str language u32le nodes u32le uptime_s
//! Shutdown   := 'D'
//! Bye        := 'b'
//! Error      := 'E' u32le code str message
//! ```
//!
//! Everything is capped (`MAX_FRAME`, `MAX_PATHS`, `MAX_STRING`,
//! `MAX_ACTIONS`); anything over the cap is a protocol error, not a read.

use std::io::{self, Read, Write};
use std::path::PathBuf;

/// Bump on any incompatible wire change; clients on a different version
/// fall back to in-process planning.
pub const PROTOCOL_VERSION: u32 = 1;

pub const MAX_FRAME: usize = 4 * 1024 * 1024;
pub const MAX_PATHS: usize = 16_384;
pub const MAX_STRING: usize = 65_536;
pub const MAX_ACTIONS: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Wire bytes violated the grammar or a bound.
    BadMessage = 1,
    /// Client `Hello` carried a protocol version the service doesn't speak.
    VersionMismatch = 2,
    /// A path escaped the loaded project/store roots, or was relative.
    PathOutsideRoots = 3,
    /// The service has no graph loaded yet.
    NotLoaded = 4,
    /// Graph (re)load failed; `message` carries the diagnostic.
    LoadFailed = 5,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionEntry {
    /// `ActionKind::tag()` byte.
    pub kind: u8,
    pub name: String,
}

/// One full message body, tag byte included via `tag()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello { version: u32 },
    HelloReply { version: u32, flags: u32 },
    Load { project_root: String, store_root: String },
    LoadReply { language: String, node_count: u32 },
    Changes { paths: Vec<String> },
    Plan,
    PlanReply { reload: bool, actions: Vec<ActionEntry>, ignored: u32 },
    Status,
    StatusReply { language: String, nodes: u32, uptime_s: u32 },
    Shutdown,
    Bye,
    Error { code: ErrorCode, message: String },
}

impl Message {
    pub fn tag(&self) -> u8 {
        match self {
            Message::Hello { .. } => b'H',
            Message::HelloReply { .. } => b'h',
            Message::Load { .. } => b'L',
            Message::LoadReply { .. } => b'l',
            Message::Changes { .. } => b'C',
            Message::Plan => b'P',
            Message::PlanReply { .. } => b'p',
            Message::Status => b'S',
            Message::StatusReply { .. } => b's',
            Message::Shutdown => b'D',
            Message::Bye => b'b',
            Message::Error { .. } => b'E',
        }
    }
}

// ---------------------------------------------------------------- encode

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Body bytes (tag + fields) for `msg`.
pub fn encode_body(msg: &Message) -> Vec<u8> {
    let mut out = vec![msg.tag()];
    match msg {
        Message::Hello { version } | Message::HelloReply { version, .. } => {
            out.extend_from_slice(&version.to_le_bytes());
            if let Message::HelloReply { flags, .. } = msg {
                out.extend_from_slice(&flags.to_le_bytes());
            }
        }
        Message::Load { project_root, store_root } => {
            put_str(&mut out, project_root);
            put_str(&mut out, store_root);
        }
        Message::LoadReply { language, node_count } => {
            put_str(&mut out, language);
            out.extend_from_slice(&node_count.to_le_bytes());
        }
        Message::Changes { paths } => {
            out.extend_from_slice(&(paths.len() as u32).to_le_bytes());
            for path in paths {
                put_str(&mut out, path);
            }
        }
        Message::Plan | Message::Status | Message::Shutdown | Message::Bye => {}
        Message::PlanReply { reload, actions, ignored } => {
            out.push(u8::from(*reload));
            out.extend_from_slice(&(actions.len() as u32).to_le_bytes());
            for action in actions {
                out.push(action.kind);
                put_str(&mut out, &action.name);
            }
            out.extend_from_slice(&ignored.to_le_bytes());
        }
        Message::StatusReply { language, nodes, uptime_s } => {
            put_str(&mut out, language);
            out.extend_from_slice(&nodes.to_le_bytes());
            out.extend_from_slice(&uptime_s.to_le_bytes());
        }
        Message::Error { code, message } => {
            out.extend_from_slice(&(*code as u32).to_le_bytes());
            put_str(&mut out, message);
        }
    }
    out
}

/// `u32le length + body`, ready for the wire.
pub fn encode_frame(msg: &Message) -> Vec<u8> {
    let body = encode_body(msg);
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

// ---------------------------------------------------------------- decode

struct Cursor<'a> {
    body: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.body.len() - self.at < n {
            return Err(bad("truncated message"));
        }
        let out = &self.body[self.at..self.at + n];
        self.at += n;
        Ok(out)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn string(&mut self) -> io::Result<String> {
        let len = self.u32()? as usize;
        if len > MAX_STRING {
            return Err(bad("string over limit"));
        }
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| bad("string not UTF-8"))
    }

    fn done(&self) -> bool {
        self.at == self.body.len()
    }
}

fn bad(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

/// Parse one message body (already de-framed).
pub fn decode_body(body: &[u8]) -> io::Result<Message> {
    let mut cur = Cursor { body, at: 0 };
    let tag = cur.u8()?;
    let msg = match tag {
        b'H' => Message::Hello { version: cur.u32()? },
        b'h' => Message::HelloReply { version: cur.u32()?, flags: cur.u32()? },
        b'L' => Message::Load {
            project_root: cur.string()?,
            store_root: cur.string()?,
        },
        b'l' => Message::LoadReply { language: cur.string()?, node_count: cur.u32()? },
        b'C' => {
            let count = cur.u32()? as usize;
            if count > MAX_PATHS {
                return Err(bad("too many paths"));
            }
            let mut paths = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                paths.push(cur.string()?);
            }
            Message::Changes { paths }
        }
        b'P' => Message::Plan,
        b'p' => {
            let reload = match cur.u8()? {
                0 => false,
                1 => true,
                _ => return Err(bad("invalid reload flag")),
            };
            let count = cur.u32()? as usize;
            if count > MAX_ACTIONS {
                return Err(bad("too many actions"));
            }
            let mut actions = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                let kind = cur.u8()?;
                let name = cur.string()?;
                actions.push(ActionEntry { kind, name });
            }
            let ignored = cur.u32()?;
            Message::PlanReply { reload, actions, ignored }
        }
        b'S' => Message::Status,
        b's' => Message::StatusReply {
            language: cur.string()?,
            nodes: cur.u32()?,
            uptime_s: cur.u32()?,
        },
        b'D' => Message::Shutdown,
        b'b' => Message::Bye,
        b'E' => {
            let code = match cur.u32()? {
                1 => ErrorCode::BadMessage,
                2 => ErrorCode::VersionMismatch,
                3 => ErrorCode::PathOutsideRoots,
                4 => ErrorCode::NotLoaded,
                5 => ErrorCode::LoadFailed,
                _ => return Err(bad("unknown error code")),
            };
            Message::Error { code, message: cur.string()? }
        }
        _ => return Err(bad("unknown message tag")),
    };
    if !cur.done() {
        return Err(bad("trailing bytes"));
    }
    Ok(msg)
}

// ---------------------------------------------------------------- stream

/// Read one bounded frame from `r`; `Ok(None)` on clean EOF before a frame.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Message>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(bad("frame over limit"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    decode_body(&body).map(Some)
}

pub fn write_frame(w: &mut impl Write, msg: &Message) -> io::Result<()> {
    w.write_all(&encode_frame(msg))?;
    w.flush()
}

/// Paths a client may mention must live under the loaded project root or
/// the store root; anything else is a protocol error, not a plan input.
pub fn path_under_roots(path: &PathBuf, project_root: &PathBuf, store_root: &PathBuf) -> bool {
    path.is_absolute()
        && (path.starts_with(project_root) || path.starts_with(store_root))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden wire vectors: byte-exact request/response encoding. Changing
    /// these bytes silently means a protocol break, not a refactor.
    #[test]
    fn golden_frames() {
        let vectors: Vec<(Message, &[u8])> = vec![
            (Message::Hello { version: 1 }, b"H\x01\x00\x00\x00"),
            (
                Message::HelloReply { version: 1, flags: 0 },
                b"h\x01\x00\x00\x00\x00\x00\x00\x00",
            ),
            (
                Message::Load {
                    project_root: "/p".into(),
                    store_root: "/s".into(),
                },
                b"L\x02\x00\x00\x00/p\x02\x00\x00\x00/s",
            ),
            (Message::Plan, b"P"),
            (
                Message::PlanReply {
                    reload: false,
                    actions: vec![ActionEntry {
                        kind: b'C',
                        name: "greeter".into(),
                    }],
                    ignored: 0,
                },
                b"p\x00\x01\x00\x00\x00C\x07\x00\x00\x00greeter\x00\x00\x00\x00",
            ),
            (
                Message::Error {
                    code: ErrorCode::VersionMismatch,
                    message: "v".into(),
                },
                b"E\x02\x00\x00\x00\x01\x00\x00\x00v",
            ),
            (Message::Shutdown, b"D"),
            (Message::Bye, b"b"),
        ];
        for (msg, body) in vectors {
            assert_eq!(encode_body(&msg), body, "encode {msg:?}");
            assert_eq!(decode_body(body).unwrap(), msg, "decode {msg:?}");
            let frame = encode_frame(&msg);
            assert_eq!(&frame[..4], (body.len() as u32).to_le_bytes().as_slice());
            assert_eq!(
                read_frame(&mut io::Cursor::new(frame)).unwrap(),
                Some(msg)
            );
        }
    }

    #[test]
    fn rejects_oversize_and_trailing() {
        let mut frame = (MAX_FRAME as u32 + 1).to_le_bytes().to_vec();
        assert!(read_frame(&mut io::Cursor::new(frame.clone())).is_err());
        frame = 1u32.to_le_bytes().to_vec();
        frame.push(0x7f); // unknown tag
        assert!(read_frame(&mut io::Cursor::new(frame)).is_err());
        assert!(decode_body(b"Pjunk").is_err());
    }
}
