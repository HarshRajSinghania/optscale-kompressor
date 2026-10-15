//! Server-sent event framing.
//!
//! Written here rather than taken from a crate because the relay needs
//! something no SSE *client* provides: the exact bytes of each frame, so a
//! frame the state machine has no opinion about can be handed to the client
//! without being re-serialized. A decoded-and-re-encoded stream is a different
//! stream — field order, spacing and unknown fields all move — and provider SSE
//! carries fields we deliberately do not understand.
//!
//! The parser is deliberately tolerant. Providers terminate frames with `\n\n`
//! and proxies in front of them sometimes rewrite that to `\r\n\r\n`; a single
//! frame's `data` may be split across several lines that the spec says to join
//! with a newline; comment lines (`: ping`) are keep-alives that carry no
//! event; and `data: [DONE]` is OpenAI's end marker rather than a JSON payload.
//! Getting any of these wrong means a stalled or corrupted client stream, which
//! is worse than never having compressed the request at all.

use bytes::{Bytes, BytesMut};
use serde_json::Value;

/// One complete event, with the bytes it arrived as.
#[derive(Clone, Debug)]
pub struct SseFrame {
    raw: Bytes,
    event: Option<String>,
    data: String,
    has_data: bool,
}

impl SseFrame {
    /// The frame exactly as it arrived, terminator included.
    ///
    /// This is what a passthrough relay writes: no field this proxy does not
    /// understand can be lost by relaying it.
    pub fn raw(&self) -> &Bytes {
        &self.raw
    }

    /// The `event:` field, when the provider sends one. Anthropic does; the
    /// OpenAI families put the discriminator inside `data` instead.
    pub fn event(&self) -> Option<&str> {
        self.event.as_deref()
    }

    /// The joined `data:` lines.
    pub fn data(&self) -> &str {
        &self.data
    }

    /// Whether the frame carried any `data:` line at all. A keep-alive comment
    /// carries none, and is not an empty event.
    pub fn has_data(&self) -> bool {
        self.has_data
    }

    /// OpenAI's terminator, which is not JSON and must not be parsed as it.
    pub fn is_done(&self) -> bool {
        self.has_data && self.data.trim() == "[DONE]"
    }

    /// The frame's payload as JSON, or `None` when it has none or is not JSON.
    ///
    /// A frame we cannot parse is not an error: the state machine simply has no
    /// opinion about it, and it is relayed untouched.
    pub fn json(&self) -> Option<Value> {
        if !self.has_data || self.is_done() {
            return None;
        }
        serde_json::from_str(&self.data).ok()
    }

    /// Build a frame from bytes the relay is synthesising.
    pub fn synthetic(raw: impl Into<Bytes>) -> SseFrame {
        let raw = raw.into();
        parse(raw)
    }
}

/// Splits a byte stream into frames.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: BytesMut,
}

impl SseDecoder {
    pub fn new() -> SseDecoder {
        SseDecoder::default()
    }

    /// Add bytes as they arrive from the upstream.
    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// The next complete frame, if the bytes so far contain one.
    pub fn next_frame(&mut self) -> Option<SseFrame> {
        let end = frame_end(&self.buf)?;
        let raw = self.buf.split_to(end).freeze();
        Some(parse(raw))
    }

    /// Whatever is left when the upstream closed without a final blank line.
    ///
    /// Providers do terminate their streams properly; a truncated connection
    /// does not, and dropping the bytes silently would lose the last event of
    /// every interrupted stream.
    pub fn flush(&mut self) -> Option<SseFrame> {
        if self.buf.is_empty() {
            return None;
        }
        let raw = self.buf.split().freeze();
        Some(parse(raw))
    }

    /// Bytes held but not yet framed, for the relay's buffer bound.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

/// The offset one past the blank line that ends the first frame, if it is
/// complete.
///
/// Scans line by line rather than searching for a terminator literal because
/// `\n`, `\r\n` and a lone `\r` are all line endings, and a frame ends at the
/// first *empty* line whatever the endings around it look like. A trailing
/// `\r` at the buffer's end is deliberately treated as incomplete: the `\n`
/// that would make it a CRLF may be in the next chunk.
fn frame_end(buf: &[u8]) -> Option<usize> {
    let mut start = 0usize;
    while start < buf.len() {
        let break_at = buf[start..]
            .iter()
            .position(|b| *b == b'\n' || *b == b'\r')
            .map(|offset| start + offset)?;
        let next = if buf[break_at] == b'\n' {
            break_at + 1
        } else if break_at + 1 >= buf.len() {
            return None;
        } else if buf[break_at + 1] == b'\n' {
            break_at + 2
        } else {
            break_at + 1
        };
        if break_at == start {
            return Some(next);
        }
        start = next;
    }
    None
}

pub(crate) fn parse(raw: Bytes) -> SseFrame {
    let mut event = None;
    let mut data_lines: Vec<String> = Vec::new();
    let mut has_data = false;
    let text = String::from_utf8_lossy(&raw).into_owned();

    for line in split_lines(&text) {
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix(':') {
            let _ = comment;
            continue;
        }
        let (field, value) = match line.split_once(':') {
            // a line with no colon is a field with an empty value
            None => (line, ""),
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
        };
        match field {
            "data" => {
                has_data = true;
                data_lines.push(value.to_string());
            }
            "event" => event = Some(value.to_string()),
            _ => {}
        }
    }

    SseFrame {
        raw,
        event,
        data: data_lines.join("\n"),
        has_data,
    }
}

/// Split on any of the three line endings, without allocating a Vec of Strings.
fn split_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive(['\n', '\r'])
        .map(|line| line.trim_end_matches(['\r', '\n']))
}

