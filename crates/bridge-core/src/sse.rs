//! `sse` — a minimal, spec-compliant Server-Sent Events (`text/event-stream`) decoder for the board firehose.
//!
//! Operator directive (#363): the board already serves the `/events` firehose as SSE (the event `seq` is the
//! SSE `Last-Event-ID`, see [`crate::board::Event::seq`]); a bridge should CONSUME that push stream instead of
//! polling `GET /events?since_seq=…` on a timer. The transport (`reqwest`) reads the response body in byte
//! chunks; this decoder turns that chunk stream into whole SSE events, and [`crate::board::BoardClient::stream_events`]
//! maps each event's `data` payload to a board [`crate::board::Event`].
//!
//! The decoder is the canonical line-oriented SSE algorithm (WHATWG `EventSource`): buffer bytes, split into
//! lines on `\n` / `\r` / `\r\n`, and dispatch a frame on a blank line. It is **pure + fully unit-tested** —
//! it never touches the network, so the framing is verified offline (the one board-specific piece, the
//! `data`→`Event` JSON mapping, lives in the client and is confirmed against the live board).
//!
//! Byte-level buffering (not `&str`) is deliberate: `reqwest` hands back arbitrary byte chunks that can split
//! a multi-byte UTF-8 character — or a `\r\n` terminator — across a boundary. Holding partial lines in a byte
//! buffer and only decoding a line once it is terminated makes both splits a non-issue (`\r`/`\n` are
//! single-byte and never occur inside a multi-byte UTF-8 sequence).

/// One decoded SSE event: a block of field lines terminated by a blank line.
///
/// `id` is the block's `id:` field when present (the `Last-Event-ID` resume cursor — for the board, the event
/// `seq`); `event` is its `event:` type; `data` is the `data:` line(s) joined with `\n` (no trailing newline).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseFrame {
    /// The `id:` field (persisted by the reader as the resume cursor), if the block carried one.
    pub id: Option<String>,
    /// The `event:` type, if the block named one (default event type otherwise).
    pub event: Option<String>,
    /// The concatenated `data:` payload (multiple `data:` lines joined by `\n`).
    pub data: String,
}

/// A streaming SSE decoder: feed it raw response-body byte chunks with [`push`](Self::push); it returns the
/// frames those chunks completed. Partial lines (and partial UTF-8 / a dangling `\r`) are retained across
/// calls, so it is safe to drive from an arbitrary byte-chunk stream.
#[derive(Debug, Default)]
pub struct SseDecoder {
    /// Unconsumed raw bytes (an in-progress line at the tail).
    buf: Vec<u8>,
    /// Fields accumulated for the in-progress event block, dispatched on the next blank line.
    id: Option<String>,
    event: Option<String>,
    data: Vec<String>,
    /// Whether the in-progress block has seen any recognized field (so a blank line dispatches a frame rather
    /// than firing on stray blank lines / a leading keepalive).
    saw_field: bool,
}

impl SseDecoder {
    /// A fresh decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of the response body; returns every event completed by it (a blank line ends an event).
    /// Bytes that form an incomplete line — including a trailing `\r` that might yet pair with `\n`, or a
    /// partial multi-byte UTF-8 character — are buffered for the next call.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some((line, consumed)) = take_line(&self.buf) {
            let line_str = String::from_utf8_lossy(&line).into_owned();
            self.buf.drain(..consumed);
            if let Some(frame) = self.process_line(&line_str) {
                frames.push(frame);
            }
        }
        frames
    }

    /// Apply one complete line to the in-progress block. A blank line dispatches the accumulated frame
    /// (returned); a field line updates the accumulator; a comment (`:` prefix) / unknown field is ignored.
    fn process_line(&mut self, line: &str) -> Option<SseFrame> {
        if line.is_empty() {
            // Blank line: dispatch the accumulated event, if it carried any field.
            if !self.saw_field {
                return None;
            }
            let frame = SseFrame {
                id: self.id.take(),
                event: self.event.take(),
                data: self.data.join("\n"),
            };
            self.data.clear();
            self.saw_field = false;
            return Some(frame);
        }
        if line.starts_with(':') {
            return None; // comment / keepalive line
        }
        // `field:value` (one leading space of the value stripped), or a bare `field` with an empty value.
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "id" => {
                self.id = Some(value.to_string());
                self.saw_field = true;
            }
            "event" => {
                self.event = Some(value.to_string());
                self.saw_field = true;
            }
            "data" => {
                self.data.push(value.to_string());
                self.saw_field = true;
            }
            "retry" => {
                // Reconnection-time hint; our reconnect cadence is fixed, so we note it as a field (so a
                // retry-only block still dispatches its id) but otherwise ignore the value.
                self.saw_field = true;
            }
            _ => {} // unknown field: ignored per spec
        }
        None
    }
}

