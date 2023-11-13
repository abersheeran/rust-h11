use crate::_events::*;
use crate::_headers::*;
use crate::_readers::*;
use crate::_receivebuffer::*;
use crate::_state::*;
use crate::_util::*;
use crate::_writers::*;
use std::collections::HashMap;

static DEFAULT_MAX_INCOMPLETE_EVENT_SIZE: usize = 16 * 1024;

enum RequestOrResponse {
    Request(Request),
    Response(Response),
}

impl RequestOrResponse {
    pub fn headers(&self) -> &Headers {
        match self {
            Self::Request(request) => &request.headers,
            Self::Response(response) => &response.headers,
        }
    }

    pub fn http_version(&self) -> &Vec<u8> {
        match self {
            Self::Request(request) => &request.http_version,
            Self::Response(response) => &response.http_version,
        }
    }
}

impl From<Request> for RequestOrResponse {
    fn from(value: Request) -> Self {
        Self::Request(value)
    }
}

impl From<Response> for RequestOrResponse {
    fn from(value: Response) -> Self {
        Self::Response(value)
    }
}

impl From<Event> for RequestOrResponse {
    fn from(value: Event) -> Self {
        match value {
            Event::Request(request) => Self::Request(request),
            Event::NormalResponse(response) => Self::Response(response),
            _ => panic!("Invalid event type"),
        }
    }
}

fn _keep_alive<T: Into<RequestOrResponse>>(event: T) -> bool {
    let event: RequestOrResponse = event.into();
    let connection = get_comma_header(event.headers(), b"connection".to_vec());
    if connection.contains(&b"close".to_vec()) {
        return false;
    }
    if event.http_version() < &b"1.1".to_vec() {
        return false;
    }
    return true;
}

fn _body_framing<T: Into<RequestOrResponse>>(request_method: &[u8], event: T) -> (&str, isize) {
    let event: RequestOrResponse = event.into();
    if let RequestOrResponse::Response(response) = &event {
        if response.status_code == 204
            || response.status_code == 304
            || request_method == b"HEAD"
            || (request_method == b"CONNECT"
                && 200 <= response.status_code
                && response.status_code < 300)
        {
            return ("content-length", 0);
        }
        assert!(response.status_code >= 200);
    }

    let trasfer_encodings = get_comma_header(event.headers(), b"transfer-encoding".to_vec());
    if !trasfer_encodings.is_empty() {
        assert!(trasfer_encodings == vec![b"chunked".to_vec()]);
        return ("chunked", 0);
    }

    let content_lengths = get_comma_header(event.headers(), b"content-length".to_vec());
    if !content_lengths.is_empty() {
        return (
            "content-length",
            std::str::from_utf8(&content_lengths[0])
                .unwrap()
                .parse()
                .unwrap(),
        );
    }

    if let RequestOrResponse::Request(_) = event {
        return ("content-length", 0);
    } else {
        return ("http/1.0", 0);
    }
}

pub struct Connection {
    pub our_role: Role,
    pub their_role: Role,
    _cstate: ConnectionState,
    _writer: Option<Box<WriterFnMut>>,
    _reader: Option<Box<dyn Reader>>,
    _max_incomplete_event_size: usize,
    _receive_buffer: ReceiveBuffer,
    _receive_buffer_closed: bool,
    pub their_http_version: Option<Vec<u8>>,
    _request_method: Option<Vec<u8>>,
    pub client_is_waiting_for_100_continue: bool,
}

impl Connection {
    pub fn new(our_role: Role, max_incomplete_event_size: Option<usize>) -> Self {
        Self {
            our_role,
            their_role: if our_role == Role::Client {
                Role::Server
            } else {
                Role::Client
            },
            _cstate: ConnectionState::new(),
            _writer: match our_role {
                Role::Client => Some(Box::new(write_request)),
                Role::Server => Some(Box::new(write_response)),
            },
            _reader: match our_role {
                Role::Server => Some(Box::new(IdleClientReader {})),
                Role::Client => Some(Box::new(SendResponseServerReader {})),
            },
            _max_incomplete_event_size: max_incomplete_event_size
                .unwrap_or(DEFAULT_MAX_INCOMPLETE_EVENT_SIZE),
            _receive_buffer: ReceiveBuffer::new(),
            _receive_buffer_closed: false,
            their_http_version: None,
            _request_method: None,
            client_is_waiting_for_100_continue: false,
        }
    }

