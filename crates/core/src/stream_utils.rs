//! Shared streaming utilities.
//!
//! Incremental UTF-8 decoding across network chunk boundaries and the SSE
//! message boundary scanner (previously duplicated between the Anthropic and
//! OpenAI-compatible parsers).

/// Incremental UTF-8 decoder for network chunk boundaries.
///
/// Holds back an incomplete trailing multibyte prefix instead of emitting
/// U+FFFD per chunk (what `String::from_utf8_lossy` would do). A multibyte
/// character split across two TCP chunks is reassembled before decoding.
#[derive(Debug, Default)]
pub struct Utf8ChunkDecoder {
    pending: Vec<u8>,
}

impl Utf8ChunkDecoder {
    /// Create a new decoder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Feed raw bytes; returns the safely decoded fragment.
    ///
    /// Incomplete trailing byte sequences are retained for the next push.
    /// Invalid (not merely incomplete) sequences are lossy-decoded so the
    /// decoder can never wedge on garbage input.
    pub fn push(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        self.drain_valid()
    }

    /// End of stream: lossy-decode any stranded bytes.
    ///
    /// A genuinely truncated final byte correctly becomes U+FFFD here.
    pub fn finish(&mut self) -> String {
        let out = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        out
    }

    /// Decode the valid UTF-8 prefix of `pending`, keeping any incomplete
    /// trailing sequence buffered. Invalid sequences are lossy-emitted.
    fn drain_valid(&mut self) -> String {
        match std::str::from_utf8(&self.pending) {
            Ok(s) => {
                let out = s.to_string();
                self.pending.clear();
                out
            }
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                let mut out = String::from_utf8_lossy(&self.pending[..valid_up_to]).into_owned();
                match e.error_len() {
                    // Incomplete trailing sequence: keep it buffered.
                    None => {
                        self.pending.drain(..valid_up_to);
                    }
                    // Genuinely invalid bytes: emit them lossily and continue
                    // decoding after them.
                    Some(bad_len) => {
                        out.push_str(&String::from_utf8_lossy(
                            &self.pending[valid_up_to..valid_up_to + bad_len],
                        ));
                        self.pending.drain(..valid_up_to + bad_len);
                        out.push_str(&self.drain_valid());
                    }
                }
                out
            }
        }
    }
}

/// Find the position of the next SSE message boundary (`\n\n` or `\r\n\r\n`).
///
/// The returned index includes one trailing delimiter character run so the
/// caller can `drain(..=pos)` past it.
#[must_use]
pub fn find_sse_message_boundary(buffer: &str) -> Option<usize> {
    // Check for \r\n\r\n first (CRLF, used by some proxies/servers)
    if let Some(pos) = buffer.find("\r\n\r\n") {
        return Some(pos + 3); // include one CRLF
    }
    // Fall back to \n\n (LF, standard SSE)
    buffer
        .as_bytes()
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|pos| pos + 1) // include one newline
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_multibyte_char_across_chunks() {
        let mut d = Utf8ChunkDecoder::new();
        let bytes = "你好".as_bytes();
        // Split mid-character: 你 is 3 bytes (E4 BD A0).
        let head = d.push(&bytes[..2]);
        assert_eq!(head, "");
        let rest = d.push(&bytes[2..]);
        assert_eq!(rest, "你好");
    }

    #[test]
    fn finish_lossy_on_truncated_tail() {
        let mut d = Utf8ChunkDecoder::new();
        let bytes = "ok你".as_bytes();
        let out = d.push(&bytes[..4]); // "ok" + 2 bytes of 你
        assert_eq!(out, "ok");
        let tail = d.finish();
        assert_eq!(tail.chars().count(), 1); // U+FFFD
        assert_eq!(tail, String::from_utf8_lossy(&bytes[2..3]));
    }

    #[test]
    fn ascii_passthrough() {
        let mut d = Utf8ChunkDecoder::new();
        assert_eq!(d.push(b"hello "), "hello ");
        assert_eq!(d.push(b"world"), "world");
        assert_eq!(d.finish(), "");
    }

    #[test]
    fn invalid_bytes_do_not_wedge() {
        let mut d = Utf8ChunkDecoder::new();
        let out = d.push(&[0xFF, b'a', b'b']);
        assert!(out.contains("ab"));
        assert_eq!(d.finish(), "");
    }

    #[test]
    fn boundary_variants() {
        // "data: x\n\n..." — LF boundary at index 7, include one newline → 8.
        assert_eq!(find_sse_message_boundary("data: x\n\nevent: y"), Some(8));
        // "data: x\r\n\r\n..." — CRLF boundary at index 7, include one CRLF → 10.
        assert_eq!(find_sse_message_boundary("data: x\r\n\r\n"), Some(10));
        assert_eq!(find_sse_message_boundary("no boundary"), None);
    }
}
