use crate::{
    _abnf::{CHUNK_HEADER, HEADER_FIELD, REQUEST_LINE, STATUS_LINE},
    _events::{ConnectionClosed, Data, EndOfMessage, Event, Request, Response},
    _headers::normalize_and_validate,
    _receivebuffer::ReceiveBuffer,
    _util::ProtocolError,
};
use lazy_static::lazy_static;
use regex::bytes::Regex;

lazy_static! {
    static ref HEADER_FIELD_RE: Regex = Regex::new(&format!(r"^{}$", *HEADER_FIELD)).unwrap();
    static ref OBS_FOLD_RE: Regex = Regex::new(r"^[ \t]+").unwrap();
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

#[derive(Clone)]
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

#[derive(Clone)]
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

#[derive(Clone)]
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
    static ref CHUNK_HEADER_RE: Regex = Regex::new(&CHUNK_HEADER).unwrap();
}

#[derive(Clone)]
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
                            headers: normalize_and_validate(_decode_header_lines(lines)?, true)?,
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
        let chunk_start: bool;
        if self.bytes_in_chunk == 0 {
            if let Some(chunk_header) = buf.maybe_extract_next_line() {
                let matches = match CHUNK_HEADER_RE.captures(&chunk_header) {
                    Some(matches) => matches,
                    None => {
                        return Err(ProtocolError::LocalProtocolError(
                            format!("illegal chunk header: {:?}", &chunk_header).into(),
                        ))
                    }
                };
                self.bytes_in_chunk = match usize::from_str_radix(
                    std::str::from_utf8(&matches["chunk_size"].to_vec()).unwrap(),
                    16,
                ) {
                    Ok(bytes_in_chunk) => bytes_in_chunk,
                    Err(_) => {
                        return Err(ProtocolError::LocalProtocolError(
                            format!("illegal chunk size: {:?}", &matches["chunk_size"]).into(),
                        ))
                    }
                };
                if self.bytes_in_chunk == 0 {
                    self.reading_trailer = true;
                    return self.call(buf);
                }
            } else {
                return Ok(None);
            }
            chunk_start = true;
        } else {
            chunk_start = false;
        }
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

#[derive(Clone)]
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

