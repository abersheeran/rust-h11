use crate::{
    _abnf::{FIELD_NAME, FIELD_VALUE},
    _events::Request,
    _util::ProtocolError,
};
use lazy_static::lazy_static;
use regex::bytes::Regex;

lazy_static! {
    static ref CONTENT_LENGTH_RE: Regex = Regex::new(r"[0-9]+").unwrap();
    static ref FIELD_NAME_RE: Regex = Regex::new(&FIELD_NAME).unwrap();
    static ref FIELD_VALUE_RE: Regex = Regex::new(&FIELD_VALUE).unwrap();
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Headers(Vec<(Vec<u8>, Vec<u8>, Vec<u8>)>);

impl Headers {
    pub fn iter(&self) -> impl Iterator<Item = (Vec<u8>, Vec<u8>)> + '_ {
        self.0
            .iter()
            .map(|(_, name, value)| ((*name).clone(), (*value).clone()))
    }

    pub fn raw_items(&self) -> Vec<&(Vec<u8>, Vec<u8>, Vec<u8>)> {
        self.0.iter().collect()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

pub fn normalize_and_validate(
    headers: Vec<(Vec<u8>, Vec<u8>)>,
    _parsed: bool,
) -> Result<Headers, ProtocolError> {
    let mut new_headers = vec![];
    let mut seen_content_length = None;
    let mut saw_transfer_encoding = false;
    for (name, value) in headers.iter() {
        if !_parsed {
            if !FIELD_NAME_RE.is_match(&name) {
                return Err(ProtocolError::LocalProtocolError(
                    (format!("Illegal header name {:?}", &name), 400).into(),
                ));
            }
            if !FIELD_VALUE_RE.is_match(&value) {
                return Err(ProtocolError::LocalProtocolError(
                    (format!("Illegal header value {:?}", &value), 400).into(),
                ));
            }
        }
        let raw_name = (*name).clone();
        let name = name.to_ascii_lowercase();
        if name == b"content-length" {
            let lengths: Vec<Vec<u8>> = value
                .split(|&b| b == b',')
                .map(|length| {
                    std::str::from_utf8(length)
                        .unwrap()
                        .trim()
                        .as_bytes()
                        .to_vec()
                })
                .collect();
            if lengths.len() != 1 {
                return Err(ProtocolError::LocalProtocolError(
                    ("conflicting Content-Length headers".to_string(), 400).into(),
                ));
            }
            if !CONTENT_LENGTH_RE.is_match(&lengths[0]) {
                return Err(ProtocolError::LocalProtocolError(
                    ("bad Content-Length".to_string(), 400).into(),
                ));
            }
            let value = lengths[0].clone();
            if seen_content_length.is_none() {
                seen_content_length = Some(value.clone());
                new_headers.push((raw_name, name, value));
            } else if seen_content_length != Some(value.clone()) {
                return Err(ProtocolError::LocalProtocolError(
                    ("conflicting Content-Length headers".to_string(), 400).into(),
                ));
            }
        } else if name == b"transfer-encoding" {
            // "A server that receives a request message with a transfer coding
            // it does not understand SHOULD respond with 501 (Not
            // Implemented)."
            // https://tools.ietf.org/html/rfc7230#section-3.3.1
            if saw_transfer_encoding {
                return Err(ProtocolError::LocalProtocolError(
                    ("multiple Transfer-Encoding headers".to_string(), 501).into(),
                ));
            }
            // "All transfer-coding names are case-insensitive"
            // -- https://tools.ietf.org/html/rfc7230#section-4
            let value = value.to_ascii_lowercase();
            if value != b"chunked" {
                return Err(ProtocolError::LocalProtocolError(
                    (
                        "Only Transfer-Encoding: chunked is supported".to_string(),
                        501,
                    )
                        .into(),
                ));
            }
            saw_transfer_encoding = true;
            new_headers.push((raw_name, name, value));
        } else {
            new_headers.push((raw_name, name, value.to_vec()));
        }
    }

    Ok(Headers(new_headers))
}

pub fn get_comma_header(headers: &Headers, name: Vec<u8>) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = vec![];
    for (found_name, found_value) in headers.iter() {
        if found_name == name {
            for found_split_value in found_value.split(|&b| b == b',') {
                let found_split_value = std::str::from_utf8(found_split_value).unwrap().trim();
                if !found_split_value.is_empty() {
                    out.push(found_split_value.as_bytes().to_vec());
                }
            }
        }
    }
    out
}

pub fn set_comma_header(
    headers: &Headers,
    name: Vec<u8>,
    new_values: Vec<Vec<u8>>,
) -> Result<Headers, ProtocolError> {
    let mut new_headers = vec![];
    for (found_name, found_value) in headers.iter() {
        if found_name != name {
            new_headers.push((found_name, found_value));
        }
    }
    for new_value in new_values {
        new_headers.push((name.clone(), new_value));
    }
    normalize_and_validate(new_headers, false)
}

pub fn has_expect_100_continue(request: &Request) -> bool {
    // https://tools.ietf.org/html/rfc7231#section-5.1.1
    // "A server that receives a 100-continue expectation in an HTTP/1.0 request
    // MUST ignore that expectation."
    if request.http_version < b"1.1".to_vec() {
        return false;
    }
    let expect = get_comma_header(&request.headers, b"expect".to_vec());
    expect.contains(&b"100-continue".to_vec())
}
