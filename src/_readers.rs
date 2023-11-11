use crate::{
    _abnf::{HEADER_FIELD, REQUEST_LINE, STATUS_LINE},
    _events::{ConnectionClosed, Data, EndOfMessage, Event, Request, Response},
    _headers::normalize_and_validate,
    _receivebuffer::ReceiveBuffer,
    _util::ProtocolError,
};
use lazy_static::lazy_static;
use regex::bytes::Regex;

lazy_static! {
    static ref HEADER_FIELD_RE: Regex = Regex::new(&format!(r"^{}$", *HEADER_FIELD)).unwrap();
    static ref OBS_FOLD_RE: Regex = Regex::new(r"[ \t]+").unwrap();
}

fn _obsolete_line_fold(lines: Vec<&[u8]>) -> Result<Vec<Vec<u8>>, ProtocolError> {
    let mut out = vec![];
    let mut it = lines.iter();
    let mut last: Option<Vec<u8>> = None;
    while let Some(line) = it.next() {
        let match_ = OBS_FOLD_RE.find(line);
        if let Some(match_) = match_ {
            if last.is_none() {
                return Err(ProtocolError::LocalProtocolError(
                    "continuation line at start of headers".into(),
                ));
            }
            if let Some(last) = last.as_mut() {
                last.extend_from_slice(b" ");
                last.extend_from_slice(&line[match_.end()..]);
            }
        } else {
            if let Some(last) = last.take() {
                out.push(last);
            }
            last = Some(line.to_vec());
        }
    }
    if let Some(last) = last.take() {
        out.push(last);
    }
    Ok(out)
}

fn _decode_header_lines(lines: Vec<Vec<u8>>) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ProtocolError> {
    let lines = _obsolete_line_fold(lines.iter().map(|line| line.as_slice()).collect())?;
    let mut out = vec![];
    for line in lines {
        let matches = match HEADER_FIELD_RE.captures(&line) {
            Some(matches) => matches,
            None => {
                return Err(ProtocolError::LocalProtocolError(
                    format!("illegal header line {:?}", &line).into(),
                ))
            }
        };
        out.push((
            matches["field_name"].to_vec(),
            matches["field_value"].to_vec(),
        ));
    }
    Ok(out)
}

lazy_static! {
    static ref REQUEST_LINE_RE: Regex = Regex::new(&REQUEST_LINE).unwrap();
}

pub trait Reader {
    fn call(&mut self, buf: &mut ReceiveBuffer) -> Result<Option<Event>, ProtocolError>;
    fn read_eof(&self) -> Result<Event, ProtocolError> {
        Ok(ConnectionClosed::default().into())
    }
}

pub struct IdleClientReader {}

impl Reader for IdleClientReader {
    fn call(&mut self, buf: &mut ReceiveBuffer) -> Result<Option<Event>, ProtocolError> {
        let lines = buf.maybe_extract_lines();
        if lines.is_none() {
            if buf.is_next_line_obviously_invalid_request_line() {
                return Err(ProtocolError::LocalProtocolError(
                    ("illegal request line".to_string(), 400).into(),
                ));
            }
            return Ok(None);
        }
        let lines = lines.unwrap();
        if lines.is_empty() {
            return Err(ProtocolError::LocalProtocolError(
                ("no request line received".to_string(), 400).into(),
            ));
        }
        let matches = match REQUEST_LINE_RE.captures(&lines[0]) {
            Some(matches) => matches,
            None => {
                return Err(ProtocolError::LocalProtocolError(
                    format!("illegal request line {:?}", &lines[0]).into(),
                ))
            }
        };

        let headers = normalize_and_validate(_decode_header_lines(lines[1..].to_vec())?, true)?;

        Ok(Some(
            Request::new(
                matches["method"].to_vec(),
                headers,
                matches["target"].to_vec(),
                matches["http_version"].to_vec(),
            )?
            .into(),
        ))
    }
}

lazy_static! {
    static ref STATUS_LINE_RE: Regex = Regex::new(&STATUS_LINE).unwrap();
}

pub struct SendResponseServerReader {}

impl Reader for SendResponseServerReader {
    fn call(&mut self, buf: &mut ReceiveBuffer) -> Result<Option<Event>, ProtocolError> {
        let lines = buf.maybe_extract_lines();
        if lines.is_none() {
            if buf.is_next_line_obviously_invalid_request_line() {
                return Err(ProtocolError::LocalProtocolError(
                    ("illegal request line".to_string(), 400).into(),
                ));
            }
            return Ok(None);
        }
        let lines = lines.unwrap();
        if lines.is_empty() {
            return Err(ProtocolError::LocalProtocolError(
                ("no response line received".to_string(), 400).into(),
            ));
        }
        let matches = match STATUS_LINE_RE.captures(&lines[0]) {
            Some(matches) => matches,
            None => {
                return Err(ProtocolError::LocalProtocolError(
                    format!("illegal response line {:?}", &lines[0]).into(),
                ))
            }
        };
        let http_version = matches["http_version"].to_vec();
        let reason = matches["reason"].to_vec();
        let status_code: u16 = match std::str::from_utf8(&matches["status_code"].to_vec()) {
            Ok(status_code) => match status_code.parse() {
                Ok(status_code) => status_code,
                Err(_) => {
                    return Err(ProtocolError::LocalProtocolError(
                        ("illegal status code".to_string(), 400).into(),
                    ))
                }
            },
            Err(_) => {
                return Err(ProtocolError::LocalProtocolError(
                    ("illegal status code".to_string(), 400).into(),
                ))
            }
        };
        let headers = normalize_and_validate(_decode_header_lines(lines[1..].to_vec())?, true)?;

        return Ok(Some(Event::from(Response {
            headers,
            http_version,
            reason,
            status_code,
        })));
    }
}