    pub fn get_states(&self) -> HashMap<Role, State> {
        self._cstate.states.clone()
    }

    pub fn get_our_state(&self) -> State {
        self._cstate.states[&self.our_role]
    }

    pub fn get_their_state(&self) -> State {
        self._cstate.states[&self.their_role]
    }

    pub fn get_they_are_waiting_for_100_continue(&self) -> bool {
        self.their_role == Role::Client && self.client_is_waiting_for_100_continue
    }

    pub fn start_next_cycle(&mut self) -> Result<(), ProtocolError> {
        let old_states = self._cstate.states.clone();
        self._cstate.start_next_cycle()?;
        self._request_method = None;
        self.their_http_version = None;
        self.client_is_waiting_for_100_continue = false;
        self._respond_to_state_changes(old_states, None);
        Ok(())
    }

    fn _process_error(&mut self, role: Role) {
        let old_states = self._cstate.states.clone();
        self._cstate.process_error(role);
        self._respond_to_state_changes(old_states, None);
    }

    fn _server_switch_event(&self, event: Event) -> Option<Switch> {
        if let Event::InformationalResponse(informational_response) = &event {
            if informational_response.status_code == 101 {
                return Some(Switch::SwitchUpgrade);
            }
        }
        if let Event::NormalResponse(response) = &event {
            if self
                ._cstate
                .pending_switch_proposals
                .contains(&Switch::SwitchConnect)
                && 200 <= response.status_code
                && response.status_code < 300
            {
                return Some(Switch::SwitchConnect);
            }
        }
        return None;
    }

    fn _process_event(&mut self, role: Role, event: Event) -> Result<(), ProtocolError> {
        let old_states = self._cstate.states.clone();
        if role == Role::Client {
            if let Event::Request(request) = event.clone() {
                if request.method == b"CONNECT" {
                    self._cstate
                        .process_client_switch_proposal(Switch::SwitchConnect);
                }
                if get_comma_header(&request.headers, b"upgrade".to_vec()).len() > 0 {
                    self._cstate
                        .process_client_switch_proposal(Switch::SwitchUpgrade);
                }
            }
        }
        let server_switch_event = if role == Role::Server {
            self._server_switch_event(event.clone())
        } else {
            None
        };
        self._cstate
            .process_event(role, (&event).into(), server_switch_event)?;

        if let Event::Request(request) = event.clone() {
            self._request_method = Some(request.method);
        }

        if role == self.their_role {
            if let Event::Request(request) = event.clone() {
                self.their_http_version = Some(request.http_version);
            }
            if let Event::NormalResponse(response) = event.clone() {
                self.their_http_version = Some(response.http_version);
            }
            if let Event::InformationalResponse(informational_response) = event.clone() {
                self.their_http_version = Some(informational_response.http_version);
            }
        }

        if let Event::Request(request) = event.clone() {
            if !_keep_alive(RequestOrResponse::from(request)) {
                self._cstate.process_keep_alive_disabled();
            }
        }
        if let Event::NormalResponse(response) = event.clone() {
            if !_keep_alive(RequestOrResponse::from(response)) {
                self._cstate.process_keep_alive_disabled();
            }
        }

        if let Event::Request(request) = event.clone() {
            if has_expect_100_continue(&request) {
                self.client_is_waiting_for_100_continue = true;
            }
        }
        match (&event).into() {
            EventType::InformationalResponse => {
                self.client_is_waiting_for_100_continue = false;
            }
            EventType::NormalResponse => {
                self.client_is_waiting_for_100_continue = false;
            }
            EventType::Data => {
                if role == Role::Client {
                    self.client_is_waiting_for_100_continue = false;
                }
            }
            EventType::EndOfMessage => {
                if role == Role::Client {
                    self.client_is_waiting_for_100_continue = false;
                }
            }
            _ => {}
        }

        self._respond_to_state_changes(old_states, Some(event));
        Ok(())
    }

