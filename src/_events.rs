use crate::_abnf::{METHOD, REQUEST_TARGET};
use crate::{_headers::Headers, _util::ProtocolError};
use lazy_static::lazy_static;
use regex::bytes::Regex;
use std::fmt::{self, Formatter};

lazy_static! {
    static ref METHOD_RE: Regex = Regex::new(&format!(r"^{}$", *METHOD)).unwrap();
    static ref REQUEST_TARGET_RE: Regex = Regex::new(&format!(r"^{}$", *REQUEST_TARGET)).unwrap();
}

#[derive(Clone, PartialEq, Eq, Default)]
pub struct Request {
    pub method: Vec<u8>,
    pub headers: Headers,
    pub target: Vec<u8>,
    pub http_version: Vec<u8>,
}

impl Request {
    pub fn new(
        method: Vec<u8>,
        headers: Headers,
        target: Vec<u8>,
        http_version: Vec<u8>,
    ) -> Result<Self, ProtocolError> {
        let mut host_count = 0;
        for (name, _) in headers.iter() {
            if name == b"host" {
                host_count += 1;
            }
        }
        if http_version == b"1.1" && host_count == 0 {
            return Err(ProtocolError::LocalProtocolError(
                ("Missing mandatory Host: header".to_string(), 400).into(),
            ));
        }
        if host_count > 1 {
            return Err(ProtocolError::LocalProtocolError(
                ("Found multiple Host: headers".to_string(), 400).into(),
            ));
        }

        if !METHOD_RE.is_match(&method) {
            return Err(ProtocolError::LocalProtocolError(
                ("Illegal method characters".to_string(), 400).into(),
            ));
        }
        if !REQUEST_TARGET_RE.is_match(&target) {
            return Err(ProtocolError::LocalProtocolError(
                ("Illegal target characters".to_string(), 400).into(),
            ));
        }

        Ok(Self {
            method,
            headers,
            target,
            http_version,
        })
    }
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("method", &String::from_utf8_lossy(&self.method))
            .field("headers", &self.headers)
            .field("target", &String::from_utf8_lossy(&self.target))
            .field("http_version", &String::from_utf8_lossy(&self.http_version))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Response {
    pub headers: Headers,
    pub http_version: Vec<u8>,
    pub reason: Vec<u8>,
    pub status_code: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Data {
    pub data: Vec<u8>,
    pub chunk_start: bool,
    pub chunk_end: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EndOfMessage {
    pub headers: Headers,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnectionClosed {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Request(Request),
    NormalResponse(Response),
    InformationalResponse(Response),
    Data(Data),
    EndOfMessage(EndOfMessage),
    ConnectionClosed(ConnectionClosed),
    NeedData(),
    Paused(),
}

impl From<Request> for Event {
    fn from(request: Request) -> Self {
        Self::Request(request)
    }
}

impl From<Response> for Event {
    fn from(response: Response) -> Self {
        match response.status_code {
            100..=199 => Self::InformationalResponse(response),
            _ => Self::NormalResponse(response),
        }
    }
}

impl From<Data> for Event {
    fn from(data: Data) -> Self {
        Self::Data(data)
    }
}

impl From<EndOfMessage> for Event {
    fn from(end_of_message: EndOfMessage) -> Self {
        Self::EndOfMessage(end_of_message)
    }
}

impl From<ConnectionClosed> for Event {
    fn from(connection_closed: ConnectionClosed) -> Self {
        Self::ConnectionClosed(connection_closed)
    }
}