pub struct ContentLengthReader {
    length: usize,
    remaining: usize,
}

impl ContentLengthReader {
    pub fn new(length: usize) -> Self {
        Self {
            length,
            remaining: length,
        }
    }
}

impl Reader for ContentLengthReader {
    fn call(&mut self, buf: &mut ReceiveBuffer) -> Result<Option<Event>, ProtocolError> {
        if self.remaining == 0 {
            return Ok(Some(EndOfMessage::default().into()));
        }
        match buf.maybe_extract_at_most(self.remaining) {
            Some(data) => {
                self.remaining -= data.len();
                Ok(Some(
                    Data {
                        data,
                        chunk_start: false,
                        chunk_end: false,
                    }
                    .into(),
                ))
            }
            None => Ok(None),
        }
    }

    fn read_eof(&self) -> Result<Event, ProtocolError> {
        Err(ProtocolError::RemoteProtocolError(
            (
                format!(
                    "peer closed connection without sending complete message body \
                (received {} bytes, expected {})",
                    self.length - self.remaining,
                    self.length
                ),
                400,
            )
                .into(),
        ))
    }
}

lazy_static! {
    static ref CHUNK_HEADER_RE: Regex = Regex::new(r"([0-9A-Fa-f]){1,20}").unwrap();
}

pub struct ChunkedReader {
    bytes_in_chunk: usize,
    bytes_to_discard: usize,
    reading_trailer: bool,
}

impl ChunkedReader {
    pub fn new() -> Self {
        Self {
            bytes_in_chunk: 0,
            bytes_to_discard: 0,
            reading_trailer: false,
        }
    }
}

impl Reader for ChunkedReader {
    fn call(&mut self, buf: &mut ReceiveBuffer) -> Result<Option<Event>, ProtocolError> {
        if self.reading_trailer {
            match buf.maybe_extract_lines() {
                Some(lines) => {
                    return Ok(Some(
                        EndOfMessage {
                            headers: normalize_and_validate(
                                _decode_header_lines(lines[1..].to_vec())?,
                                true,
                            )?,
                        }
                        .into(),
                    ))
                }
                None => return Ok(None),
            }
        }
        if self.bytes_to_discard > 0 {
            let data = buf.maybe_extract_at_most(self.bytes_to_discard);
            if data.is_none() {
                return Ok(None);
            }
            self.bytes_to_discard -= data.unwrap().len();
            if self.bytes_to_discard > 0 {
                return Ok(None);
            }
        }
        assert_eq!(self.bytes_to_discard, 0);
        if self.bytes_in_chunk == 0 {
            if let Some(chunk_header) = buf.maybe_extract_next_line() {
                let matches = CHUNK_HEADER_RE.find(&chunk_header).unwrap();
                self.bytes_in_chunk = usize::from_str_radix(
                    std::str::from_utf8(&matches.as_bytes().to_vec()).unwrap(),
                    16,
                )
                .unwrap();
                if self.bytes_in_chunk == 0 {
                    self.reading_trailer = true;
                    return self.call(buf);
                }
            }
            return Ok(None);
        }
        let chunk_start = self.bytes_in_chunk == 0;
        assert!(self.bytes_in_chunk > 0);

        if let Some(data) = buf.maybe_extract_at_most(self.bytes_in_chunk) {
            self.bytes_in_chunk -= data.len();
            if self.bytes_in_chunk == 0 {
                self.bytes_to_discard = 2;
            }
            let chunk_end = self.bytes_in_chunk == 0;
            Ok(Some(
                Data {
                    data,
                    chunk_start,
                    chunk_end,
                }
                .into(),
            ))
        } else {
            Ok(None)
        }
    }

    fn read_eof(&self) -> Result<Event, ProtocolError> {
        Err(ProtocolError::RemoteProtocolError(
            (
                "peer closed connection without sending complete message body \
            (incomplete chunked read)"
                    .to_string(),
                400,
            )
                .into(),
        ))
    }
}

pub struct Http10Reader {}

impl Reader for Http10Reader {
    fn call(&mut self, buf: &mut ReceiveBuffer) -> Result<Option<Event>, ProtocolError> {
        let data = buf.maybe_extract_at_most(999999999);
        match data {
            Some(data) => Ok(Some(
                Data {
                    data,
                    chunk_start: false,
                    chunk_end: false,
                }
                .into(),
            )),
            None => Ok(None),
        }
    }

    fn read_eof(&self) -> Result<Event, ProtocolError> {
        Ok(EndOfMessage::default().into())
    }
}

pub struct ClosedReader {}

impl Reader for ClosedReader {
    fn call(&mut self, buf: &mut ReceiveBuffer) -> Result<Option<Event>, ProtocolError> {
        if buf.len() > 0 {
            return Err(ProtocolError::LocalProtocolError(
                ("unexpected data".to_string(), 400).into(),
            ));
        }
        Ok(None)
    }
}
