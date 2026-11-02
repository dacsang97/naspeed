//! Bounded, lossy UTF-8 line readers for captured child stdout and stderr.

use std::io::{BufRead, BufReader, Read};
use std::thread::{self, JoinHandle};

use super::log_bus::{LogBus, LogSource, Stream};

const MAX_LINE_LEN: usize = 64 * 1024;
const TRUNCATED: &str = "…[truncated]";

pub fn spawn(
    pipe: impl Read + Send + 'static,
    bus: LogBus,
    source: LogSource,
    stream: Stream,
) -> JoinHandle<()> {
    thread::spawn(move || read_lines(BufReader::new(pipe), bus, source, stream))
}

fn read_lines<R: BufRead>(mut reader: R, bus: LogBus, source: LogSource, stream: Stream) {
    let mut line = Vec::with_capacity(1024);
    let mut truncated = false;
    loop {
        let (consumed, newline, eof) = match reader.fill_buf() {
            Ok([]) => (0, false, true),
            Ok(chunk) => {
                let newline_at = chunk.iter().position(|byte| *byte == b'\n');
                let count = newline_at.map_or(chunk.len(), |at| at + 1);
                let content_len = newline_at.unwrap_or(count);
                let available = MAX_LINE_LEN.saturating_sub(line.len());
                let retained = content_len.min(available);
                line.extend_from_slice(&chunk[..retained]);
                truncated |= retained < content_len;
                (count, newline_at.is_some(), false)
            }
            Err(err) => {
                bus.log(
                    LogSource::Ns,
                    Stream::Stderr,
                    format!("app output reader failed: {err}"),
                );
                break;
            }
        };
        if eof {
            if !line.is_empty() || truncated {
                emit(&bus, &source, stream, &mut line, truncated);
            }
            break;
        }
        reader.consume(consumed);
        if newline {
            emit(&bus, &source, stream, &mut line, truncated);
            truncated = false;
        }
    }
}

fn emit(bus: &LogBus, source: &LogSource, stream: Stream, bytes: &mut Vec<u8>, truncated: bool) {
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if truncated {
        text.push_str(TRUNCATED);
    }
    bus.log(source.clone(), stream, text);
    bytes.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::log_bus::DevEvent;
    use std::io::Cursor;

    fn lines(input: &[u8]) -> Vec<String> {
        let (bus, rx) = LogBus::channel();
        read_lines(
            Cursor::new(input.to_vec()),
            bus,
            LogSource::App("api".into()),
            Stream::Stdout,
        );
        rx.try_iter()
            .filter_map(|event| match event {
                DevEvent::Log(line) => Some(line.raw),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn splits_crlf_and_flushes_partial_eof() {
        assert_eq!(lines(b"one\r\ntwo"), ["one", "two"]);
    }

    #[test]
    fn caps_overlong_lines_and_decodes_invalid_utf8_lossily() {
        let mut input = vec![b'x'; MAX_LINE_LEN + 12];
        input.extend_from_slice(b"\n\xff\n");
        let got = lines(&input);
        assert_eq!(got[0].len(), MAX_LINE_LEN + TRUNCATED.len());
        assert!(got[0].ends_with(TRUNCATED));
        assert_eq!(got[1], "�");
    }

    #[test]
    fn emits_empty_lines() {
        assert_eq!(lines(b"\n"), [""]);
    }
}