#[cfg(test)]
mod tests {
    /// An event that never terminates grows without limit.
    ///
    /// `buffered_bytes` in the relay counts complete frames, so bytes sitting
    /// in the decoder before a blank line are outside both
    /// `stream_buffer_bytes` and `stream_intercept_bytes`. `pending()` was
    /// written for exactly that accounting and has no caller anywhere in the
    /// workspace, so nothing bounds this.
    #[test]
    fn an_unterminated_event_accumulates_outside_every_bound() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"data: ");
        for _ in 0..1024 {
            decoder.push(&[b'x'; 1024]);
        }
        assert!(decoder.next_frame().is_none(), "no blank line, no frame");
        assert!(
            decoder.pending() >= 1024 * 1024,
            "the decoder is holding {} bytes with nothing to hand back",
            decoder.pending()
        );
    }

    use super::*;

    fn frames(input: &str) -> Vec<SseFrame> {
        let mut decoder = SseDecoder::new();
        decoder.push(input.as_bytes());
        let mut out = Vec::new();
        while let Some(frame) = decoder.next_frame() {
            out.push(frame);
        }
        out
    }

    #[test]
    fn frames_are_split_on_lf_crlf_and_a_lone_cr_alike() {
        let lf = frames("data: one\n\ndata: two\n\n");
        assert_eq!(lf.len(), 2);
        assert_eq!(lf[0].data(), "one");
        assert_eq!(lf[1].data(), "two");

        let crlf = frames("data: one\r\n\r\ndata: two\r\n\r\n");
        assert_eq!(crlf.len(), 2);
        assert_eq!(crlf[0].data(), "one");
        assert_eq!(crlf[1].data(), "two");

        let mut cr = SseDecoder::new();
        cr.push(b"data: one\r\rdata: two\r\r");
        assert_eq!(
            cr.next_frame().expect("a CR-terminated frame").data(),
            "one"
        );
        assert!(
            cr.next_frame().is_none(),
            "the trailing CR is held: the byte after it may be an LF"
        );
        assert_eq!(cr.flush().expect("the tail").data(), "two");
    }

    #[test]
    fn a_frame_keeps_the_exact_bytes_it_arrived_as() {
        // the relay forwards these bytes; a field we do not understand must
        // survive the trip
        let raw = "event: message_start\r\ndata: {\"a\":1}\r\nid: 42\r\n\r\n";
        let got = frames(raw);
        assert_eq!(got[0].raw().as_ref(), raw.as_bytes());
        assert_eq!(got[0].event(), Some("message_start"));
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        let got = frames("data: first\ndata: second\ndata:third\n\n");
        assert_eq!(got[0].data(), "first\nsecond\nthird");
    }

    #[test]
    fn a_comment_is_a_frame_with_no_data() {
        let got = frames(": ping\n\n");
        assert_eq!(got.len(), 1);
        assert!(!got[0].has_data());
        assert_eq!(got[0].data(), "");
        assert!(got[0].json().is_none());
    }

    #[test]
    fn the_done_marker_is_not_parsed_as_json() {
        let got = frames("data: [DONE]\n\n");
        assert!(got[0].is_done());
        assert!(got[0].json().is_none(), "[DONE] is not a JSON payload");
    }

    #[test]
    fn a_frame_split_across_chunks_is_not_emitted_until_it_is_complete() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"data: {\"a\":");
        assert!(decoder.next_frame().is_none());
        decoder.push(b"1}\r");
        assert!(
            decoder.next_frame().is_none(),
            "a trailing CR may still become a CRLF"
        );
        decoder.push(b"\n\r\n");
        let frame = decoder.next_frame().expect("now complete");
        assert_eq!(frame.json().unwrap()["a"], 1);
    }

    #[test]
    fn a_stream_that_ends_without_a_blank_line_still_yields_its_last_frame() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"data: {\"a\":1}\n");
        assert!(decoder.next_frame().is_none());
        let frame = decoder.flush().expect("a truncated tail is not dropped");
        assert_eq!(frame.json().unwrap()["a"], 1);
        assert!(decoder.flush().is_none());
    }

    #[test]
    fn a_frame_whose_data_is_not_json_is_simply_one_we_have_no_opinion_about() {
        let got = frames("data: not json at all\n\n");
        assert!(got[0].has_data());
        assert!(got[0].json().is_none());
    }
}