    fn _respond_to_state_changes(
        &mut self,
        old_states: HashMap<Role, State>,
        event: Option<Event>,
    ) {
        if self.get_our_state() != old_states[&self.our_role] {
            let state = self._cstate.states[&self.our_role];
            self._writer = match state {
                State::SendBody => {
                    let (framing_type, length) = _body_framing(
                        &self._request_method.as_ref().unwrap(),
                        RequestOrResponse::from(event.clone().unwrap()),
                    );

                    match framing_type {
                        "content-length" => Some(Box::new(content_length_writer(length))),
                        "chunked" => Some(Box::new(chunked_writer())),
                        "http/1.0" => Some(Box::new(http10_writer())),
                        _ => {
                            panic!("Invalid role and framing type combination");
                        }
                    }
                }
                _ => match (&self.our_role, state) {
                    (Role::Client, State::Idle) => Some(Box::new(write_request)),
                    (Role::Server, State::Idle) => Some(Box::new(write_response)),
                    (Role::Server, State::SendResponse) => Some(Box::new(write_response)),
                    _ => None,
                },
            };
        }
        if self.get_their_state() != old_states[&self.their_role] {
            self._reader = match self._cstate.states[&self.their_role] {
                State::SendBody => {
                    let (framing_type, length) = _body_framing(
                        &self._request_method.as_ref().unwrap(),
                        RequestOrResponse::from(event.clone().unwrap()),
                    );
                    match framing_type {
                        "content-length" => {
                            Some(Box::new(ContentLengthReader::new(length as usize)))
                        }
                        "chunked" => Some(Box::new(ChunkedReader::new())),
                        "http/1.0" => Some(Box::new(Http10Reader {})),
                        _ => {
                            panic!("Invalid role and framing type combination");
                        }
                    }
                }
                _ => match (&self.their_role, self._cstate.states[&self.their_role]) {
                    (Role::Client, State::Idle) => Some(Box::new(IdleClientReader {})),
                    (Role::Server, State::Idle) => Some(Box::new(SendResponseServerReader {})),
                    (Role::Server, State::SendResponse) => {
                        Some(Box::new(SendResponseServerReader {}))
                    }
                    (Role::Client, State::Done) => Some(Box::new(ClosedReader {})),
                    (Role::Client, State::MustClose) => Some(Box::new(ClosedReader {})),
                    (Role::Client, State::Closed) => Some(Box::new(ClosedReader {})),
                    (Role::Server, State::Done) => Some(Box::new(ClosedReader {})),
                    (Role::Server, State::MustClose) => Some(Box::new(ClosedReader {})),
                    (Role::Server, State::Closed) => Some(Box::new(ClosedReader {})),
                    _ => None,
                },
            };
        }
    }

    pub fn receive_data(&mut self, data: &[u8]) {
        if data.len() > 0 {
            if self._receive_buffer_closed {
                panic!("received close, then received more data?");
            }
            self._receive_buffer.add(data);
        } else {
            self._receive_buffer_closed = true;
        }
    }

    fn _extract_next_receive_event(&mut self) -> Result<Event, ProtocolError> {
        let state = self.get_their_state();
        if state == State::Done && self._receive_buffer.len() > 0 {
            return Ok(Event::Paused());
        }
        if state == State::MightSwitchProtocol || state == State::SwitchedProtocol {
            return Ok(Event::Paused());
        }
        let event = self
            ._reader
            .as_mut()
            .unwrap()
            .call(&mut self._receive_buffer)?;
        if event.is_none() {
            if self._receive_buffer.len() == 0 && self._receive_buffer_closed {
                return self._reader.as_mut().unwrap().read_eof();
            }
        }
        Ok(event.unwrap_or(Event::NeedData()))
    }

