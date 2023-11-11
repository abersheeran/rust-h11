use crate::{
    _events::{Data, EndOfMessage, Request, Response},
    _headers::Headers,
    _util::ProtocolError,
};
use std::any::Any;

pub type WriterFnMut = dyn FnMut(Box<dyn Any>) -> Result<Vec<u8>, ProtocolError>;

fn _write_headers(headers: &Headers) -> Result<Vec<u8>, ProtocolError> {
    let mut data_list = Vec::new();
    for (raw_name, name, value) in headers.raw_items() {
        if name == b"host" {
            data_list.append(&mut raw_name.clone());
            data_list.append(&mut b": ".to_vec());
            data_list.append(&mut value.clone());
            data_list.append(&mut b"\r\n".to_vec());
        }
    }
    for (raw_name, name, value) in headers.raw_items() {
        if name != b"host" {
            data_list.append(&mut raw_name.clone());
            data_list.append(&mut b": ".to_vec());
            data_list.append(&mut value.clone());
            data_list.append(&mut b"\r\n".to_vec());
        }
    }
    data_list.append(&mut b"\r\n".to_vec());
    Ok(data_list)
}

fn _write_request(request: &Request) -> Result<Vec<u8>, ProtocolError> {
    let mut data_list = Vec::new();
    if request.http_version != b"1.1" {
        return Err(ProtocolError::LocalProtocolError(
            "I only send HTTP/1.1".into(),
        ));
    }
    data_list.append(&mut request.method.clone());
    data_list.append(&mut b" ".to_vec());
    data_list.append(&mut request.target.clone());
    data_list.append(&mut b" HTTP/1.1\r\n".to_vec());
    data_list.append(&mut (_write_headers(&request.headers)?));
    Ok(data_list)
}

pub fn write_request(event: Box<dyn Any>) -> Result<Vec<u8>, ProtocolError> {
    match event.downcast_ref::<Request>() {
        Some(request) => _write_request(request),
        None => panic!("Expected Request event, got {:?}", event),
    }
}

fn _write_response(response: &Response) -> Result<Vec<u8>, ProtocolError> {
    if response.http_version != b"1.1" {
        return Err(ProtocolError::LocalProtocolError(
            "I only send HTTP/1.1".into(),
        ));
    }
    let status_code = response.status_code.to_string();
    let status_bytes = status_code.as_bytes();
    let mut data_list = Vec::new();
    data_list.append(&mut b"HTTP/1.1 ".to_vec());
    data_list.append(&mut status_bytes.to_vec());
    data_list.append(&mut b" ".to_vec());
    data_list.append(&mut response.reason.clone());
    data_list.append(&mut b"\r\n".to_vec());
    data_list.append(&mut (_write_headers(&response.headers))?);
    Ok(data_list)
}

pub fn write_response(event: Box<dyn Any>) -> Result<Vec<u8>, ProtocolError> {
    match event.downcast_ref::<Response>() {
        Some(response) => _write_response(response),
        None => panic!("Expected Response event, got {:?}", event),
    }
}

trait BodyWriter {
    fn call(&mut self, event: Box<dyn Any>) -> Result<Vec<u8>, ProtocolError> {
        if event.is::<Data>() {
            self.send_data(&event.downcast_ref::<Data>().unwrap().data)
        } else if event.is::<EndOfMessage>() {
            self.send_eom(&event.downcast_ref::<EndOfMessage>().unwrap().headers)
        } else {
            panic!("Unknown event type");
        }
    }

    fn send_data(&mut self, data: &Vec<u8>) -> Result<Vec<u8>, ProtocolError>;
    fn send_eom(&mut self, headers: &Headers) -> Result<Vec<u8>, ProtocolError>;
}

struct ContentLengthWriter {
    length: isize,
}

impl BodyWriter for ContentLengthWriter {
    fn send_data(&mut self, data: &Vec<u8>) -> Result<Vec<u8>, ProtocolError> {
        self.length -= data.len() as isize;
        if self.length < 0 {
            Err(ProtocolError::LocalProtocolError(
                "Too much data for declared Content-Length".into(),
            ))
        } else {
            Ok(data.clone())
        }
    }

    fn send_eom(&mut self, headers: &Headers) -> Result<Vec<u8>, ProtocolError> {
        if self.length != 0 {
            return Err(ProtocolError::LocalProtocolError(
                "Too little data for declared Content-Length".into(),
            ));
        }
        if headers.len() > 0 {
            return Err(ProtocolError::LocalProtocolError(
                "Content-Length and trailers don't mix".into(),
            ));
        }
        Ok(Vec::new())
    }
}

pub fn content_length_writer(
    length: isize,
) -> impl FnMut(Box<dyn Any>) -> Result<Vec<u8>, ProtocolError> {
    let mut writer = ContentLengthWriter { length };
    move |data: Box<dyn Any>| writer.call(data)
}

struct ChunkedWriter;

impl BodyWriter for ChunkedWriter {
    fn send_data(&mut self, data: &Vec<u8>) -> Result<Vec<u8>, ProtocolError> {
        // if we encoded 0-length data in the naive way, it would look like an
        // end-of-message.
        if data.len() == 0 {
            return Ok(Vec::new());
        }
        // write(format!("{:x}\r\n", data.len()).as_bytes().to_vec());
        // write(data.clone());
        // write(b"\r\n".to_vec());
        let mut data_list = Vec::new();
        data_list.append(&mut format!("{:x}\r\n", data.len()).as_bytes().to_vec());
        data_list.append(&mut data.clone());
        data_list.append(&mut b"\r\n".to_vec());
        Ok(data_list)
    }

    fn send_eom(&mut self, headers: &Headers) -> Result<Vec<u8>, ProtocolError> {
        // write(b"0\r\n".to_vec());
        // _write_headers(headers);
        let mut data_list = Vec::new();
        data_list.append(&mut b"0\r\n".to_vec());
        data_list.append(&mut (_write_headers(headers))?);
        Ok(data_list)
    }
}

pub fn chunked_writer() -> impl FnMut(Box<dyn Any>) -> Result<Vec<u8>, ProtocolError> {
    let mut writer = ChunkedWriter;
    move |data: Box<dyn Any>| writer.call(data)
}

struct Http10Writer;

impl BodyWriter for Http10Writer {
    fn send_data(&mut self, data: &Vec<u8>) -> Result<Vec<u8>, ProtocolError> {
        Ok(data.clone())
    }

    fn send_eom(&mut self, headers: &Headers) -> Result<Vec<u8>, ProtocolError> {
        if headers.len() > 0 {
            Err(ProtocolError::LocalProtocolError(
                "can't send trailers to HTTP/1.0 client".into(),
            ))
        } else {
            Ok(Vec::new())
        }
        // no need to close the socket ourselves, that will be taken care of by
        // Connection: close machinery
    }
}

pub fn http10_writer() -> impl FnMut(Box<dyn Any>) -> Result<Vec<u8>, ProtocolError> {
    let mut writer = Http10Writer;
    move |data: Box<dyn Any>| writer.call(data)
}