/// Take the first complete line from `buf`: its bytes (terminator excluded) and how many bytes to consume
/// (including the terminator). Terminators are `\n`, `\r`, or `\r\n`. Returns `None` when no complete line is
/// buffered yet — including a lone trailing `\r` (it might still pair with a `\n`), so no line is emitted
/// until the terminator is unambiguous. Pure.
fn take_line(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
    let pos = buf.iter().position(|&b| b == b'\n' || b == b'\r')?;
    match buf[pos] {
        b'\n' => Some((buf[..pos].to_vec(), pos + 1)),
        b'\r' => {
            match buf.get(pos + 1) {
                Some(&b'\n') => Some((buf[..pos].to_vec(), pos + 2)), // CRLF
                Some(_) => Some((buf[..pos].to_vec(), pos + 1)),      // lone CR terminator
                None => None, // trailing CR: wait — it might be the CR of a CRLF split across chunks
            }
        }
        _ => unreachable!("position matched \\n or \\r"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the decoder with one whole byte slice and collect every frame.
    fn decode_all(input: &str) -> Vec<SseFrame> {
        SseDecoder::new().push(input.as_bytes())
    }

    #[test]
    fn single_event_lf() {
        let frames = decode_all("data: hello\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
        assert_eq!(frames[0].id, None);
        assert_eq!(frames[0].event, None);
    }

    #[test]
    fn crlf_terminators() {
        let frames = decode_all("id: 42\r\nevent: message\r\ndata: {\"k\":1}\r\n\r\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id.as_deref(), Some("42"));
        assert_eq!(frames[0].event.as_deref(), Some("message"));
        assert_eq!(frames[0].data, "{\"k\":1}");
    }

    #[test]
    fn multiple_data_lines_join_with_newline() {
        let frames = decode_all("data: line one\ndata: line two\n\n");
        assert_eq!(frames[0].data, "line one\nline two");
    }

    #[test]
    fn comment_and_keepalive_lines_ignored() {
        // A leading `:` comment (a keepalive ping) produces no frame; the real event still decodes.
        let frames = decode_all(": keepalive\n\ndata: x\n\n");
        assert_eq!(frames.len(), 1, "the keepalive block dispatches nothing");
        assert_eq!(frames[0].data, "x");
    }

    #[test]
    fn one_leading_space_stripped_from_value_only_once() {
        let frames = decode_all("data:  two-leading-spaces\n\n");
        assert_eq!(frames[0].data, " two-leading-spaces", "exactly one leading space is stripped");
    }

    #[test]
    fn field_without_colon_is_empty_value() {
        // A bare `data` line contributes an empty data line (per spec).
        let frames = decode_all("data\ndata: y\n\n");
        assert_eq!(frames[0].data, "\ny");
    }

    #[test]
    fn two_events_in_one_stream() {
        let frames = decode_all("id: 1\ndata: a\n\nid: 2\ndata: b\n\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].id.as_deref(), Some("1"));
        assert_eq!(frames[0].data, "a");
        assert_eq!(frames[1].id.as_deref(), Some("2"));
        assert_eq!(frames[1].data, "b");
    }

    #[test]
    fn id_only_frame_dispatches_for_cursor_tracking() {
        // An id-only block (no data) still dispatches so the reader can advance its resume cursor.
        let frames = decode_all("id: 99\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id.as_deref(), Some("99"));
        assert_eq!(frames[0].data, "");
    }

    #[test]
    fn unknown_field_ignored() {
        let frames = decode_all("weird: nope\ndata: kept\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "kept");
    }

    #[test]
    fn incomplete_block_yields_nothing_until_blank_line() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: partial\n").is_empty(), "no blank line yet -> no frame");
        let frames = d.push(b"\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "partial");
    }

    #[test]
    fn chunk_split_mid_line_reassembles() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"da").is_empty());
        assert!(d.push(b"ta: spl").is_empty());
        let frames = d.push(b"it\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "split");
    }

    #[test]
    fn crlf_split_across_chunks_not_treated_as_two_terminators() {
        // A chunk ending in `\r` must NOT be emitted as a line yet: the next chunk's `\n` completes a CRLF,
        // not a second (blank-line) terminator.
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: hi\r").is_empty(), "dangling CR is held back");
        assert!(d.push(b"\n").is_empty(), "CRLF completes the line, still no blank line");
        let frames = d.push(b"\r\n");
        assert_eq!(frames.len(), 1, "the blank CRLF line dispatches");
        assert_eq!(frames[0].data, "hi");
    }

    #[test]
    fn multibyte_utf8_split_across_chunks() {
        // "héllo" — the 'é' (0xC3 0xA9) is split across the chunk boundary; the line must reassemble intact.
        let full = "data: héllo\n\n";
        let bytes = full.as_bytes();
        // Find a split point in the middle of the 'é'.
        let e_pos = full.find('é').unwrap();
        let mut d = SseDecoder::new();
        assert!(d.push(&bytes[..e_pos + 1]).is_empty(), "split inside the multi-byte char");
        let frames = d.push(&bytes[e_pos + 1..]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "héllo");
    }

    #[test]
    fn lone_cr_splits_lines() {
        // Old-style lone-CR line terminators are valid SSE: here `\r` separates the two data lines, and the
        // trailing `\n\n` (unambiguous) dispatches. (A stream ending in a bare `\r` is deliberately held back
        // — it might yet be the CR of a split CRLF — see `crlf_split_across_chunks_not_treated_as_two_terminators`.)
        let frames = decode_all("data: a\rdata: b\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "a\nb");
    }

    #[test]
    fn trailing_lone_cr_is_held_then_resolves_as_lone_terminator() {
        // A buffer ending in a bare `\r` yields no frame yet (it could be the start of a split CRLF); once a
        // following non-`\n` byte arrives, the CR resolves as a lone-CR line terminator and both lines decode.
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: a\r").is_empty(), "dangling CR is held back");
        let frames = d.push(b"data: b\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "a\nb");
    }
}