    pub fn next_event(&mut self) -> Result<Event, ProtocolError> {
        if self.get_their_state() == State::Error {
            panic!("Can't receive data when peer state is ERROR");
        }
        match self._extract_next_receive_event() {
            Ok(event) => {
                match event {
                    Event::NeedData() | Event::Paused() => {
                        self._process_event(self.their_role, event.clone())?;
                    }
                    _ => {}
                };

                if let Event::NeedData() = event.clone() {
                    if self._receive_buffer.len() > self._max_incomplete_event_size {
                        return Err(ProtocolError::RemoteProtocolError(
                            ("Receive buffer too long".to_string(), 431).into(),
                        ));
                    }
                    if self._receive_buffer_closed {
                        return Err(ProtocolError::RemoteProtocolError(
                            "peer unexpectedly closed connection".to_string().into(),
                        ));
                    }
                }

                Ok(event)
            }
            Err(error) => {
                self._process_error(self.their_role);
                match error {
                    ProtocolError::LocalProtocolError(error) => {
                        Err(error._reraise_as_remote_protocol_error().into())
                    }
                    _ => Err(error),
                }
            }
        }
    }

    pub fn send(&mut self, event: Event) -> Result<Option<Vec<u8>>, ProtocolError> {
        self.send_with_data_passthrough(event)
    }

    pub fn send_with_data_passthrough(
        &mut self,
        mut event: Event,
    ) -> Result<Option<Vec<u8>>, ProtocolError> {
        if self.get_our_state() == State::Error {
            return Err(ProtocolError::LocalProtocolError(
                "Can't send data when our state is ERROR".to_string().into(),
            ));
        }
        event = if let Event::NormalResponse(response) = &event {
            Event::NormalResponse(self._clean_up_response_headers_for_sending(response.clone())?)
        } else {
            event
        };
        let mut res: Result<Vec<u8>, ProtocolError> = Ok(vec![]);
        {
            res = self._writer.as_mut().unwrap()(event.clone());
        }
        self._process_event(self.our_role, event.clone())?;
        let event_type: EventType = (&event).into();
        if event_type == EventType::ConnectionClosed {
            return Ok(None);
        } else {
            match res {
                Ok(data_list) => Ok(Some(data_list)),
                Err(error) => {
                    self._process_error(self.our_role);
                    Err(error)
                }
            }
        }
    }

    pub fn send_failed(&mut self) {
        self._process_error(self.our_role);
    }

