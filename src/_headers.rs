use std::collections::HashSet;

use crate::{
    _abnf::{FIELD_NAME, FIELD_VALUE},
    _events::Request,
    _util::ProtocolError,
};
use lazy_static::lazy_static;
use regex::bytes::Regex;

lazy_static! {
    static ref CONTENT_LENGTH_RE: Regex = Regex::new(r"^[0-9]+$").unwrap();
    static ref FIELD_NAME_RE: Regex = Regex::new(&format!(r"^{}$", FIELD_NAME)).unwrap();
    static ref FIELD_VALUE_RE: Regex = Regex::new(&format!(r"^{}$", *FIELD_VALUE)).unwrap();
}

#[derive(Clone, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Headers(Vec<(Vec<u8>, Vec<u8>, Vec<u8>)>);

impl std::fmt::Debug for Headers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug_struct = f.debug_struct("Headers");
        self.0.iter().for_each(|(raw_name, _, value)| {
            debug_struct.field(
                std::str::from_utf8(raw_name).unwrap(),
                &std::str::from_utf8(value).unwrap(),
            );
        });
        debug_struct.finish()
    }
}

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

impl From<Vec<(Vec<u8>, Vec<u8>)>> for Headers {
    fn from(value: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        normalize_and_validate(value, false).unwrap()
    }
}

