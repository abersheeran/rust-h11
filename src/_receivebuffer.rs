use lazy_static::lazy_static;
use regex::bytes::Regex;
use std::cmp::min;

lazy_static! {
    static ref BLANK_LINE_REGEX: Regex = Regex::new(r"\n\r?\n").unwrap();
}

pub struct ReceiveBuffer {
    data: Vec<u8>,
    next_line_search: usize,
    multiple_lines_search: usize,
}

impl ReceiveBuffer {
    pub fn new() -> Self {
        Self {
            data: vec![],
            next_line_search: 0,
            multiple_lines_search: 0,
        }
    }

    pub fn add(&mut self, byteslike: &[u8]) {
        self.data.extend(byteslike);
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    fn extract(&mut self, count: usize) -> Vec<u8> {
        let out = self.data.drain(..min(count, self.data.len())).collect();
        self.next_line_search = 0;
        self.multiple_lines_search = 0;
        out
    }

    pub fn maybe_extract_at_most(&mut self, count: usize) -> Option<Vec<u8>> {
        if count == 0 || self.data.is_empty() {
            None
        } else {
            Some(self.extract(count))
        }
    }

    pub fn maybe_extract_next_line(&mut self) -> Option<Vec<u8>> {
        let search_start_index = self.next_line_search.saturating_sub(1);
        let needle = b"\r\n";
        let partial_idx = self.data[search_start_index..]
            .windows(needle.len())
            .position(|window| window == needle);
        match partial_idx {
            Some(idx) => Some(self.extract(search_start_index + idx + needle.len())),
            None => {
                self.next_line_search = self.data.len();
                None
            }
        }
    }

    pub fn maybe_extract_lines(&mut self) -> Option<Vec<Vec<u8>>> {
        let lf: &[u8] = b"\n";
        if &self.data[..min(1, self.data.len())] == lf {
            self.extract(1);
            return Some(vec![]);
        }
        let crlf: &[u8] = b"\r\n";
        if &self.data[..min(2, self.data.len())] == crlf {
            self.extract(2);
            return Some(vec![]);
        }
        let match_ = BLANK_LINE_REGEX.find(&self.data[self.multiple_lines_search..]);
        if match_.is_none() {
            self.multiple_lines_search = self.data.len().saturating_sub(2);
            None
        } else {
            let idx = match_.unwrap().end();
            let out = self.extract(idx);
            let mut lines = out
                .split(|&b| b == b'\n')
                .map(|line| {
                    let mut line = line.to_vec();
                    if line.ends_with(&[b'\r']) {
                        line.pop();
                    }
                    line
                })
                .collect::<Vec<_>>();
            assert_eq!(lines[lines.len() - 2], lines[lines.len() - 1]);
            lines.pop();
            lines.pop();
            Some(lines)
        }
    }

    pub fn is_next_line_obviously_invalid_request_line(&self) -> bool {
        if self.data.is_empty() {
            return false;
        }
        self.data[0] < 0x21
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_receivebuffer() {
        let mut b = ReceiveBuffer::new();
        assert_eq!(b.len(), 0);
        assert_eq!(b.extract(0), b"");
        assert_eq!(b.maybe_extract_at_most(10), None);
        assert_eq!(b.maybe_extract_next_line(), None);
        assert_eq!(b.maybe_extract_lines(), None);
        assert!(!b.is_next_line_obviously_invalid_request_line());

        b.add(b"123");
        assert_eq!(b.len(), 3);
        assert_eq!(b.extract(2), b"12");
        assert_eq!(b.len(), 1);
        assert_eq!(b.extract(1), b"3");
        assert_eq!(b.len(), 0);
        assert_eq!(b.maybe_extract_at_most(10), None);
        assert_eq!(b.maybe_extract_next_line(), None);
        assert_eq!(b.maybe_extract_lines(), None);
        assert!(!b.is_next_line_obviously_invalid_request_line());
    }

    #[test]
    fn test_receivebuffer_maybe_extract_until_next() {
        let mut b = ReceiveBuffer::new();
        b.add(b"123\n456\r\n789\r\n");
        assert_eq!(b.maybe_extract_next_line(), Some(b"123\n456\r\n".to_vec()));
        assert_eq!(b.maybe_extract_next_line(), Some(b"789\r\n".to_vec()));
        assert_eq!(b.maybe_extract_next_line(), None);
        assert_eq!(b.maybe_extract_lines(), None);
        assert!(!b.is_next_line_obviously_invalid_request_line());

        b.add(b"12\r");
        assert_eq!(b.maybe_extract_next_line(), None);
        assert_eq!(b.maybe_extract_lines(), None);
        assert!(!b.is_next_line_obviously_invalid_request_line());

        b.add(b"345\n\r");
        assert_eq!(b.maybe_extract_next_line(), None);
        assert_eq!(b.maybe_extract_lines(), None);
        assert!(!b.is_next_line_obviously_invalid_request_line());

        b.add(b"\n6789aaa123\r\n");
        assert_eq!(b.maybe_extract_next_line(), Some(b"12\r345\n\r\n".to_vec()));
        assert_eq!(
            b.maybe_extract_next_line(),
            Some(b"6789aaa123\r\n".to_vec())
        );
        assert_eq!(b.maybe_extract_next_line(), None);
        assert_eq!(b.maybe_extract_lines(), None);
        assert!(!b.is_next_line_obviously_invalid_request_line());
    }

    #[test]
    fn test_receivebuffer_maybe_extract_lines() {
        let mut b = ReceiveBuffer::new();
        b.add(b"123\r\na: b\r\nfoo:bar\r\n\r\ntrailing");
        let lines = b.maybe_extract_lines();
        assert_eq!(
            lines,
            Some(vec![b"123".to_vec(), b"a: b".to_vec(), b"foo:bar".to_vec()])
        );
        assert_eq!(b.maybe_extract_lines(), None);
        assert!(!b.is_next_line_obviously_invalid_request_line());

        b.add(b"\r\n\r");
        assert_eq!(b.maybe_extract_lines(), None);
        assert!(!b.is_next_line_obviously_invalid_request_line());

        assert_eq!(
            b.maybe_extract_at_most(100),
            Some(b"trailing\r\n\r".to_vec())
        );
        assert_eq!(b.maybe_extract_at_most(100), None);
        assert!(!b.is_next_line_obviously_invalid_request_line());

        // Empty body case (as happens at the end of chunked encoding if there are
        // no trailing headers, e.g.)
        b.add(b"\r\ntrailing");
        assert_eq!(b.maybe_extract_lines(), Some(vec![]));
        assert_eq!(b.maybe_extract_lines(), None);
        assert!(!b.is_next_line_obviously_invalid_request_line());
    }

    #[test]
    fn test_receivebuffer_for_invalid_delimiter() {
        let mut b = ReceiveBuffer::new();

        b.add(b"HTTP/1.1 200 OK\r\n");
        b.add(b"Content-type: text/plain\r\n");
        b.add(b"Connection: close\r\n");
        b.add(b"\r\n");
        b.add(b"Some body");

        let lines = b.maybe_extract_lines();

        assert_eq!(
            lines,
            Some(vec![
                b"HTTP/1.1 200 OK".to_vec(),
                b"Content-type: text/plain".to_vec(),
                b"Connection: close".to_vec(),
            ])
        );
        assert_eq!(b.data, b"Some body");
    }
}