    fn _clean_up_response_headers_for_sending(
        &self,
        response: Response,
    ) -> Result<Response, ProtocolError> {
        let mut headers = response.clone().headers;
        let mut need_close = false;
        let mut method_for_choosing_headers = self._request_method.clone().unwrap();
        if method_for_choosing_headers == b"HEAD".to_vec() {
            method_for_choosing_headers = b"GET".to_vec();
        }
        let (framing_type, _) = _body_framing(&method_for_choosing_headers, response.clone());
        if framing_type == "chunked" || framing_type == "http/1.0" {
            headers = set_comma_header(&headers, b"content-length".to_vec(), vec![])?;
            if self
                .their_http_version
                .clone()
                .map(|v| v < b"1.1".to_vec())
                .unwrap_or(true)
            {
                headers = set_comma_header(&headers, b"transfer-encoding".to_vec(), vec![])?;
                if self._request_method.clone().unwrap() != b"HEAD".to_vec() {
                    need_close = true;
                }
            } else {
                headers = set_comma_header(
                    &headers,
                    b"transfer-encoding".to_vec(),
                    vec![b"chunked".to_vec()],
                )?;
            }
        }
        if !self._cstate.keep_alive || need_close {
            let mut connection = get_comma_header(&headers, b"connection".to_vec());
            connection.retain(|x| x != &b"keep-alive".to_vec());
            connection.push(b"close".to_vec());
            headers = set_comma_header(&headers, b"connection".to_vec(), connection)?;
        }
        return Ok(Response {
            headers,
            status_code: response.status_code,
            http_version: response.http_version,
            reason: response.reason,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keep_alive() {
        assert!(_keep_alive(Request {
            method: b"GET".to_vec(),
            target: b"/".to_vec(),
            headers: vec![(b"Host".to_vec(), b"Example.com".to_vec())].into(),
            http_version: b"1.1".to_vec(),
        }));
        assert!(!_keep_alive(Request {
            method: b"GET".to_vec(),
            target: b"/".to_vec(),
            headers: vec![
                (b"Host".to_vec(), b"Example.com".to_vec()),
                (b"Connection".to_vec(), b"close".to_vec()),
            ]
            .into(),
            http_version: b"1.1".to_vec(),
        }));
        assert!(!_keep_alive(Request {
            method: b"GET".to_vec(),
            target: b"/".to_vec(),
            headers: vec![
                (b"Host".to_vec(), b"Example.com".to_vec()),
                (b"Connection".to_vec(), b"a, b, cLOse, foo".to_vec()),
            ]
            .into(),
            http_version: b"1.1".to_vec(),
        }));
        assert!(!_keep_alive(Request {
            method: b"GET".to_vec(),
            target: b"/".to_vec(),
            headers: vec![].into(),
            http_version: b"1.0".to_vec(),
        }));

        assert!(_keep_alive(Response {
            status_code: 200,
            headers: vec![].into(),
            http_version: b"1.1".to_vec(),
            reason: b"OK".to_vec(),
        }));
        assert!(!_keep_alive(Response {
            status_code: 200,
            headers: vec![(b"Connection".to_vec(), b"close".to_vec())].into(),
            http_version: b"1.1".to_vec(),
            reason: b"OK".to_vec(),
        }));
        assert!(!_keep_alive(Response {
            status_code: 200,
            headers: vec![(b"Connection".to_vec(), b"a, b, cLOse, foo".to_vec()),].into(),
            http_version: b"1.1".to_vec(),
            reason: b"OK".to_vec(),
        }));
        assert!(!_keep_alive(Response {
            status_code: 200,
            headers: vec![].into(),
            http_version: b"1.0".to_vec(),
            reason: b"OK".to_vec(),
        }));
    }

    #[test]
    fn test_body_framing() {
        fn headers(cl: Option<usize>, te: bool) -> Headers {
            let mut headers = vec![];
            if let Some(cl) = cl {
                headers.push((
                    b"Content-Length".to_vec(),
                    cl.to_string().as_bytes().to_vec(),
                ));
            }
            if te {
                headers.push((b"Transfer-Encoding".to_vec(), b"chunked".to_vec()));
            }
            headers.push((b"Host".to_vec(), b"example.com".to_vec()));
            return headers.into();
        }

        fn resp(status_code: u16, cl: Option<usize>, te: bool) -> Response {
            Response {
                status_code,
                headers: headers(cl, te),
                http_version: b"1.1".to_vec(),
                reason: b"OK".to_vec(),
            }
        }

        fn req(cl: Option<usize>, te: bool) -> Request {
            Request {
                method: b"GET".to_vec(),
                target: b"/".to_vec(),
                headers: headers(cl, te),
                http_version: b"1.1".to_vec(),
            }
        }

        // Special cases where the headers are ignored:
        for (cl, te) in vec![(Some(100), false), (None, true), (Some(100), true)] {
            for (meth, r) in vec![
                (b"HEAD".to_vec(), resp(200, cl, te)),
                (b"GET".to_vec(), resp(204, cl, te)),
                (b"GET".to_vec(), resp(304, cl, te)),
            ] {
                assert_eq!(_body_framing(&meth, r), ("content-length", 0));
            }
        }

        // Transfer-encoding
        for (cl, te) in vec![(None, true), (Some(100), true)] {
            for (meth, r) in vec![
                (b"".to_vec(), RequestOrResponse::from(req(cl, te))),
                (b"GET".to_vec(), RequestOrResponse::from(resp(200, cl, te))),
            ] {
                assert_eq!(_body_framing(&meth, r), ("chunked", 0));
            }
        }

        // Content-Length
        for (meth, r) in vec![
            (b"".to_vec(), RequestOrResponse::from(req(Some(100), false))),
            (
                b"GET".to_vec(),
                RequestOrResponse::from(resp(200, Some(100), false)),
            ),
        ] {
            assert_eq!(_body_framing(&meth, r), ("content-length", 100));
        }

        // No headers
        assert_eq!(_body_framing(b"", req(None, false)), ("content-length", 0));
        assert_eq!(
            _body_framing(b"GET", resp(200, None, false)),
            ("http/1.0", 0)
        );
    }
}