pub fn normalize_and_validate(
    headers: Vec<(Vec<u8>, Vec<u8>)>,
    _parsed: bool,
) -> Result<Headers, ProtocolError> {
    let mut new_headers = vec![];
    let mut seen_content_length = None;
    let mut saw_transfer_encoding = false;
    for (name, value) in headers {
        if !_parsed {
            if !FIELD_NAME_RE.is_match(&name) {
                return Err(ProtocolError::LocalProtocolError(
                    format!("Illegal header name {:?}", &name).into(),
                ));
            }
            if !FIELD_VALUE_RE.is_match(&value) {
                return Err(ProtocolError::LocalProtocolError(
                    format!("Illegal header value {:?}", &value).into(),
                ));
            }
        }
        let raw_name = name.clone();
        let name = name.to_ascii_lowercase();
        if name == b"content-length" {
            let lengths: HashSet<Vec<u8>> = value
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
                    "conflicting Content-Length headers".into(),
                ));
            }
            let value = lengths.iter().next().unwrap();
            if !CONTENT_LENGTH_RE.is_match(value) {
                return Err(ProtocolError::LocalProtocolError(
                    "bad Content-Length".into(),
                ));
            }
            if seen_content_length.is_none() {
                seen_content_length = Some(value.clone());
                new_headers.push((raw_name, name, value.clone()));
            } else if seen_content_length != Some(value.clone()) {
                return Err(ProtocolError::LocalProtocolError(
                    "conflicting Content-Length headers".into(),
                ));
            }
        } else if name == b"transfer-encoding" {
            // "A server that receives a request message with a transfer coding
            // it does not understand SHOULD respond with 501 (Not
            // Implemented)."
            // https://tools.ietf.org/html/rfc7230#section-3.3.1
            if saw_transfer_encoding {
                return Err(ProtocolError::LocalProtocolError(
                    ("multiple Transfer-Encoding headers", 501).into(),
                ));
            }
            // "All transfer-coding names are case-insensitive"
            // -- https://tools.ietf.org/html/rfc7230#section-4
            let value = value.to_ascii_lowercase();
            if value != b"chunked" {
                return Err(ProtocolError::LocalProtocolError(
                    ("Only Transfer-Encoding: chunked is supported", 501).into(),
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

pub fn get_comma_header(headers: &Headers, name: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = vec![];
    let name = name.to_ascii_lowercase();
    for (found_name, found_value) in headers.iter() {
        if found_name == name {
            for found_split_value in found_value.to_ascii_lowercase().split(|&b| b == b',') {
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
    name: &[u8],
    new_values: Vec<Vec<u8>>,
) -> Result<Headers, ProtocolError> {
    let mut new_headers = vec![];
    for (found_name, found_value) in headers.iter() {
        if found_name != name {
            new_headers.push((found_name, found_value));
        }
    }
    for new_value in new_values {
        new_headers.push((name.to_vec(), new_value));
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
    let expect = get_comma_header(&request.headers, b"expect");
    expect.contains(&b"100-continue".to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_and_validate() {
        assert_eq!(
            normalize_and_validate(vec![(b"foo".to_vec(), b"bar".to_vec())], false).unwrap(),
            Headers(vec![(b"foo".to_vec(), b"foo".to_vec(), b"bar".to_vec())])
        );

        // no leading/trailing whitespace in names
        assert_eq!(
            normalize_and_validate(vec![(b"foo ".to_vec(), b"bar".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                ("Illegal header name [102, 111, 111, 32]".to_string(), 400).into()
            )
        );
        assert_eq!(
            normalize_and_validate(vec![(b" foo".to_vec(), b"bar".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                ("Illegal header name [32, 102, 111, 111]".to_string(), 400).into()
            )
        );

        // no weird characters in names
        assert_eq!(
            normalize_and_validate(vec![(b"foo bar".to_vec(), b"baz".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                (
                    "Illegal header name [102, 111, 111, 32, 98, 97, 114]".to_string(),
                    400
                )
                    .into()
            )
        );
        assert_eq!(
            normalize_and_validate(vec![(b"foo\x00bar".to_vec(), b"baz".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                (
                    "Illegal header name [102, 111, 111, 0, 98, 97, 114]".to_string(),
                    400
                )
                    .into()
            )
        );
        // Not even 8-bit characters:
        assert_eq!(
            normalize_and_validate(vec![(b"foo\xffbar".to_vec(), b"baz".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                (
                    "Illegal header name [102, 111, 111, 255, 98, 97, 114]".to_string(),
                    400
                )
                    .into()
            )
        );
        // And not even the control characters we allow in values:
        assert_eq!(
            normalize_and_validate(vec![(b"foo\x01bar".to_vec(), b"baz".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                (
                    "Illegal header name [102, 111, 111, 1, 98, 97, 114]".to_string(),
                    400
                )
                    .into()
            )
        );

        // no return or NUL characters in values
        assert_eq!(
            normalize_and_validate(vec![(b"foo".to_vec(), b"bar\rbaz".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                (
                    "Illegal header value [98, 97, 114, 13, 98, 97, 122]".to_string(),
                    400
                )
                    .into()
            )
        );
        assert_eq!(
            normalize_and_validate(vec![(b"foo".to_vec(), b"bar\nbaz".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                (
                    "Illegal header value [98, 97, 114, 10, 98, 97, 122]".to_string(),
                    400
                )
                    .into()
            )
        );
        assert_eq!(
            normalize_and_validate(vec![(b"foo".to_vec(), b"bar\x00baz".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                (
                    "Illegal header value [98, 97, 114, 0, 98, 97, 122]".to_string(),
                    400
                )
                    .into()
            )
        );
        // no leading/trailing whitespace
        assert_eq!(
            normalize_and_validate(vec![(b"foo".to_vec(), b"barbaz  ".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                (
                    "Illegal header value [98, 97, 114, 98, 97, 122, 32, 32]".to_string(),
                    400
                )
                    .into()
            )
        );
        assert_eq!(
            normalize_and_validate(vec![(b"foo".to_vec(), b"  barbaz".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                (
                    "Illegal header value [32, 32, 98, 97, 114, 98, 97, 122]".to_string(),
                    400
                )
                    .into()
            )
        );
        assert_eq!(
            normalize_and_validate(vec![(b"foo".to_vec(), b"barbaz\t".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                (
                    "Illegal header value [98, 97, 114, 98, 97, 122, 9]".to_string(),
                    400
                )
                    .into()
            )
        );
        assert_eq!(
            normalize_and_validate(vec![(b"foo".to_vec(), b"\tbarbaz".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                (
                    "Illegal header value [9, 98, 97, 114, 98, 97, 122]".to_string(),
                    400
                )
                    .into()
            )
        );

        // content-length
        assert_eq!(
            normalize_and_validate(vec![(b"Content-Length".to_vec(), b"1".to_vec())], false)
                .unwrap(),
            Headers(vec![(
                b"Content-Length".to_vec(),
                b"content-length".to_vec(),
                b"1".to_vec()
            )])
        );
        assert_eq!(
            normalize_and_validate(vec![(b"Content-Length".to_vec(), b"asdf".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(("bad Content-Length".to_string(), 400).into())
        );
        assert_eq!(
            normalize_and_validate(vec![(b"Content-Length".to_vec(), b"1x".to_vec())], false)
                .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(("bad Content-Length".to_string(), 400).into())
        );
        assert_eq!(
            normalize_and_validate(
                vec![
                    (b"Content-Length".to_vec(), b"1".to_vec()),
                    (b"Content-Length".to_vec(), b"2".to_vec())
                ],
                false
            )
            .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                ("conflicting Content-Length headers".to_string(), 400).into()
            )
        );
        assert_eq!(
            normalize_and_validate(
                vec![
                    (b"Content-Length".to_vec(), b"0".to_vec()),
                    (b"Content-Length".to_vec(), b"0".to_vec())
                ],
                false
            )
            .unwrap(),
            Headers(vec![(
                b"Content-Length".to_vec(),
                b"content-length".to_vec(),
                b"0".to_vec()
            )])
        );
        assert_eq!(
            normalize_and_validate(vec![(b"Content-Length".to_vec(), b"0 , 0".to_vec())], false)
                .unwrap(),
            Headers(vec![(
                b"Content-Length".to_vec(),
                b"content-length".to_vec(),
                b"0".to_vec()
            )])
        );
        assert_eq!(
            normalize_and_validate(
                vec![
                    (b"Content-Length".to_vec(), b"1".to_vec()),
                    (b"Content-Length".to_vec(), b"1".to_vec()),
                    (b"Content-Length".to_vec(), b"2".to_vec())
                ],
                false
            )
            .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                ("conflicting Content-Length headers".to_string(), 400).into()
            )
        );
        assert_eq!(
            normalize_and_validate(
                vec![(b"Content-Length".to_vec(), b"1 , 1,2".to_vec())],
                false
            )
            .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                ("conflicting Content-Length headers".to_string(), 400).into()
            )
        );

        // transfer-encoding
        assert_eq!(
            normalize_and_validate(
                vec![(b"Transfer-Encoding".to_vec(), b"chunked".to_vec())],
                false
            )
            .unwrap(),
            Headers(vec![(
                b"Transfer-Encoding".to_vec(),
                b"transfer-encoding".to_vec(),
                b"chunked".to_vec()
            )])
        );
        assert_eq!(
            normalize_and_validate(
                vec![(b"Transfer-Encoding".to_vec(), b"cHuNkEd".to_vec())],
                false
            )
            .unwrap(),
            Headers(vec![(
                b"Transfer-Encoding".to_vec(),
                b"transfer-encoding".to_vec(),
                b"chunked".to_vec()
            )])
        );
        assert_eq!(
            normalize_and_validate(
                vec![(b"Transfer-Encoding".to_vec(), b"gzip".to_vec())],
                false
            )
            .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                (
                    "Only Transfer-Encoding: chunked is supported".to_string(),
                    501
                )
                    .into()
            )
        );
        assert_eq!(
            normalize_and_validate(
                vec![
                    (b"Transfer-Encoding".to_vec(), b"chunked".to_vec()),
                    (b"Transfer-Encoding".to_vec(), b"gzip".to_vec())
                ],
                false
            )
            .expect_err("Expect ProtocolError::LocalProtocolError"),
            ProtocolError::LocalProtocolError(
                ("multiple Transfer-Encoding headers".to_string(), 501).into()
            )
        );
    }

    #[test]
    fn test_get_set_comma_header() {
        let headers = normalize_and_validate(
            vec![
                (b"Connection".to_vec(), b"close".to_vec()),
                (b"whatever".to_vec(), b"something".to_vec()),
                (b"connectiON".to_vec(), b"fOo,, , BAR".to_vec()),
            ],
            false,
        )
        .unwrap();

        assert_eq!(
            get_comma_header(&headers, b"connection"),
            vec![b"close".to_vec(), b"foo".to_vec(), b"bar".to_vec()]
        );

        let headers =
            set_comma_header(&headers, b"newthing", vec![b"a".to_vec(), b"b".to_vec()]).unwrap();

        assert_eq!(
            headers,
            Headers(vec![
                (
                    b"connection".to_vec(),
                    b"connection".to_vec(),
                    b"close".to_vec()
                ),
                (
                    b"whatever".to_vec(),
                    b"whatever".to_vec(),
                    b"something".to_vec()
                ),
                (
                    b"connection".to_vec(),
                    b"connection".to_vec(),
                    b"fOo,, , BAR".to_vec()
                ),
                (b"newthing".to_vec(), b"newthing".to_vec(), b"a".to_vec()),
                (b"newthing".to_vec(), b"newthing".to_vec(), b"b".to_vec()),
            ])
        );

        let headers =
            set_comma_header(&headers, b"whatever", vec![b"different thing".to_vec()]).unwrap();

        assert_eq!(
            headers,
            Headers(vec![
                (
                    b"connection".to_vec(),
                    b"connection".to_vec(),
                    b"close".to_vec()
                ),
                (
                    b"connection".to_vec(),
                    b"connection".to_vec(),
                    b"fOo,, , BAR".to_vec()
                ),
                (b"newthing".to_vec(), b"newthing".to_vec(), b"a".to_vec()),
                (b"newthing".to_vec(), b"newthing".to_vec(), b"b".to_vec()),
                (
                    b"whatever".to_vec(),
                    b"whatever".to_vec(),
                    b"different thing".to_vec()
                ),
            ])
        );
    }

    #[test]
    fn test_has_100_continue() {
        assert!(has_expect_100_continue(&Request {
            method: b"GET".to_vec(),
            target: b"/".to_vec(),
            headers: normalize_and_validate(
                vec![
                    (b"Host".to_vec(), b"example.com".to_vec()),
                    (b"Expect".to_vec(), b"100-continue".to_vec())
                ],
                false
            )
            .unwrap(),
            http_version: b"1.1".to_vec(),
        }));
        assert!(!has_expect_100_continue(&Request {
            method: b"GET".to_vec(),
            target: b"/".to_vec(),
            headers: normalize_and_validate(
                vec![(b"Host".to_vec(), b"example.com".to_vec())],
                false
            )
            .unwrap(),
            http_version: b"1.1".to_vec(),
        }));
        // Case insensitive
        assert!(has_expect_100_continue(&Request {
            method: b"GET".to_vec(),
            target: b"/".to_vec(),
            headers: normalize_and_validate(
                vec![
                    (b"Host".to_vec(), b"example.com".to_vec()),
                    (b"Expect".to_vec(), b"100-Continue".to_vec())
                ],
                false
            )
            .unwrap(),
            http_version: b"1.1".to_vec(),
        }));
        // Doesn't work in HTTP/1.0
        assert!(!has_expect_100_continue(&Request {
            method: b"GET".to_vec(),
            target: b"/".to_vec(),
            headers: normalize_and_validate(
                vec![
                    (b"Host".to_vec(), b"example.com".to_vec()),
                    (b"Expect".to_vec(), b"100-continue".to_vec())
                ],
                false
            )
            .unwrap(),
            http_version: b"1.0".to_vec(),
        }));
    }
}