#[derive(Clone)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_obsolete_line_fold_bytes() {
        assert_eq!(
            _obsolete_line_fold(vec![
                b"aaa".as_ref(),
                b"bbb".as_ref(),
                b"  ccc".as_ref(),
                b"ddd".as_ref()
            ])
            .unwrap(),
            vec![b"aaa".to_vec(), b"bbb ccc".to_vec(), b"ddd".to_vec()]
        );
    }

    fn normalize_data_events(in_events: Vec<Event>) -> Vec<Event> {
        let mut out_events = Vec::new();
        for in_event in in_events {
            let event = match in_event {
                Event::Data(data) => Event::Data(Data {
                    data: data.data.clone(),
                    chunk_start: false,
                    chunk_end: false,
                }),
                _ => in_event.clone(),
            };
            if !out_events.is_empty() {
                if let Event::Data(data_event) = &event {
                    if let Event::Data(last_data_event) = out_events.last().unwrap() {
                        let mut x = last_data_event.clone();
                        x.data.extend_from_slice(&data_event.data);
                        let l = out_events.len();
                        out_events[l - 1] = Event::Data(x);
                        continue;
                    }
                }
            }
            out_events.push(event);
        }
        return out_events;
    }

    fn _run_reader(
        reader: &mut impl Reader,
        buf: &mut ReceiveBuffer,
        do_eof: bool,
    ) -> Result<Vec<Event>, ProtocolError> {
        let mut events = vec![];
        {
            loop {
                match reader.call(buf)? {
                    Some(event) => {
                        events.push(event.clone());
                        if let Event::EndOfMessage(_) = event {
                            break;
                        }
                    }
                    None => break,
                }
            }
            if do_eof {
                assert!(buf.len() == 0);
                events.push(reader.read_eof().unwrap());
            }
        }
        return Ok(normalize_data_events(events));
    }

    fn t_body_reader(
        reader: impl Reader + Clone,
        data: &[u8],
        expected: Vec<Event>,
        do_eof: bool,
    ) -> Result<(), ProtocolError> {
        assert_eq!(
            _run_reader(
                &mut reader.clone(),
                &mut ReceiveBuffer::from(data.to_vec()),
                do_eof
            )?,
            expected
        );

        let mut buf = ReceiveBuffer::new();
        let mut events = vec![];
        let mut r1 = reader.clone();
        for i in 0..data.len() {
            events.append(&mut _run_reader(&mut r1, &mut buf, false)?);
            buf.add(&mut data[i..i + 1].to_vec());
        }
        events.append(&mut _run_reader(&mut r1, &mut buf, do_eof)?);
        assert_eq!(normalize_data_events(events.clone()), expected);

        if expected.iter().any(|event| match event {
            Event::EndOfMessage(_) => true,
            _ => false,
        }) && !do_eof
        {
            assert_eq!(
                _run_reader(
                    &mut reader.clone(),
                    &mut ReceiveBuffer::from(data.to_vec()),
                    false
                )?,
                expected.clone()
            );
        }
        Ok(())
    }

    #[test]
    fn test_content_length_reader() {
        t_body_reader(
            ContentLengthReader::new(0),
            b"",
            vec![EndOfMessage::default().into()],
            false,
        )
        .unwrap();

        t_body_reader(
            ContentLengthReader::new(10),
            b"0123456789",
            vec![
                Data {
                    data: b"0123456789".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }
                .into(),
                EndOfMessage::default().into(),
            ],
            false,
        )
        .unwrap();
    }

    #[test]
    fn test_http10_reader() {
        t_body_reader(
            Http10Reader {},
            b"",
            vec![EndOfMessage::default().into()],
            true,
        )
        .unwrap();

        t_body_reader(
            Http10Reader {},
            b"asdf",
            vec![Data {
                data: b"asdf".to_vec(),
                chunk_start: false,
                chunk_end: false,
            }
            .into()],
            false,
        )
        .unwrap();

        t_body_reader(
            Http10Reader {},
            b"asdf",
            vec![
                Data {
                    data: b"asdf".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }
                .into(),
                EndOfMessage::default().into(),
            ],
            true,
        )
        .unwrap();
    }

    #[test]
    fn test_chunked_reader() {
        t_body_reader(
            ChunkedReader::new(),
            b"0\r\n\r\n",
            vec![EndOfMessage::default().into()],
            false,
        )
        .unwrap();

        t_body_reader(
            ChunkedReader::new(),
            b"0\r\nSome: header\r\n\r\n",
            vec![EndOfMessage {
                headers: vec![(b"Some".to_vec(), b"header".to_vec())].into(),
            }
            .into()],
            false,
        )
        .unwrap();

        t_body_reader(
            ChunkedReader::new(),
            b"5\r\n01234\r\n10\r\n0123456789abcdef\r\n0\r\nSome: header\r\n\r\n",
            vec![
                Data {
                    data: b"012340123456789abcdef".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }
                .into(),
                EndOfMessage {
                    headers: vec![(b"Some".to_vec(), b"header".to_vec())].into(),
                }
                .into(),
            ],
            false,
        )
        .unwrap();

        t_body_reader(
            ChunkedReader::new(),
            b"5\r\n01234\r\n10\r\n0123456789abcdef\r\n0\r\n\r\n",
            vec![
                Data {
                    data: b"012340123456789abcdef".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }
                .into(),
                EndOfMessage::default().into(),
            ],
            false,
        )
        .unwrap();

        // handles upper and lowercase hex
        t_body_reader(
            ChunkedReader::new(),
            &[
                b"aA\r\n".to_vec(),
                vec![120; 0xAA],
                b"\r\n".to_vec(),
                b"0\r\n\r\n".to_vec(),
            ]
            .concat(),
            vec![
                Data {
                    data: vec![120; 0xAA],
                    chunk_start: false,
                    chunk_end: false,
                }
                .into(),
                EndOfMessage::default().into(),
            ],
            false,
        )
        .unwrap();

        // refuses arbitrarily long chunk integers
        assert!(t_body_reader(
            ChunkedReader::new(),
            &[vec![57; 100], b"\r\nxxx".to_vec()].concat(),
            vec![Data {
                data: b"xxx".to_vec(),
                chunk_start: false,
                chunk_end: false,
            }
            .into()],
            false,
        )
        .is_err());

        // refuses garbage in the chunk count
        assert!(t_body_reader(ChunkedReader::new(), b"10\x00\r\nxxx", vec![], false,).is_err());

        // handles (and discards) "chunk extensions" omg wtf
        t_body_reader(
            ChunkedReader::new(),
            b"5; hello=there\r\nxxxxx\r\n0; random=\"junk\"; some=more; canbe=lonnnnngg\r\n\r\n",
            vec![
                Data {
                    data: b"xxxxx".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }
                .into(),
                EndOfMessage::default().into(),
            ],
            false,
        )
        .unwrap();

        t_body_reader(
            ChunkedReader::new(),
            b"5   	 \r\n01234\r\n0\r\n\r\n",
            vec![
                Data {
                    data: b"01234".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }
                .into(),
                EndOfMessage::default().into(),
            ],
            false,
        )
        .unwrap();
    }
}
