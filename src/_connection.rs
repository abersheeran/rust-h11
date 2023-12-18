use crate::_events::*;
use crate::_headers::*;
use crate::_readers::*;
use crate::_receivebuffer::*;
use crate::_state::*;
use crate::_util::*;
use crate::_writers::*;
use std::collections::HashMap;
use std::collections::HashSet;

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
    let connection = get_comma_header(event.headers(), b"connection");
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

    let trasfer_encodings = get_comma_header(event.headers(), b"transfer-encoding");
    if !trasfer_encodings.is_empty() {
        assert!(trasfer_encodings == vec![b"chunked".to_vec()]);
        return ("chunked", 0);
    }

    let content_lengths = get_comma_header(event.headers(), b"content-length");
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
    _writer: Option<Box<dyn FnMut(Event) -> Result<Vec<u8>, ProtocolError> + Send>>,
    _reader: Option<Box<dyn Reader + Send>>,
    _max_incomplete_event_size: usize,
    _receive_buffer: ReceiveBuffer,
    _receive_buffer_closed: bool,
    pub their_http_version: Option<Vec<u8>>,
    _request_method: Option<Vec<u8>>,
    client_is_waiting_for_100_continue: bool,
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

    pub fn get_client_is_waiting_for_100_continue(&self) -> bool {
        self.client_is_waiting_for_100_continue
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
                if get_comma_header(&request.headers, b"upgrade").len() > 0 {
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
                    let request_method = self._request_method.clone().unwrap_or(vec![]);
                    let (framing_type, length) = _body_framing(
                        &request_method,
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
                    let request_method = self._request_method.clone().unwrap_or(vec![]);
                    let (framing_type, length) = _body_framing(
                        &request_method,
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

    pub fn get_trailing_data(&self) -> (Vec<u8>, bool) {
        (
            self._receive_buffer.bytes().to_vec(),
            self._receive_buffer_closed,
        )
    }

    pub fn receive_data(&mut self, data: &[u8]) -> Result<(), String> {
        Ok(if data.len() > 0 {
            if self._receive_buffer_closed {
                return Err("received close, then received more data?".to_string());
            }
            self._receive_buffer.add(data);
        } else {
            self._receive_buffer_closed = true;
        })
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
            return Err(ProtocolError::RemoteProtocolError(
                "Can't receive data when peer state is ERROR".into(),
            ));
        }
        match (|| {
            let event = self._extract_next_receive_event()?;
            match event {
                Event::NeedData() | Event::Paused() => {}
                _ => {
                    self._process_event(self.their_role, event.clone())?;
                }
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
        })() {
            Err(error) => {
                self._process_error(self.their_role);
                match error {
                    ProtocolError::LocalProtocolError(error) => {
                        Err(error._reraise_as_remote_protocol_error().into())
                    }
                    _ => Err(error),
                }
            }
            Ok(any) => Ok(any),
        }
    }

    pub fn send(&mut self, mut event: Event) -> Result<Option<Vec<u8>>, ProtocolError> {
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
        let event_type: EventType = (&event).into();
        let res: Result<Vec<u8>, ProtocolError> = match self._writer.as_mut() {
            Some(_) if event_type == EventType::ConnectionClosed => Ok(vec![]),
            Some(writer) => writer(event.clone()),
            None => Err(ProtocolError::LocalProtocolError(
                "Can't send data when our state is not SEND_BODY"
                    .to_string()
                    .into(),
            )),
        };
        self._process_event(self.our_role, event.clone())?;
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
        let mut method_for_choosing_headers = self._request_method.clone().unwrap_or(vec![]);
        if method_for_choosing_headers == b"HEAD".to_vec() {
            method_for_choosing_headers = b"GET".to_vec();
        }
        let (framing_type, _) = _body_framing(&method_for_choosing_headers, response.clone());
        if framing_type == "chunked" || framing_type == "http/1.0" {
            headers = set_comma_header(&headers, b"content-length", vec![])?;
            if self
                .their_http_version
                .clone()
                .map(|v| v < b"1.1".to_vec())
                .unwrap_or(true)
            {
                headers = set_comma_header(&headers, b"transfer-encoding", vec![])?;
                if self._request_method.clone().unwrap_or(vec![]) != b"HEAD".to_vec() {
                    need_close = true;
                }
            } else {
                headers =
                    set_comma_header(&headers, b"transfer-encoding", vec![b"chunked".to_vec()])?;
            }
        }
        if !self._cstate.keep_alive || need_close {
            let mut connection: HashSet<Vec<u8>> = get_comma_header(&headers, b"connection")
                .into_iter()
                .collect();
            connection.retain(|x| x != &b"keep-alive".to_vec());
            connection.insert(b"close".to_vec());
            headers = set_comma_header(&headers, b"connection", connection.into_iter().collect())?;
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

    // from typing import cast, List, Type, Union, ValuesView

    // from .._connection import Connection, NEED_DATA, PAUSED
    // from .._events import (
    //     ConnectionClosed,
    //     Data,
    //     EndOfMessage,
    //     Event,
    //     InformationalResponse,
    //     Request,
    //     Response,
    // )
    // from .._state import CLIENT, CLOSED, DONE, MUST_CLOSE, SERVER
    // from .._util import Sentinel

    // try:
    //     from typing import Literal
    // except ImportError:
    //     from typing_extensions import Literal  # type: ignore

    // def get_all_events(conn: Connection) -> List[Event]:
    //     got_events = []
    //     while True:
    //         event = conn.next_event()
    //         if event in (NEED_DATA, PAUSED):
    //             break
    //         event = cast(Event, event)
    //         got_events.append(event)
    //         if type(event) is ConnectionClosed:
    //             break
    //     return got_events

    // def receive_and_get(conn: Connection, data: bytes) -> List[Event]:
    //     conn.receive_data(data)
    //     return get_all_events(conn)

    // # Merges adjacent Data events, converts payloads to bytestrings, and removes
    // # chunk boundaries.
    // def normalize_data_events(in_events: List[Event]) -> List[Event]:
    //     out_events: List[Event] = []
    //     for event in in_events:
    //         if type(event) is Data:
    //             event = Data(data=bytes(event.data), chunk_start=False, chunk_end=False)
    //         if out_events and type(out_events[-1]) is type(event) is Data:
    //             out_events[-1] = Data(
    //                 data=out_events[-1].data + event.data,
    //                 chunk_start=out_events[-1].chunk_start,
    //                 chunk_end=out_events[-1].chunk_end,
    //             )
    //         else:
    //             out_events.append(event)
    //     return out_events

    // # Given that we want to write tests that push some events through a Connection
    // # and check that its state updates appropriately... we might as make a habit
    // # of pushing them through two Connections with a fake network link in
    // # between.
    // class ConnectionPair:
    //     def __init__(self) -> None:
    //         self.conn = {CLIENT: Connection(CLIENT), SERVER: Connection(SERVER)}
    //         self.other = {CLIENT: SERVER, SERVER: CLIENT}

    //     @property
    //     def conns(self) -> ValuesView[Connection]:
    //         return self.conn.values()

    //     # expect="match" if expect=send_events; expect=[...] to say what expected
    //     def send(
    //         self,
    //         role: Type[Sentinel],
    //         send_events: Union[List[Event], Event],
    //         expect: Union[List[Event], Event, Literal["match"]] = "match",
    //     ) -> bytes:
    //         if not isinstance(send_events, list):
    //             send_events = [send_events]
    //         data = b""
    //         closed = False
    //         for send_event in send_events:
    //             new_data = self.conn[role].send(send_event)
    //             if new_data is None:
    //                 closed = True
    //             else:
    //                 data += new_data
    //         # send uses b"" to mean b"", and None to mean closed
    //         # receive uses b"" to mean closed, and None to mean "try again"
    //         # so we have to translate between the two conventions
    //         if data:
    //             self.conn[self.other[role]].receive_data(data)
    //         if closed:
    //             self.conn[self.other[role]].receive_data(b"")
    //         got_events = get_all_events(self.conn[self.other[role]])
    //         if expect == "match":
    //             expect = send_events
    //         if not isinstance(expect, list):
    //             expect = [expect]
    //         assert got_events == expect
    //         return data

    use std::collections::HashMap;

    pub fn get_all_events(conn: &mut Connection) -> Result<Vec<Event>, ProtocolError> {
        let mut got_events = Vec::new();
        loop {
            let event = conn.next_event()?;
            let event_type = EventType::from(&event);
            if event_type == EventType::NeedData || event_type == EventType::Paused {
                break;
            }
            got_events.push(event);
            if event_type == EventType::ConnectionClosed {
                break;
            }
        }
        return Ok(got_events);
    }

    pub fn receive_and_get(
        conn: &mut Connection,
        data: &[u8],
    ) -> Result<Vec<Event>, ProtocolError> {
        conn.receive_data(data).unwrap();
        return get_all_events(conn);
    }

    pub struct ConnectionPair {
        pub conn: HashMap<Role, Connection>,
        pub other: HashMap<Role, Role>,
    }

    impl ConnectionPair {
        pub fn new() -> Self {
            Self {
                conn: HashMap::from([
                    (Role::Client, Connection::new(Role::Client, None)),
                    (Role::Server, Connection::new(Role::Server, None)),
                ]),
                other: HashMap::from([(Role::Client, Role::Server), (Role::Server, Role::Client)]),
            }
        }

        pub fn send(
            &mut self,
            role: Role,
            send_events: Vec<Event>,
            expect: Option<Vec<Event>>,
        ) -> Result<Vec<u8>, ProtocolError> {
            let mut data = Vec::new();
            let mut closed = false;
            for send_event in &send_events {
                match self.conn.get_mut(&role).unwrap().send(send_event.clone())? {
                    Some(new_data) => data.extend(new_data),
                    None => closed = true,
                }
            }
            if !data.is_empty() {
                self.conn
                    .get_mut(&self.other[&role])
                    .unwrap()
                    .receive_data(&data)
                    .unwrap();
            }
            if closed {
                self.conn
                    .get_mut(&self.other[&role])
                    .unwrap()
                    .receive_data(b"")
                    .unwrap();
            }
            let got_events = get_all_events(self.conn.get_mut(&self.other[&role]).unwrap())?;
            match expect {
                Some(expect) => assert_eq!(got_events, expect),
                None => assert_eq!(got_events, send_events),
            };

            Ok(data)
        }
    }

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

    #[test]
    fn test_connection_basics_and_content_length() {
        let mut p = ConnectionPair::new();
        assert_eq!(
            p.send(
                Role::Client,
                vec![Request::new(
                    b"GET".to_vec(),
                    vec![
                        (b"Host".to_vec(), b"example.com".to_vec()),
                        (b"Content-Length".to_vec(), b"10".to_vec())
                    ]
                    .into(),
                    b"/".to_vec(),
                    b"1.1".to_vec(),
                )
                .unwrap()
                .into(),],
                None,
            )
            .unwrap(),
            b"GET / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 10\r\n\r\n".to_vec()
        );
        for (_, connection) in &p.conn {
            assert_eq!(
                connection.get_states(),
                HashMap::from([
                    (Role::Client, State::SendBody),
                    (Role::Server, State::SendResponse),
                ])
            );
        }
        assert_eq!(p.conn[&Role::Client].get_our_state(), State::SendBody);
        assert_eq!(p.conn[&Role::Client].get_their_state(), State::SendResponse);
        assert_eq!(p.conn[&Role::Server].get_our_state(), State::SendResponse);
        assert_eq!(p.conn[&Role::Server].get_their_state(), State::SendBody);
        assert_eq!(p.conn[&Role::Client].their_http_version, None);
        assert_eq!(
            p.conn[&Role::Server].their_http_version,
            Some(b"1.1".to_vec())
        );

        assert_eq!(
            p.send(
                Role::Server,
                vec![Response {
                    status_code: 100,
                    headers: vec![].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into()],
                None
            )
            .unwrap(),
            b"HTTP/1.1 100 \r\n\r\n".to_vec()
        );

        assert_eq!(
            p.send(
                Role::Server,
                vec![Response {
                    status_code: 200,
                    headers: vec![(b"Content-Length".to_vec(), b"11".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into()],
                None
            )
            .unwrap(),
            b"HTTP/1.1 200 \r\nContent-Length: 11\r\n\r\n".to_vec()
        );

        for (_, connection) in &p.conn {
            assert_eq!(
                connection.get_states(),
                HashMap::from([
                    (Role::Client, State::SendBody),
                    (Role::Server, State::SendBody),
                ])
            );
        }

        assert_eq!(
            p.conn[&Role::Client].their_http_version,
            Some(b"1.1".to_vec())
        );
        assert_eq!(
            p.conn[&Role::Server].their_http_version,
            Some(b"1.1".to_vec())
        );

        assert_eq!(
            p.send(
                Role::Client,
                vec![Data {
                    data: b"12345".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }
                .into()],
                None
            )
            .unwrap(),
            b"12345".to_vec()
        );

        assert_eq!(
            p.send(
                Role::Client,
                vec![Data {
                    data: b"67890".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }
                .into()],
                Some(vec![
                    Data {
                        data: b"67890".to_vec(),
                        chunk_start: false,
                        chunk_end: false,
                    }
                    .into(),
                    EndOfMessage::default().into(),
                ]),
            )
            .unwrap(),
            b"67890".to_vec()
        );

        assert_eq!(
            p.send(
                Role::Client,
                vec![EndOfMessage::default().into()],
                Some(vec![]),
            )
            .unwrap(),
            b"".to_vec()
        );

        for (_, connection) in &p.conn {
            assert_eq!(
                connection.get_states(),
                HashMap::from([(Role::Client, State::Done), (Role::Server, State::SendBody),])
            );
        }

        assert_eq!(
            p.send(
                Role::Server,
                vec![Data {
                    data: b"1234567890".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }
                .into()],
                None
            )
            .unwrap(),
            b"1234567890".to_vec()
        );

        assert_eq!(
            p.send(
                Role::Server,
                vec![Data {
                    data: b"1".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }
                .into()],
                Some(vec![
                    Data {
                        data: b"1".to_vec(),
                        chunk_start: false,
                        chunk_end: false,
                    }
                    .into(),
                    EndOfMessage::default().into(),
                ]),
            )
            .unwrap(),
            b"1".to_vec()
        );

        assert_eq!(
            p.send(
                Role::Server,
                vec![EndOfMessage::default().into()],
                Some(vec![]),
            )
            .unwrap(),
            b"".to_vec()
        );

        for (_, connection) in &p.conn {
            assert_eq!(
                connection.get_states(),
                HashMap::from([(Role::Client, State::Done), (Role::Server, State::Done),])
            );
        }
    }

    #[test]
    fn test_chunked() {
        let mut p = ConnectionPair::new();
        assert_eq!(
            p.send(
                Role::Client,
                vec![Request::new(
                    b"GET".to_vec(),
                    vec![
                        (b"Host".to_vec(), b"example.com".to_vec()),
                        (b"Transfer-Encoding".to_vec(), b"chunked".to_vec())
                    ]
                    .into(),
                    b"/".to_vec(),
                    b"1.1".to_vec(),
                )
                .unwrap()
                .into(),],
                None,
            )
            .unwrap(),
            b"GET / HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec()
        );
        assert_eq!(
            p.send(
                Role::Client,
                vec![Data {
                    data: b"1234567890".to_vec(),
                    chunk_start: true,
                    chunk_end: true,
                }
                .into()],
                None,
            )
            .unwrap(),
            b"a\r\n1234567890\r\n".to_vec()
        );
        assert_eq!(
            p.send(
                Role::Client,
                vec![Data {
                    data: b"abcde".to_vec(),
                    chunk_start: true,
                    chunk_end: true,
                }
                .into()],
                None,
            )
            .unwrap(),
            b"5\r\nabcde\r\n".to_vec()
        );
        assert_eq!(
            p.send(Role::Client, vec![Data::default().into()], Some(vec![]),)
                .unwrap(),
            b"".to_vec()
        );
        assert_eq!(
            p.send(
                Role::Client,
                vec![EndOfMessage {
                    headers: vec![(b"hello".to_vec(), b"there".to_vec())].into(),
                }
                .into()],
                None,
            )
            .unwrap(),
            b"0\r\nhello: there\r\n\r\n".to_vec()
        );

        assert_eq!(
            p.send(
                Role::Server,
                vec![Response {
                    status_code: 200,
                    headers: vec![
                        (b"hello".to_vec(), b"there".to_vec()),
                        (b"transfer-encoding".to_vec(), b"chunked".to_vec()),
                    ]
                    .into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into()],
                None,
            )
            .unwrap(),
            b"HTTP/1.1 200 \r\nhello: there\r\ntransfer-encoding: chunked\r\n\r\n".to_vec()
        );
        assert_eq!(
            p.send(
                Role::Server,
                vec![Data {
                    data: b"54321".to_vec(),
                    chunk_start: true,
                    chunk_end: true,
                }
                .into()],
                None,
            )
            .unwrap(),
            b"5\r\n54321\r\n".to_vec()
        );
        assert_eq!(
            p.send(
                Role::Server,
                vec![Data {
                    data: b"12345".to_vec(),
                    chunk_start: true,
                    chunk_end: true,
                }
                .into()],
                None,
            )
            .unwrap(),
            b"5\r\n12345\r\n".to_vec()
        );
        assert_eq!(
            p.send(Role::Server, vec![EndOfMessage::default().into()], None,)
                .unwrap(),
            b"0\r\n\r\n".to_vec()
        );

        for (_, connection) in &p.conn {
            assert_eq!(
                connection.get_states(),
                HashMap::from([(Role::Client, State::Done), (Role::Server, State::Done),])
            );
        }
    }

    #[test]
    fn test_chunk_boundaries() {
        let mut conn = Connection::new(Role::Server, None);

        let request = b"POST / HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n";
        conn.receive_data(request).unwrap();
        assert_eq!(
            conn.next_event().unwrap(),
            Event::Request(Request {
                method: b"POST".to_vec(),
                target: b"/".to_vec(),
                headers: vec![
                    (b"Host".to_vec(), b"example.com".to_vec()),
                    (b"Transfer-Encoding".to_vec(), b"chunked".to_vec())
                ]
                .into(),
                http_version: b"1.1".to_vec(),
            })
        );
        assert_eq!(conn.next_event().unwrap(), Event::NeedData {});

        conn.receive_data(b"5\r\nhello\r\n").unwrap();
        assert_eq!(
            conn.next_event().unwrap(),
            Event::Data(Data {
                data: b"hello".to_vec(),
                chunk_start: true,
                chunk_end: true,
            })
        );

        conn.receive_data(b"5\r\nhel").unwrap();
        assert_eq!(
            conn.next_event().unwrap(),
            Event::Data(Data {
                data: b"hel".to_vec(),
                chunk_start: true,
                chunk_end: false,
            })
        );

        conn.receive_data(b"l").unwrap();
        assert_eq!(
            conn.next_event().unwrap(),
            Event::Data(Data {
                data: b"l".to_vec(),
                chunk_start: false,
                chunk_end: false,
            })
        );

        conn.receive_data(b"o\r\n").unwrap();
        assert_eq!(
            conn.next_event().unwrap(),
            Event::Data(Data {
                data: b"o".to_vec(),
                chunk_start: false,
                chunk_end: true,
            })
        );

        conn.receive_data(b"5\r\nhello").unwrap();
        assert_eq!(
            conn.next_event().unwrap(),
            Event::Data(Data {
                data: b"hello".to_vec(),
                chunk_start: true,
                chunk_end: true,
            })
        );

        conn.receive_data(b"\r\n").unwrap();
        assert_eq!(conn.next_event().unwrap(), Event::NeedData {});

        conn.receive_data(b"0\r\n\r\n").unwrap();
        assert_eq!(
            conn.next_event().unwrap(),
            Event::EndOfMessage(EndOfMessage {
                headers: vec![].into(),
            })
        );
    }

    // def test_client_talking_to_http10_server() -> None:
    //     c = Connection(CLIENT)
    //     c.send(Request(method="GET", target="/", headers=[("Host", "example.com")]))
    //     c.send(EndOfMessage())
    //     assert c.our_state is DONE
    //     # No content-length, so Http10 framing for body
    //     assert receive_and_get(c, b"HTTP/1.0 200 OK\r\n\r\n") == [
    //         Response(status_code=200, headers=[], http_version="1.0", reason=b"OK")  # type: ignore[arg-type]
    //     ]
    //     assert c.our_state is MUST_CLOSE
    //     assert receive_and_get(c, b"12345") == [Data(data=b"12345")]
    //     assert receive_and_get(c, b"67890") == [Data(data=b"67890")]
    //     assert receive_and_get(c, b"") == [EndOfMessage(), ConnectionClosed()]
    //     assert c.their_state is CLOSED

    #[test]
    fn test_client_talking_to_http10_server() {
        let mut c = Connection::new(Role::Client, None);
        c.send(
            Request::new(
                b"GET".to_vec(),
                vec![(b"Host".to_vec(), b"example.com".to_vec())].into(),
                b"/".to_vec(),
                b"1.1".to_vec(),
            )
            .unwrap()
            .into(),
        )
        .unwrap();
        c.send(EndOfMessage::default().into()).unwrap();
        assert_eq!(c.get_our_state(), State::Done);
        assert_eq!(
            receive_and_get(&mut c, b"HTTP/1.0 200 OK\r\n\r\n").unwrap(),
            vec![Event::NormalResponse(Response {
                status_code: 200,
                headers: vec![].into(),
                http_version: b"1.0".to_vec(),
                reason: b"OK".to_vec(),
            })],
        );
        assert_eq!(c.get_our_state(), State::MustClose);
        assert_eq!(
            receive_and_get(&mut c, b"12345").unwrap(),
            vec![Event::Data(Data {
                data: b"12345".to_vec(),
                chunk_start: false,
                chunk_end: false,
            })],
        );
        assert_eq!(
            receive_and_get(&mut c, b"67890").unwrap(),
            vec![Event::Data(Data {
                data: b"67890".to_vec(),
                chunk_start: false,
                chunk_end: false,
            })],
        );
        assert_eq!(
            receive_and_get(&mut c, b"").unwrap(),
            vec![
                Event::EndOfMessage(EndOfMessage::default()),
                Event::ConnectionClosed(ConnectionClosed::default()),
            ],
        );
        assert_eq!(c.get_their_state(), State::Closed);
    }

    // def test_server_talking_to_http10_client() -> None:
    //     c = Connection(SERVER)
    //     # No content-length, so no body
    //     # NB: no host header
    //     assert receive_and_get(c, b"GET / HTTP/1.0\r\n\r\n") == [
    //         Request(method="GET", target="/", headers=[], http_version="1.0"),  # type: ignore[arg-type]
    //         EndOfMessage(),
    //     ]
    //     assert c.their_state is MUST_CLOSE

    //     # We automatically Connection: close back at them
    //     assert (
    //         c.send(Response(status_code=200, headers=[]))  # type: ignore[arg-type]
    //         == b"HTTP/1.1 200 \r\nConnection: close\r\n\r\n"
    //     )

    //     assert c.send(Data(data=b"12345")) == b"12345"
    //     assert c.send(EndOfMessage()) == b""
    //     assert c.our_state is MUST_CLOSE

    //     # Check that it works if they do send Content-Length
    //     c = Connection(SERVER)
    //     # NB: no host header
    //     assert receive_and_get(c, b"POST / HTTP/1.0\r\nContent-Length: 10\r\n\r\n1") == [
    //         Request(
    //             method="POST",
    //             target="/",
    //             headers=[("Content-Length", "10")],
    //             http_version="1.0",
    //         ),
    //         Data(data=b"1"),
    //     ]
    //     assert receive_and_get(c, b"234567890") == [Data(data=b"234567890"), EndOfMessage()]
    //     assert c.their_state is MUST_CLOSE
    //     assert receive_and_get(c, b"") == [ConnectionClosed()]

    #[test]
    fn test_server_talking_to_http10_client() {
        let mut c = Connection::new(Role::Server, None);
        // No content-length, so no body
        // NB: no host header
        assert_eq!(
            receive_and_get(&mut c, b"GET / HTTP/1.0\r\n\r\n").unwrap(),
            vec![
                Event::Request(Request {
                    method: b"GET".to_vec(),
                    target: b"/".to_vec(),
                    headers: vec![].into(),
                    http_version: b"1.0".to_vec(),
                }),
                Event::EndOfMessage(EndOfMessage::default()),
            ],
        );
        assert_eq!(c.get_their_state(), State::MustClose);

        // We automatically Connection: close back at them
        assert_eq!(
            c.send(
                Response {
                    status_code: 200,
                    headers: vec![].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into()
            )
            .unwrap()
            .unwrap(),
            b"HTTP/1.1 200 \r\nconnection: close\r\n\r\n".to_vec()
        );

        assert_eq!(
            c.send(
                Data {
                    data: b"12345".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }
                .into()
            )
            .unwrap()
            .unwrap(),
            b"12345".to_vec()
        );
        assert_eq!(
            c.send(EndOfMessage::default().into()).unwrap().unwrap(),
            b"".to_vec()
        );
        assert_eq!(c.get_our_state(), State::MustClose);

        // Check that it works if they do send Content-Length
        let mut c = Connection::new(Role::Server, None);
        // NB: no host header
        assert_eq!(
            receive_and_get(&mut c, b"POST / HTTP/1.0\r\nContent-Length: 10\r\n\r\n1").unwrap(),
            vec![
                Event::Request(Request {
                    method: b"POST".to_vec(),
                    target: b"/".to_vec(),
                    headers: vec![(b"Content-Length".to_vec(), b"10".to_vec())].into(),
                    http_version: b"1.0".to_vec(),
                }),
                Event::Data(Data {
                    data: b"1".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }),
            ],
        );
        assert_eq!(
            receive_and_get(&mut c, b"234567890").unwrap(),
            vec![
                Event::Data(Data {
                    data: b"234567890".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }),
                Event::EndOfMessage(EndOfMessage::default()),
            ],
        );
        assert_eq!(c.get_their_state(), State::MustClose);
        assert_eq!(
            receive_and_get(&mut c, b"").unwrap(),
            vec![Event::ConnectionClosed(ConnectionClosed::default())],
        );
    }

    // def test_automatic_transfer_encoding_in_response() -> None:
    //     # Check that in responses, the user can specify either Transfer-Encoding:
    //     # chunked or no framing at all, and in both cases we automatically select
    //     # the right option depending on whether the peer speaks HTTP/1.0 or
    //     # HTTP/1.1
    //     for user_headers in [
    //         [("Transfer-Encoding", "chunked")],
    //         [],
    //         # In fact, this even works if Content-Length is set,
    //         # because if both are set then Transfer-Encoding wins
    //         [("Transfer-Encoding", "chunked"), ("Content-Length", "100")],
    //     ]:
    //         user_headers = cast(List[Tuple[str, str]], user_headers)
    //         p = ConnectionPair()
    //         p.send(
    //             CLIENT,
    //             [
    //                 Request(method="GET", target="/", headers=[("Host", "example.com")]),
    //                 EndOfMessage(),
    //             ],
    //         )
    //         # When speaking to HTTP/1.1 client, all of the above cases get
    //         # normalized to Transfer-Encoding: chunked
    //         p.send(
    //             SERVER,
    //             Response(status_code=200, headers=user_headers),
    //             expect=Response(
    //                 status_code=200, headers=[("Transfer-Encoding", "chunked")]
    //             ),
    //         )

    //         # When speaking to HTTP/1.0 client, all of the above cases get
    //         # normalized to no-framing-headers
    //         c = Connection(SERVER)
    //         receive_and_get(c, b"GET / HTTP/1.0\r\n\r\n")
    //         assert (
    //             c.send(Response(status_code=200, headers=user_headers))
    //             == b"HTTP/1.1 200 \r\nConnection: close\r\n\r\n"
    //         )
    //         assert c.send(Data(data=b"12345")) == b"12345"

    #[test]
    fn test_automatic_transfer_encoding_in_response() {
        // Check that in responses, the user can specify either Transfer-Encoding:
        // chunked or no framing at all, and in both cases we automatically select
        // the right option depending on whether the peer speaks HTTP/1.0 or
        // HTTP/1.1
        for user_headers in vec![
            vec![(b"Transfer-Encoding".to_vec(), b"chunked".to_vec())],
            vec![],
            // In fact, this even works if Content-Length is set,
            // because if both are set then Transfer-Encoding wins
            vec![
                (b"Transfer-Encoding".to_vec(), b"chunked".to_vec()),
                (b"Content-Length".to_vec(), b"100".to_vec()),
            ],
        ] {
            let mut p = ConnectionPair::new();
            p.send(
                Role::Client,
                vec![
                    Request::new(
                        b"GET".to_vec(),
                        vec![(b"Host".to_vec(), b"example.com".to_vec())].into(),
                        b"/".to_vec(),
                        b"1.1".to_vec(),
                    )
                    .unwrap()
                    .into(),
                    EndOfMessage::default().into(),
                ],
                None,
            )
            .unwrap();
            // When speaking to HTTP/1.1 client, all of the above cases get
            // normalized to Transfer-Encoding: chunked
            p.send(
                Role::Server,
                vec![Response {
                    status_code: 200,
                    headers: user_headers.clone().into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into()],
                Some(vec![Response {
                    status_code: 200,
                    headers: vec![(b"transfer-encoding".to_vec(), b"chunked".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into()]),
            )
            .unwrap();

            // When speaking to HTTP/1.0 client, all of the above cases get
            // normalized to no-framing-headers
            let mut c = Connection::new(Role::Server, None);
            receive_and_get(&mut c, b"GET / HTTP/1.0\r\n\r\n").unwrap();
            assert_eq!(
                c.send(
                    Response {
                        status_code: 200,
                        headers: user_headers.clone().into(),
                        http_version: b"1.1".to_vec(),
                        reason: b"".to_vec(),
                    }
                    .into()
                )
                .unwrap()
                .unwrap(),
                b"HTTP/1.1 200 \r\nconnection: close\r\n\r\n".to_vec()
            );
            assert_eq!(
                c.send(
                    Data {
                        data: b"12345".to_vec(),
                        chunk_start: false,
                        chunk_end: false,
                    }
                    .into()
                )
                .unwrap()
                .unwrap(),
                b"12345".to_vec()
            );
        }
    }

    // def test_automagic_connection_close_handling() -> None:
    //     p = ConnectionPair()
    //     # If the user explicitly sets Connection: close, then we notice and
    //     # respect it
    //     p.send(
    //         CLIENT,
    //         [
    //             Request(
    //                 method="GET",
    //                 target="/",
    //                 headers=[("Host", "example.com"), ("Connection", "close")],
    //             ),
    //             EndOfMessage(),
    //         ],
    //     )
    //     for conn in p.conns:
    //         assert conn.states[CLIENT] is MUST_CLOSE
    //     # And if the client sets it, the server automatically echoes it back
    //     p.send(
    //         SERVER,
    //         # no header here...
    //         [Response(status_code=204, headers=[]), EndOfMessage()],  # type: ignore[arg-type]
    //         # ...but oh look, it arrived anyway
    //         expect=[
    //             Response(status_code=204, headers=[("connection", "close")]),
    //             EndOfMessage(),
    //         ],
    //     )
    //     for conn in p.conns:
    //         assert conn.states == {CLIENT: MUST_CLOSE, SERVER: MUST_CLOSE}

    #[test]
    fn test_automagic_connection_close_handling() {
        let mut p = ConnectionPair::new();
        // If the user explicitly sets Connection: close, then we notice and
        // respect it
        p.send(
            Role::Client,
            vec![
                Request::new(
                    b"GET".to_vec(),
                    vec![
                        (b"Host".to_vec(), b"example.com".to_vec()),
                        (b"Connection".to_vec(), b"close".to_vec()),
                    ]
                    .into(),
                    b"/".to_vec(),
                    b"1.1".to_vec(),
                )
                .unwrap()
                .into(),
                EndOfMessage::default().into(),
            ],
            None,
        )
        .unwrap();
        for (_, connection) in &p.conn {
            assert_eq!(connection.get_states()[&Role::Client], State::MustClose);
        }
        // And if the client sets it, the server automatically echoes it back
        p.send(
            Role::Server,
            vec![
                Response {
                    status_code: 204,
                    headers: vec![].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into(),
                EndOfMessage::default().into(),
            ],
            Some(vec![
                Response {
                    status_code: 204,
                    headers: vec![(b"connection".to_vec(), b"close".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into(),
                EndOfMessage::default().into(),
            ]),
        )
        .unwrap();
        for (_, connection) in &p.conn {
            assert_eq!(
                connection.get_states(),
                HashMap::from([
                    (Role::Client, State::MustClose),
                    (Role::Server, State::MustClose),
                ])
            );
        }
    }

    // def test_100_continue() -> None:
    //     def setup() -> ConnectionPair:
    //         p = ConnectionPair()
    //         p.send(
    //             CLIENT,
    //             Request(
    //                 method="GET",
    //                 target="/",
    //                 headers=[
    //                     ("Host", "example.com"),
    //                     ("Content-Length", "100"),
    //                     ("Expect", "100-continue"),
    //                 ],
    //             ),
    //         )
    //         for conn in p.conns:
    //             assert conn.get_client_is_waiting_for_100_continue()
    //         assert not p.conn[CLIENT].they_are_waiting_for_100_continue
    //         assert p.conn[SERVER].they_are_waiting_for_100_continue
    //         return p

    //     # Disabled by 100 Continue
    //     p = setup()
    //     p.send(SERVER, InformationalResponse(status_code=100, headers=[]))  # type: ignore[arg-type]
    //     for conn in p.conns:
    //         assert not conn.get_client_is_waiting_for_100_continue()
    //         assert not conn.they_are_waiting_for_100_continue

    //     # Disabled by a real response
    //     p = setup()
    //     p.send(
    //         SERVER, Response(status_code=200, headers=[("Transfer-Encoding", "chunked")])
    //     )
    //     for conn in p.conns:
    //         assert not conn.get_client_is_waiting_for_100_continue()
    //         assert not conn.they_are_waiting_for_100_continue

    //     # Disabled by the client going ahead and sending stuff anyway
    //     p = setup()
    //     p.send(CLIENT, Data(data=b"12345"))
    //     for conn in p.conns:
    //         assert not conn.get_client_is_waiting_for_100_continue()
    //         assert not conn.they_are_waiting_for_100_continue

    #[test]
    fn test_100_continue() {
        fn setup() -> ConnectionPair {
            let mut p = ConnectionPair::new();
            p.send(
                Role::Client,
                vec![Request::new(
                    b"GET".to_vec(),
                    vec![
                        (b"Host".to_vec(), b"example.com".to_vec()),
                        (b"Content-Length".to_vec(), b"100".to_vec()),
                        (b"Expect".to_vec(), b"100-continue".to_vec()),
                    ]
                    .into(),
                    b"/".to_vec(),
                    b"1.1".to_vec(),
                )
                .unwrap()
                .into()],
                None,
            )
            .unwrap();
            for (_, connection) in &p.conn {
                assert!(connection.get_client_is_waiting_for_100_continue());
            }
            assert!(!p.conn[&Role::Client].get_they_are_waiting_for_100_continue());
            assert!(p.conn[&Role::Server].get_they_are_waiting_for_100_continue());
            p
        }

        // Disabled by 100 Continue
        let mut p = setup();
        p.send(
            Role::Server,
            vec![Response {
                status_code: 100,
                headers: vec![].into(),
                http_version: b"1.1".to_vec(),
                reason: b"".to_vec(),
            }
            .into()],
            None,
        )
        .unwrap();
        for (_, connection) in &p.conn {
            assert!(!connection.get_client_is_waiting_for_100_continue());
            assert!(!connection.get_they_are_waiting_for_100_continue());
        }

        // Disabled by a real response
        let mut p = setup();
        p.send(
            Role::Server,
            vec![Response {
                status_code: 200,
                headers: vec![(b"transfer-encoding".to_vec(), b"chunked".to_vec())].into(),
                http_version: b"1.1".to_vec(),
                reason: b"".to_vec(),
            }
            .into()],
            None,
        )
        .unwrap();
        for (_, connection) in &p.conn {
            assert!(!connection.get_client_is_waiting_for_100_continue());
            assert!(!connection.get_they_are_waiting_for_100_continue());
        }

        // Disabled by the client going ahead and sending stuff anyway
        let mut p = setup();
        p.send(
            Role::Client,
            vec![Data {
                data: b"12345".to_vec(),
                chunk_start: false,
                chunk_end: false,
            }
            .into()],
            None,
        )
        .unwrap();
        for (_, connection) in &p.conn {
            assert!(!connection.get_client_is_waiting_for_100_continue());
            assert!(!connection.get_they_are_waiting_for_100_continue());
        }

        // Disabled by the client going ahead and sending stuff anyway
        let mut p = setup();
        p.send(
            Role::Client,
            vec![Data {
                data: b"12345".to_vec(),
                chunk_start: false,
                chunk_end: false,
            }
            .into()],
            None,
        )
        .unwrap();
        for (_, connection) in &p.conn {
            assert!(!connection.get_client_is_waiting_for_100_continue());
            assert!(!connection.get_they_are_waiting_for_100_continue());
        }

        // Disabled by the client going ahead and sending stuff anyway
        let mut p = setup();
        p.send(
            Role::Client,
            vec![Data {
                data: b"12345".to_vec(),
                chunk_start: false,
                chunk_end: false,
            }
            .into()],
            None,
        )
        .unwrap();
        for (_, connection) in &p.conn {
            assert!(!connection.get_client_is_waiting_for_100_continue());
            assert!(!connection.get_they_are_waiting_for_100_continue());
        }
    }

    // def test_max_incomplete_event_size_countermeasure() -> None:
    //     # Infinitely long headers are definitely not okay
    //     c = Connection(SERVER)
    //     c.receive_data(b"GET / HTTP/1.0\r\nEndless: ")
    //     assert c.next_event() is NEED_DATA
    //     with pytest.raises(RemoteProtocolError):
    //         while True:
    //             c.receive_data(b"a" * 1024)
    //             c.next_event()

    //     # Checking that the same header is accepted / rejected depending on the
    //     # max_incomplete_event_size setting:
    //     c = Connection(SERVER, max_incomplete_event_size=5000)
    //     c.receive_data(b"GET / HTTP/1.0\r\nBig: ")
    //     c.receive_data(b"a" * 4000)
    //     c.receive_data(b"\r\n\r\n")
    //     assert get_all_events(c) == [
    //         Request(
    //             method="GET", target="/", http_version="1.0", headers=[("big", "a" * 4000)]
    //         ),
    //         EndOfMessage(),
    //     ]

    //     c = Connection(SERVER, max_incomplete_event_size=4000)
    //     c.receive_data(b"GET / HTTP/1.0\r\nBig: ")
    //     c.receive_data(b"a" * 4000)
    //     with pytest.raises(RemoteProtocolError):
    //         c.next_event()

    //     # Temporarily exceeding the size limit is fine, as long as its done with
    //     # complete events:
    //     c = Connection(SERVER, max_incomplete_event_size=5000)
    //     c.receive_data(b"GET / HTTP/1.0\r\nContent-Length: 10000")
    //     c.receive_data(b"\r\n\r\n" + b"a" * 10000)
    //     assert get_all_events(c) == [
    //         Request(
    //             method="GET",
    //             target="/",
    //             http_version="1.0",
    //             headers=[("Content-Length", "10000")],
    //         ),
    //         Data(data=b"a" * 10000),
    //         EndOfMessage(),
    //     ]

    //     c = Connection(SERVER, max_incomplete_event_size=100)
    //     # Two pipelined requests to create a way-too-big receive buffer... but
    //     # it's fine because we're not checking
    //     c.receive_data(
    //         b"GET /1 HTTP/1.1\r\nHost: a\r\n\r\n"
    //         b"GET /2 HTTP/1.1\r\nHost: b\r\n\r\n" + b"X" * 1000
    //     )
    //     assert get_all_events(c) == [
    //         Request(method="GET", target="/1", headers=[("host", "a")]),
    //         EndOfMessage(),
    //     ]
    //     # Even more data comes in, still no problem
    //     c.receive_data(b"X" * 1000)
    //     # We can respond and reuse to get the second pipelined request
    //     c.send(Response(status_code=200, headers=[]))  # type: ignore[arg-type]
    //     c.send(EndOfMessage())
    //     c.start_next_cycle()
    //     assert get_all_events(c) == [
    //         Request(method="GET", target="/2", headers=[("host", "b")]),
    //         EndOfMessage(),
    //     ]
    //     # But once we unpause and try to read the next message, and find that it's
    //     # incomplete and the buffer is *still* way too large, then *that's* a
    //     # problem:
    //     c.send(Response(status_code=200, headers=[]))  # type: ignore[arg-type]
    //     c.send(EndOfMessage())
    //     c.start_next_cycle()
    //     with pytest.raises(RemoteProtocolError):
    //         c.next_event()

    #[test]
    fn test_max_incomplete_event_size_countermeasure() {
        // Infinitely long headers are definitely not okay
        let mut c = Connection::new(Role::Server, Some(5000));
        c.receive_data(b"GET / HTTP/1.0\r\nEndless: ").unwrap();
        assert_eq!(c.next_event().unwrap(), Event::NeedData {});

        // Checking that the same header is accepted / rejected depending on the
        // max_incomplete_event_size setting:
        let mut c = Connection::new(Role::Server, Some(5000));
        c.receive_data(b"GET / HTTP/1.0\r\nBig: ").unwrap();
        c.receive_data(&vec![b'a'; 4000]).unwrap();
        c.receive_data(b"\r\n\r\n").unwrap();
        assert_eq!(
            get_all_events(&mut c).unwrap(),
            vec![
                Event::Request(Request {
                    method: b"GET".to_vec(),
                    target: b"/".to_vec(),
                    headers: vec![(b"Big".to_vec(), vec![b'a'; 4000])].into(),
                    http_version: b"1.0".to_vec(),
                }),
                Event::EndOfMessage(EndOfMessage::default()),
            ],
        );

        let mut c = Connection::new(Role::Server, Some(4000));
        c.receive_data(b"GET / HTTP/1.0\r\nBig: ").unwrap();
        c.receive_data(&vec![b'a'; 4000]).unwrap();
        assert!(match c.next_event().unwrap_err() {
            ProtocolError::RemoteProtocolError(_) => true,
            _ => false,
        });

        // Temporarily exceeding the size limit is fine, as long as its done with
        // complete events:
        let mut c = Connection::new(Role::Server, Some(5000));
        c.receive_data(b"GET / HTTP/1.0\r\nContent-Length: 10000")
            .unwrap();
        c.receive_data(b"\r\n\r\n").unwrap();
        c.receive_data(&vec![b'a'; 10000]).unwrap();
        assert_eq!(
            get_all_events(&mut c).unwrap(),
            vec![
                Event::Request(Request {
                    method: b"GET".to_vec(),
                    target: b"/".to_vec(),
                    headers: vec![(b"Content-Length".to_vec(), b"10000".to_vec())].into(),
                    http_version: b"1.0".to_vec(),
                }),
                Event::Data(Data {
                    data: vec![b'a'; 10000],
                    chunk_start: false,
                    chunk_end: false,
                }),
                Event::EndOfMessage(EndOfMessage::default()),
            ],
        );

        let mut c = Connection::new(Role::Server, Some(100));
        // Two pipelined requests to create a way-too-big receive buffer... but
        // it's fine because we're not checking
        c.receive_data(
            b"GET /1 HTTP/1.1\r\nHost: a\r\n\r\n"
                .to_vec()
                .into_iter()
                .chain(b"GET /2 HTTP/1.1\r\nHost: b\r\n\r\n".to_vec().into_iter())
                .chain(vec![b'X'; 1000].into_iter())
                .collect::<Vec<u8>>()
                .as_slice(),
        )
        .unwrap();
        assert_eq!(
            get_all_events(&mut c).unwrap(),
            vec![
                Event::Request(Request {
                    method: b"GET".to_vec(),
                    target: b"/1".to_vec(),
                    headers: vec![(b"Host".to_vec(), b"a".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                }),
                Event::EndOfMessage(EndOfMessage::default()),
            ],
        );
        // Even more data comes in, still no problem
        c.receive_data(&vec![b'X'; 1000]).unwrap();
        // We can respond and reuse to get the second pipelined request
        c.send(
            Response {
                status_code: 200,
                headers: vec![].into(),
                http_version: b"1.1".to_vec(),
                reason: b"".to_vec(),
            }
            .into(),
        )
        .unwrap();
        c.send(EndOfMessage::default().into()).unwrap();
        c.start_next_cycle().unwrap();
        assert_eq!(
            get_all_events(&mut c).unwrap(),
            vec![
                Event::Request(Request {
                    method: b"GET".to_vec(),
                    target: b"/2".to_vec(),
                    headers: vec![(b"Host".to_vec(), b"b".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                }),
                Event::EndOfMessage(EndOfMessage::default()),
            ],
        );
        // But once we unpause and try to read the next message, and find that it's
        // incomplete and the buffer is *still* way too large, then *that's* a
        // problem:
        c.send(
            Response {
                status_code: 200,
                headers: vec![].into(),
                http_version: b"1.1".to_vec(),
                reason: b"".to_vec(),
            }
            .into(),
        )
        .unwrap();
        c.send(EndOfMessage::default().into()).unwrap();
        c.start_next_cycle().unwrap();
        assert!(match c.next_event().unwrap_err() {
            ProtocolError::RemoteProtocolError(_) => true,
            _ => false,
        });

        // Check that we can still send data after this happens
        let mut c = Connection::new(Role::Server, Some(100));
        // Two pipelined requests to create a way-too-big receive buffer... but
        // it's fine because we're not checking
        c.receive_data(
            b"GET /1 HTTP/1.1\r\nHost: a\r\n\r\n"
                .to_vec()
                .into_iter()
                .chain(b"GET /2 HTTP/1.1\r\nHost: b\r\n\r\n".to_vec().into_iter())
                .chain(vec![b'X'; 1000].into_iter())
                .collect::<Vec<u8>>()
                .as_slice(),
        )
        .unwrap();
        assert_eq!(
            get_all_events(&mut c).unwrap(),
            vec![
                Event::Request(Request {
                    method: b"GET".to_vec(),
                    target: b"/1".to_vec(),
                    headers: vec![(b"Host".to_vec(), b"a".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                }),
                Event::EndOfMessage(EndOfMessage::default()),
            ],
        );
        // Even more data comes in, still no problem
        c.receive_data(&vec![b'X'; 1000]).unwrap();
        // We can respond and reuse to get the second pipelined request
        c.send(
            Response {
                status_code: 200,
                headers: vec![].into(),
                http_version: b"1.1".to_vec(),
                reason: b"".to_vec(),
            }
            .into(),
        )
        .unwrap();
        c.send(EndOfMessage::default().into()).unwrap();
        c.start_next_cycle().unwrap();
        assert_eq!(
            get_all_events(&mut c).unwrap(),
            vec![
                Event::Request(Request {
                    method: b"GET".to_vec(),
                    target: b"/2".to_vec(),
                    headers: vec![(b"Host".to_vec(), b"b".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                }),
                Event::EndOfMessage(EndOfMessage::default()),
            ],
        );
        // But once we unpause and try to read the next message, and find that it's
        // incomplete and the buffer is *still* way too large, then *that's* a
        // problem:
        c.send(
            Response {
                status_code: 200,
                headers: vec![].into(),
                http_version: b"1.1".to_vec(),
                reason: b"".to_vec(),
            }
            .into(),
        )
        .unwrap();
        c.send(EndOfMessage::default().into()).unwrap();
        c.start_next_cycle().unwrap();
        assert!(match c.next_event().unwrap_err() {
            ProtocolError::RemoteProtocolError(_) => true,
            _ => false,
        });
    }

    // def test_reuse_simple() -> None:
    //     p = ConnectionPair()
    //     p.send(
    //         CLIENT,
    //         [Request(method="GET", target="/", headers=[("Host", "a")]), EndOfMessage()],
    //     )
    //     p.send(
    //         SERVER,
    //         [
    //             Response(status_code=200, headers=[(b"transfer-encoding", b"chunked")]),
    //             EndOfMessage(),
    //         ],
    //     )
    //     for conn in p.conns:
    //         assert conn.states == {CLIENT: DONE, SERVER: DONE}
    //         conn.start_next_cycle()

    //     p.send(
    //         CLIENT,
    //         [
    //             Request(method="DELETE", target="/foo", headers=[("Host", "a")]),
    //             EndOfMessage(),
    //         ],
    //     )
    //     p.send(
    //         SERVER,
    //         [
    //             Response(status_code=404, headers=[(b"transfer-encoding", b"chunked")]),
    //             EndOfMessage(),
    //         ],
    //     )

    #[test]
    fn test_reuse_simple() {
        let mut p = ConnectionPair::new();
        p.send(
            Role::Client,
            vec![
                Request::new(
                    b"GET".to_vec(),
                    vec![(b"Host".to_vec(), b"a".to_vec())].into(),
                    b"/".to_vec(),
                    b"1.1".to_vec(),
                )
                .unwrap()
                .into(),
                EndOfMessage::default().into(),
            ],
            None,
        )
        .unwrap();
        p.send(
            Role::Server,
            vec![
                Response {
                    status_code: 200,
                    headers: vec![(b"transfer-encoding".to_vec(), b"chunked".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into(),
                EndOfMessage::default().into(),
            ],
            None,
        )
        .unwrap();
        for (_, connection) in &mut p.conn {
            assert_eq!(
                connection.get_states(),
                HashMap::from([(Role::Client, State::Done), (Role::Server, State::Done),])
            );
            connection.start_next_cycle().unwrap();
        }

        p.send(
            Role::Client,
            vec![
                Request::new(
                    b"DELETE".to_vec(),
                    vec![(b"Host".to_vec(), b"a".to_vec())].into(),
                    b"/foo".to_vec(),
                    b"1.1".to_vec(),
                )
                .unwrap()
                .into(),
                EndOfMessage::default().into(),
            ],
            None,
        )
        .unwrap();
        p.send(
            Role::Server,
            vec![
                Response {
                    status_code: 404,
                    headers: vec![(b"transfer-encoding".to_vec(), b"chunked".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into(),
                EndOfMessage::default().into(),
            ],
            None,
        )
        .unwrap();
    }

    // def test_pipelining() -> None:
    //     # Client doesn't support pipelining, so we have to do this by hand
    //     c = Connection(SERVER)
    //     assert c.next_event() is NEED_DATA
    //     # 3 requests all bunched up
    //     c.receive_data(
    //         b"GET /1 HTTP/1.1\r\nHost: a.com\r\nContent-Length: 5\r\n\r\n"
    //         b"12345"
    //         b"GET /2 HTTP/1.1\r\nHost: a.com\r\nContent-Length: 5\r\n\r\n"
    //         b"67890"
    //         b"GET /3 HTTP/1.1\r\nHost: a.com\r\n\r\n"
    //     )
    //     assert get_all_events(c) == [
    //         Request(
    //             method="GET",
    //             target="/1",
    //             headers=[("Host", "a.com"), ("Content-Length", "5")],
    //         ),
    //         Data(data=b"12345"),
    //         EndOfMessage(),
    //     ]
    //     assert c.their_state is DONE
    //     assert c.our_state is SEND_RESPONSE

    //     assert c.next_event() is PAUSED

    //     c.send(Response(status_code=200, headers=[]))  # type: ignore[arg-type]
    //     c.send(EndOfMessage())
    //     assert c.their_state is DONE
    //     assert c.our_state is DONE

    //     c.start_next_cycle()

    //     assert get_all_events(c) == [
    //         Request(
    //             method="GET",
    //             target="/2",
    //             headers=[("Host", "a.com"), ("Content-Length", "5")],
    //         ),
    //         Data(data=b"67890"),
    //         EndOfMessage(),
    //     ]
    //     assert c.next_event() is PAUSED
    //     c.send(Response(status_code=200, headers=[]))  # type: ignore[arg-type]
    //     c.send(EndOfMessage())
    //     c.start_next_cycle()

    //     assert get_all_events(c) == [
    //         Request(method="GET", target="/3", headers=[("Host", "a.com")]),
    //         EndOfMessage(),
    //     ]
    //     # Doesn't pause this time, no trailing data
    //     assert c.next_event() is NEED_DATA
    //     c.send(Response(status_code=200, headers=[]))  # type: ignore[arg-type]
    //     c.send(EndOfMessage())

    //     # Arrival of more data triggers pause
    //     assert c.next_event() is NEED_DATA
    //     c.receive_data(b"SADF")
    //     assert c.next_event() is PAUSED
    //     assert c.trailing_data == (b"SADF", False)
    //     # If EOF arrives while paused, we don't see that either:
    //     c.receive_data(b"")
    //     assert c.trailing_data == (b"SADF", True)
    //     assert c.next_event() is PAUSED
    //     c.receive_data(b"")
    //     assert c.next_event() is PAUSED

    #[test]
    fn test_pipelining() {
        // Client doesn't support pipelining, so we have to do this by hand
        let mut c = Connection::new(Role::Server, None);
        assert_eq!(c.next_event().unwrap(), Event::NeedData {});

        // 3 requests all bunched up
        c.receive_data(
            &vec![
                b"GET /1 HTTP/1.1\r\nHost: a.com\r\nContent-Length: 5\r\n\r\n".to_vec(),
                b"12345".to_vec(),
                b"GET /2 HTTP/1.1\r\nHost: a.com\r\nContent-Length: 5\r\n\r\n".to_vec(),
                b"67890".to_vec(),
                b"GET /3 HTTP/1.1\r\nHost: a.com\r\n\r\n".to_vec(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<u8>>(),
        )
        .unwrap();
        assert_eq!(
            get_all_events(&mut c).unwrap(),
            vec![
                Event::Request(Request {
                    method: b"GET".to_vec(),
                    target: b"/1".to_vec(),
                    headers: vec![
                        (b"Host".to_vec(), b"a.com".to_vec()),
                        (b"Content-Length".to_vec(), b"5".to_vec())
                    ]
                    .into(),
                    http_version: b"1.1".to_vec(),
                }),
                Event::Data(Data {
                    data: b"12345".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }),
                Event::EndOfMessage(EndOfMessage::default()),
            ],
        );
        assert_eq!(c.get_their_state(), State::Done);
        assert_eq!(c.get_our_state(), State::SendResponse);

        assert_eq!(c.next_event().unwrap(), Event::Paused {});

        c.send(
            Response {
                status_code: 200,
                headers: vec![].into(),
                http_version: b"1.1".to_vec(),
                reason: b"".to_vec(),
            }
            .into(),
        )
        .unwrap();
        c.send(EndOfMessage::default().into()).unwrap();
        assert_eq!(c.get_their_state(), State::Done);
        assert_eq!(c.get_our_state(), State::Done);

        c.start_next_cycle().unwrap();

        assert_eq!(
            get_all_events(&mut c).unwrap(),
            vec![
                Event::Request(Request {
                    method: b"GET".to_vec(),
                    target: b"/2".to_vec(),
                    headers: vec![
                        (b"Host".to_vec(), b"a.com".to_vec()),
                        (b"Content-Length".to_vec(), b"5".to_vec())
                    ]
                    .into(),
                    http_version: b"1.1".to_vec(),
                }),
                Event::Data(Data {
                    data: b"67890".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }),
                Event::EndOfMessage(EndOfMessage::default()),
            ],
        );
        assert_eq!(c.next_event().unwrap(), Event::Paused {});
        c.send(
            Response {
                status_code: 200,
                headers: vec![].into(),
                http_version: b"1.1".to_vec(),
                reason: b"".to_vec(),
            }
            .into(),
        )
        .unwrap();
        c.send(EndOfMessage::default().into()).unwrap();
        c.start_next_cycle().unwrap();

        assert_eq!(
            get_all_events(&mut c).unwrap(),
            vec![
                Event::Request(Request {
                    method: b"GET".to_vec(),
                    target: b"/3".to_vec(),
                    headers: vec![(b"Host".to_vec(), b"a.com".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                }),
                Event::EndOfMessage(EndOfMessage::default()),
            ],
        );
        // Doesn't pause this time, no trailing data
        assert_eq!(c.next_event().unwrap(), Event::NeedData {});
        c.send(
            Response {
                status_code: 200,
                headers: vec![].into(),
                http_version: b"1.1".to_vec(),
                reason: b"".to_vec(),
            }
            .into(),
        )
        .unwrap();
        c.send(EndOfMessage::default().into()).unwrap();

        // Arrival of more data triggers pause
        assert_eq!(c.next_event().unwrap(), Event::NeedData {});
        c.receive_data(b"SADF").unwrap();
        assert_eq!(c.next_event().unwrap(), Event::Paused {});
        assert_eq!(c.get_trailing_data(), (b"SADF".to_vec(), false));
        // If EOF arrives while paused, we don't see that either:
        c.receive_data(b"").unwrap();
        assert_eq!(c.get_trailing_data(), (b"SADF".to_vec(), true));
        assert_eq!(c.next_event().unwrap(), Event::Paused {});
        c.receive_data(b"").unwrap();
        assert_eq!(c.next_event().unwrap(), Event::Paused {});
    }

    // def test_protocol_switch() -> None:
    //     for req, deny, accept in [
    //         (
    //             Request(
    //                 method="CONNECT",
    //                 target="example.com:443",
    //                 headers=[("Host", "foo"), ("Content-Length", "1")],
    //             ),
    //             Response(status_code=404, headers=[(b"transfer-encoding", b"chunked")]),
    //             Response(status_code=200, headers=[(b"transfer-encoding", b"chunked")]),
    //         ),
    //         (
    //             Request(
    //                 method="GET",
    //                 target="/",
    //                 headers=[("Host", "foo"), ("Content-Length", "1"), ("Upgrade", "a, b")],
    //             ),
    //             Response(status_code=200, headers=[(b"transfer-encoding", b"chunked")]),
    //             InformationalResponse(status_code=101, headers=[("Upgrade", "a")]),
    //         ),
    //         (
    //             Request(
    //                 method="CONNECT",
    //                 target="example.com:443",
    //                 headers=[("Host", "foo"), ("Content-Length", "1"), ("Upgrade", "a, b")],
    //             ),
    //             Response(status_code=404, headers=[(b"transfer-encoding", b"chunked")]),
    //             # Accept CONNECT, not upgrade
    //             Response(status_code=200, headers=[(b"transfer-encoding", b"chunked")]),
    //         ),
    //         (
    //             Request(
    //                 method="CONNECT",
    //                 target="example.com:443",
    //                 headers=[("Host", "foo"), ("Content-Length", "1"), ("Upgrade", "a, b")],
    //             ),
    //             Response(status_code=404, headers=[(b"transfer-encoding", b"chunked")]),
    //             # Accept Upgrade, not CONNECT
    //             InformationalResponse(status_code=101, headers=[("Upgrade", "b")]),
    //         ),
    //     ]:

    //         def setup() -> ConnectionPair:
    //             p = ConnectionPair()
    //             p.send(CLIENT, req)
    //             # No switch-related state change stuff yet; the client has to
    //             # finish the request before that kicks in
    //             for conn in p.conns:
    //                 assert conn.states[CLIENT] is SEND_BODY
    //             p.send(CLIENT, [Data(data=b"1"), EndOfMessage()])
    //             for conn in p.conns:
    //                 assert conn.states[CLIENT] is MIGHT_SWITCH_PROTOCOL
    //             assert p.conn[SERVER].next_event() is PAUSED
    //             return p

    //         # Test deny case
    //         p = setup()
    //         p.send(SERVER, deny)
    //         for conn in p.conns:
    //             assert conn.states == {CLIENT: DONE, SERVER: SEND_BODY}
    //         p.send(SERVER, EndOfMessage())
    //         # Check that re-use is still allowed after a denial
    //         for conn in p.conns:
    //             conn.start_next_cycle()

    //         # Test accept case
    //         p = setup()
    //         p.send(SERVER, accept)
    //         for conn in p.conns:
    //             assert conn.states == {CLIENT: SWITCHED_PROTOCOL, SERVER: SWITCHED_PROTOCOL}
    //             conn.receive_data(b"123")
    //             assert conn.next_event() is PAUSED
    //             conn.receive_data(b"456")
    //             assert conn.next_event() is PAUSED
    //             assert conn.trailing_data == (b"123456", False)

    //         # Pausing in might-switch, then recovery
    //         # (weird artificial case where the trailing data actually is valid
    //         # HTTP for some reason, because this makes it easier to test the state
    //         # logic)
    //         p = setup()
    //         sc = p.conn[SERVER]
    //         sc.receive_data(b"GET / HTTP/1.0\r\n\r\n")
    //         assert sc.next_event() is PAUSED
    //         assert sc.trailing_data == (b"GET / HTTP/1.0\r\n\r\n", False)
    //         sc.send(deny)
    //         assert sc.next_event() is PAUSED
    //         sc.send(EndOfMessage())
    //         sc.start_next_cycle()
    //         assert get_all_events(sc) == [
    //             Request(method="GET", target="/", headers=[], http_version="1.0"),  # type: ignore[arg-type]
    //             EndOfMessage(),
    //         ]

    //         # When we're DONE, have no trailing data, and the connection gets
    //         # closed, we report ConnectionClosed(). When we're in might-switch or
    //         # switched, we don't.
    //         p = setup()
    //         sc = p.conn[SERVER]
    //         sc.receive_data(b"")
    //         assert sc.next_event() is PAUSED
    //         assert sc.trailing_data == (b"", True)
    //         p.send(SERVER, accept)
    //         assert sc.next_event() is PAUSED

    //         p = setup()
    //         sc = p.conn[SERVER]
    //         sc.receive_data(b"")
    //         assert sc.next_event() is PAUSED
    //         sc.send(deny)
    //         assert sc.next_event() == ConnectionClosed()

    //         # You can't send after switching protocols, or while waiting for a
    //         # protocol switch
    //         p = setup()
    //         with pytest.raises(LocalProtocolError):
    //             p.conn[CLIENT].send(
    //                 Request(method="GET", target="/", headers=[("Host", "a")])
    //             )
    //         p = setup()
    //         p.send(SERVER, accept)
    //         with pytest.raises(LocalProtocolError):
    //             p.conn[SERVER].send(Data(data=b"123"))

    #[test]
    fn test_protocol_switch() {
        for (req, deny, accept) in vec![
            (
                Request::new(
                    b"CONNECT".to_vec(),
                    vec![
                        (b"Host".to_vec(), b"foo".to_vec()),
                        (b"Content-Length".to_vec(), b"1".to_vec()),
                    ]
                    .into(),
                    b"example.com:443".to_vec(),
                    b"1.1".to_vec(),
                )
                .unwrap()
                .into(),
                Response {
                    status_code: 404,
                    headers: vec![(b"transfer-encoding".to_vec(), b"chunked".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into(),
                Response {
                    status_code: 200,
                    headers: vec![(b"transfer-encoding".to_vec(), b"chunked".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into(),
            ),
            (
                Request::new(
                    b"GET".to_vec(),
                    vec![
                        (b"Host".to_vec(), b"foo".to_vec()),
                        (b"Content-Length".to_vec(), b"1".to_vec()),
                        (b"Upgrade".to_vec(), b"a, b".to_vec()),
                    ]
                    .into(),
                    b"/".to_vec(),
                    b"1.1".to_vec(),
                )
                .unwrap()
                .into(),
                Response {
                    status_code: 200,
                    headers: vec![(b"transfer-encoding".to_vec(), b"chunked".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into(),
                Response {
                    status_code: 101,
                    headers: vec![(b"Upgrade".to_vec(), b"a".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into(),
            ),
            (
                Request::new(
                    b"CONNECT".to_vec(),
                    vec![
                        (b"Host".to_vec(), b"foo".to_vec()),
                        (b"Content-Length".to_vec(), b"1".to_vec()),
                        (b"Upgrade".to_vec(), b"a, b".to_vec()),
                    ]
                    .into(),
                    b"example.com:443".to_vec(),
                    b"1.1".to_vec(),
                )
                .unwrap()
                .into(),
                Response {
                    status_code: 404,
                    headers: vec![(b"transfer-encoding".to_vec(), b"chunked".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into(),
                // Accept CONNECT, not upgrade
                Response {
                    status_code: 200,
                    headers: vec![(b"transfer-encoding".to_vec(), b"chunked".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into(),
            ),
            (
                Request::new(
                    b"CONNECT".to_vec(),
                    vec![
                        (b"Host".to_vec(), b"foo".to_vec()),
                        (b"Content-Length".to_vec(), b"1".to_vec()),
                        (b"Upgrade".to_vec(), b"a, b".to_vec()),
                    ]
                    .into(),
                    b"example.com:443".to_vec(),
                    b"1.1".to_vec(),
                )
                .unwrap()
                .into(),
                Response {
                    status_code: 404,
                    headers: vec![(b"transfer-encoding".to_vec(), b"chunked".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into(),
                // Accept Upgrade, not CONNECT
                Response {
                    status_code: 101,
                    headers: vec![(b"Upgrade".to_vec(), b"b".to_vec())].into(),
                    http_version: b"1.1".to_vec(),
                    reason: b"".to_vec(),
                }
                .into(),
            ),
        ] {
            let req: Event = req;
            let deny: Event = deny;
            let accept: Event = accept;
            let setup = || {
                let mut p = ConnectionPair::new();
                p.send(Role::Client, vec![req.clone()], None).unwrap();
                // No switch-related state change stuff yet; the client has to
                // finish the request before that kicks in
                for (_, connection) in &mut p.conn {
                    assert_eq!(connection.get_states()[&Role::Client], State::SendBody);
                }
                p.send(
                    Role::Client,
                    vec![
                        Data {
                            data: b"1".to_vec(),
                            chunk_start: false,
                            chunk_end: false,
                        }
                        .into(),
                        EndOfMessage::default().into(),
                    ],
                    None,
                )
                .unwrap();
                for (_, connection) in &mut p.conn {
                    assert_eq!(
                        connection.get_states()[&Role::Client],
                        State::MightSwitchProtocol
                    );
                }
                assert_eq!(
                    p.conn.get_mut(&Role::Server).unwrap().next_event().unwrap(),
                    Event::Paused {}
                );
                return p;
            };

            // Test deny case
            let mut p = setup();
            p.send(Role::Server, vec![deny.clone()], None).unwrap();
            for (_, connection) in &mut p.conn {
                assert_eq!(
                    connection.get_states(),
                    HashMap::from([(Role::Client, State::Done), (Role::Server, State::SendBody)])
                );
            }
            p.send(Role::Server, vec![EndOfMessage::default().into()], None)
                .unwrap();
            // Check that re-use is still allowed after a denial
            for (_, connection) in &mut p.conn {
                connection.start_next_cycle().unwrap();
            }

            // Test accept case
            let mut p = setup();
            p.send(Role::Server, vec![accept.clone()], None).unwrap();
            for (_, connection) in &mut p.conn {
                assert_eq!(
                    connection.get_states(),
                    HashMap::from([
                        (Role::Client, State::SwitchedProtocol),
                        (Role::Server, State::SwitchedProtocol)
                    ])
                );
                connection.receive_data(b"123").unwrap();
                assert_eq!(connection.next_event().unwrap(), Event::Paused {});
                connection.receive_data(b"456").unwrap();
                assert_eq!(connection.next_event().unwrap(), Event::Paused {});
                assert_eq!(connection.get_trailing_data(), (b"123456".to_vec(), false));
            }

            // Pausing in might-switch, then recovery
            // (weird artificial case where the trailing data actually is valid
            // HTTP for some reason, because this makes it easier to test the state
            // logic)
            let mut p = setup();
            let sc = p.conn.get_mut(&Role::Server).unwrap();
            sc.receive_data(b"GET / HTTP/1.0\r\n\r\n").unwrap();
            assert_eq!(sc.next_event().unwrap(), Event::Paused {});
            assert_eq!(
                sc.get_trailing_data(),
                (b"GET / HTTP/1.0\r\n\r\n".to_vec(), false)
            );
            sc.send(deny.clone()).unwrap();
            assert_eq!(sc.next_event().unwrap(), Event::Paused {});
            sc.send(EndOfMessage::default().into()).unwrap();
            sc.start_next_cycle().unwrap();
            assert_eq!(
                get_all_events(sc).unwrap(),
                vec![
                    Event::Request(Request {
                        method: b"GET".to_vec(),
                        target: b"/".to_vec(),
                        headers: vec![].into(),
                        http_version: b"1.0".to_vec(),
                    }),
                    Event::EndOfMessage(EndOfMessage::default()),
                ],
            );

            // When we're DONE, have no trailing data, and the connection gets
            // closed, we report ConnectionClosed(). When we're in might-switch or
            // switched, we don't.
            let mut p = setup();
            {
                let sc = (p.conn).get_mut(&Role::Server).unwrap();
                sc.receive_data(b"").unwrap();
                assert_eq!(sc.next_event().unwrap(), Event::Paused {});
                assert_eq!(sc.get_trailing_data(), (b"".to_vec(), true));
            }
            p.send(Role::Server, vec![accept.clone()], None).unwrap();
            assert_eq!(
                (p.conn)
                    .get_mut(&Role::Server)
                    .unwrap()
                    .next_event()
                    .unwrap(),
                Event::Paused {}
            );

            let mut p = setup();
            let sc = p.conn.get_mut(&Role::Server).unwrap();
            sc.receive_data(b"").unwrap();
            assert_eq!(sc.next_event().unwrap(), Event::Paused {});
            sc.send(deny).unwrap();
            assert_eq!(
                sc.next_event().unwrap(),
                Event::ConnectionClosed(ConnectionClosed::default())
            );

            // You can't send after switching protocols, or while waiting for a
            // protocol switch
            let mut p = setup();
            let cc = p.conn.get_mut(&Role::Client).unwrap();
            assert!(match cc.send(
                Request::new(
                    b"GET".to_vec(),
                    vec![(b"Host".to_vec(), b"a".to_vec())].into(),
                    b"/".to_vec(),
                    b"1.1".to_vec(),
                )
                .unwrap()
                .into(),
            ) {
                Err(ProtocolError::LocalProtocolError(_)) => true,
                _ => false,
            });
            let mut p = setup();
            p.send(Role::Server, vec![accept], None).unwrap();
            let cc = p.conn.get_mut(&Role::Client).unwrap();
            assert!(match cc.send(
                Data {
                    data: b"123".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }
                .into(),
            ) {
                Err(ProtocolError::LocalProtocolError(_)) => true,
                _ => false,
            });
        }
    }

    // def test_close_simple() -> None:
    //     # Just immediately closing a new connection without anything having
    //     # happened yet.
    //     for who_shot_first, who_shot_second in [(CLIENT, SERVER), (SERVER, CLIENT)]:

    //         def setup() -> ConnectionPair:
    //             p = ConnectionPair()
    //             p.send(who_shot_first, ConnectionClosed())
    //             for conn in p.conns:
    //                 assert conn.states == {
    //                     who_shot_first: CLOSED,
    //                     who_shot_second: MUST_CLOSE,
    //                 }
    //             return p

    //         # You can keep putting b"" into a closed connection, and you keep
    //         # getting ConnectionClosed() out:
    //         p = setup()
    //         assert p.conn[who_shot_second].next_event() == ConnectionClosed()
    //         assert p.conn[who_shot_second].next_event() == ConnectionClosed()
    //         p.conn[who_shot_second].receive_data(b"")
    //         assert p.conn[who_shot_second].next_event() == ConnectionClosed()
    //         # Second party can close...
    //         p = setup()
    //         p.send(who_shot_second, ConnectionClosed())
    //         for conn in p.conns:
    //             assert conn.our_state is CLOSED
    //             assert conn.their_state is CLOSED
    //         # But trying to receive new data on a closed connection is a
    //         # RuntimeError (not ProtocolError, because the problem here isn't
    //         # violation of HTTP, it's violation of physics)
    //         p = setup()
    //         with pytest.raises(RuntimeError):
    //             p.conn[who_shot_second].receive_data(b"123")
    //         # And receiving new data on a MUST_CLOSE connection is a ProtocolError
    //         p = setup()
    //         p.conn[who_shot_first].receive_data(b"GET")
    //         with pytest.raises(RemoteProtocolError):
    //             p.conn[who_shot_first].next_event()

    #[test]
    fn test_close_simple() {
        // Just immediately closing a new connection without anything having
        // happened yet.
        for (who_shot_first, who_shot_second) in
            vec![(Role::Client, Role::Server), (Role::Server, Role::Client)]
        {
            let setup = || {
                let mut p = ConnectionPair::new();
                p.send(
                    who_shot_first,
                    vec![ConnectionClosed::default().into()],
                    None,
                )
                .unwrap();
                for (_, connection) in &mut p.conn {
                    assert_eq!(
                        connection.get_states(),
                        HashMap::from([
                            (who_shot_first, State::Closed),
                            (who_shot_second, State::MustClose)
                        ])
                    );
                }
                return p;
            };

            // You can keep putting b"" into a closed connection, and you keep
            // getting ConnectionClosed() out:
            let mut p = setup();
            assert_eq!(
                p.conn
                    .get_mut(&who_shot_second)
                    .unwrap()
                    .next_event()
                    .unwrap(),
                Event::ConnectionClosed(ConnectionClosed::default())
            );
            assert_eq!(
                p.conn
                    .get_mut(&who_shot_second)
                    .unwrap()
                    .next_event()
                    .unwrap(),
                Event::ConnectionClosed(ConnectionClosed::default())
            );
            p.conn
                .get_mut(&who_shot_second)
                .unwrap()
                .receive_data(b"")
                .unwrap();
            assert_eq!(
                p.conn
                    .get_mut(&who_shot_second)
                    .unwrap()
                    .next_event()
                    .unwrap(),
                Event::ConnectionClosed(ConnectionClosed::default())
            );
            // Second party can close...
            let mut p = setup();
            p.send(
                who_shot_second,
                vec![ConnectionClosed::default().into()],
                None,
            )
            .unwrap();
            for (_, connection) in &mut p.conn {
                assert_eq!(connection.get_our_state(), State::Closed);
                assert_eq!(connection.get_their_state(), State::Closed);
            }
            // But trying to receive new data on a closed connection is a
            // RuntimeError (not ProtocolError, because the problem here isn't
            // violation of HTTP, it's violation of physics)
            let mut p = setup();
            assert!(match p
                .conn
                .get_mut(&who_shot_second)
                .unwrap()
                .receive_data(b"123")
            {
                Err(_) => true,
                _ => false,
            });
            // And receiving new data on a MUST_CLOSE connection is a ProtocolError
            let mut p = setup();
            p.conn
                .get_mut(&who_shot_first)
                .unwrap()
                .receive_data(b"GET")
                .unwrap();
            assert!(match p
                .conn
                .get_mut(&who_shot_first)
                .unwrap()
                .next_event()
                .unwrap_err()
            {
                ProtocolError::RemoteProtocolError(_) => true,
                _ => false,
            });
        }
    }

    // def test_close_different_states() -> None:
    //     req = [
    //         Request(method="GET", target="/foo", headers=[("Host", "a")]),
    //         EndOfMessage(),
    //     ]
    //     resp = [
    //         Response(status_code=200, headers=[(b"transfer-encoding", b"chunked")]),
    //         EndOfMessage(),
    //     ]

    //     # Client before request
    //     p = ConnectionPair()
    //     p.send(CLIENT, ConnectionClosed())
    //     for conn in p.conns:
    //         assert conn.states == {CLIENT: CLOSED, SERVER: MUST_CLOSE}

    //     # Client after request
    //     p = ConnectionPair()
    //     p.send(CLIENT, req)
    //     p.send(CLIENT, ConnectionClosed())
    //     for conn in p.conns:
    //         assert conn.states == {CLIENT: CLOSED, SERVER: SEND_RESPONSE}

    //     # Server after request -> not allowed
    //     p = ConnectionPair()
    //     p.send(CLIENT, req)
    //     with pytest.raises(LocalProtocolError):
    //         p.conn[SERVER].send(ConnectionClosed())
    //     p.conn[CLIENT].receive_data(b"")
    //     with pytest.raises(RemoteProtocolError):
    //         p.conn[CLIENT].next_event()

    //     # Server after response
    //     p = ConnectionPair()
    //     p.send(CLIENT, req)
    //     p.send(SERVER, resp)
    //     p.send(SERVER, ConnectionClosed())
    //     for conn in p.conns:
    //         assert conn.states == {CLIENT: MUST_CLOSE, SERVER: CLOSED}

    //     # Both after closing (ConnectionClosed() is idempotent)
    //     p = ConnectionPair()
    //     p.send(CLIENT, req)
    //     p.send(SERVER, resp)
    //     p.send(CLIENT, ConnectionClosed())
    //     p.send(SERVER, ConnectionClosed())
    //     p.send(CLIENT, ConnectionClosed())
    //     p.send(SERVER, ConnectionClosed())

    //     # In the middle of sending -> not allowed
    //     p = ConnectionPair()
    //     p.send(
    //         CLIENT,
    //         Request(
    //             method="GET", target="/", headers=[("Host", "a"), ("Content-Length", "10")]
    //         ),
    //     )
    //     with pytest.raises(LocalProtocolError):
    //         p.conn[CLIENT].send(ConnectionClosed())
    //     p.conn[SERVER].receive_data(b"")
    //     with pytest.raises(RemoteProtocolError):
    //         p.conn[SERVER].next_event()

    #[test]
    fn test_close_different_states() {
        let req: Vec<Event> = vec![
            Request::new(
                b"GET".to_vec(),
                vec![(b"Host".to_vec(), b"a".to_vec())].into(),
                b"/foo".to_vec(),
                b"1.1".to_vec(),
            )
            .unwrap()
            .into(),
            EndOfMessage::default().into(),
        ];
        let resp: Vec<Event> = vec![
            Response {
                status_code: 200,
                headers: vec![(b"transfer-encoding".to_vec(), b"chunked".to_vec())].into(),
                http_version: b"1.1".to_vec(),
                reason: b"".to_vec(),
            }
            .into(),
            EndOfMessage::default().into(),
        ];

        // Client before request
        let mut p = ConnectionPair::new();
        p.send(Role::Client, vec![ConnectionClosed::default().into()], None)
            .unwrap();
        for (_, connection) in &mut p.conn {
            assert_eq!(
                connection.get_states(),
                HashMap::from([
                    (Role::Client, State::Closed),
                    (Role::Server, State::MustClose)
                ])
            );
        }

        // Client after request
        let mut p = ConnectionPair::new();
        p.send(Role::Client, req.clone(), None).unwrap();
        p.send(Role::Client, vec![ConnectionClosed::default().into()], None)
            .unwrap();
        for (_, connection) in &mut p.conn {
            assert_eq!(
                connection.get_states(),
                HashMap::from([
                    (Role::Client, State::Closed),
                    (Role::Server, State::SendResponse)
                ])
            );
        }

        // Server after request -> not allowed
        let mut p = ConnectionPair::new();
        p.send(Role::Client, req.clone(), None).unwrap();
        assert!(match p
            .conn
            .get_mut(&Role::Server)
            .unwrap()
            .send(ConnectionClosed::default().into())
        {
            Err(ProtocolError::LocalProtocolError(_)) => true,
            _ => false,
        });
        p.conn
            .get_mut(&Role::Client)
            .unwrap()
            .receive_data(b"")
            .unwrap();
        assert!(match p
            .conn
            .get_mut(&Role::Client)
            .unwrap()
            .next_event()
            .unwrap_err()
        {
            ProtocolError::RemoteProtocolError(_) => true,
            ProtocolError::LocalProtocolError(m) => panic!("{:?}", m),
        });

        // Server after response
        let mut p = ConnectionPair::new();
        p.send(Role::Client, req.clone(), None).unwrap();
        p.send(Role::Server, resp.clone(), None).unwrap();
        p.send(Role::Server, vec![ConnectionClosed::default().into()], None)
            .unwrap();
        for (_, connection) in &mut p.conn {
            assert_eq!(
                connection.get_states(),
                HashMap::from([
                    (Role::Client, State::MustClose),
                    (Role::Server, State::Closed)
                ])
            );
        }

        // Both after closing (ConnectionClosed() is idempotent)
        let mut p = ConnectionPair::new();
        p.send(Role::Client, req.clone(), None).unwrap();
        p.send(Role::Server, resp.clone(), None).unwrap();
        p.send(Role::Client, vec![ConnectionClosed::default().into()], None)
            .unwrap();
        p.send(Role::Server, vec![ConnectionClosed::default().into()], None)
            .unwrap();
        p.send(Role::Client, vec![ConnectionClosed::default().into()], None)
            .unwrap();
        p.send(Role::Server, vec![ConnectionClosed::default().into()], None)
            .unwrap();

        // In the middle of sending -> not allowed
        let mut p = ConnectionPair::new();
        p.send(
            Role::Client,
            vec![Request::new(
                b"GET".to_vec(),
                vec![
                    (b"Host".to_vec(), b"a".to_vec()),
                    (b"Content-Length".to_vec(), b"10".to_vec()),
                ]
                .into(),
                b"/".to_vec(),
                b"1.1".to_vec(),
            )
            .unwrap()
            .into()],
            None,
        )
        .unwrap();
        assert!(match p
            .conn
            .get_mut(&Role::Client)
            .unwrap()
            .send(ConnectionClosed::default().into())
        {
            Err(ProtocolError::LocalProtocolError(_)) => true,
            _ => false,
        });
        p.conn
            .get_mut(&Role::Server)
            .unwrap()
            .receive_data(b"")
            .unwrap();
        assert!(match p
            .conn
            .get_mut(&Role::Server)
            .unwrap()
            .next_event()
            .unwrap_err()
        {
            ProtocolError::RemoteProtocolError(_) => true,
            _ => false,
        });
    }

    // # Receive several requests and then client shuts down their side of the
    // # connection; we can respond to each
    // def test_pipelined_close() -> None:
    //     c = Connection(SERVER)
    //     # 2 requests then a close
    //     c.receive_data(
    //         b"GET /1 HTTP/1.1\r\nHost: a.com\r\nContent-Length: 5\r\n\r\n"
    //         b"12345"
    //         b"GET /2 HTTP/1.1\r\nHost: a.com\r\nContent-Length: 5\r\n\r\n"
    //         b"67890"
    //     )
    //     c.receive_data(b"")
    //     assert get_all_events(c) == [
    //         Request(
    //             method="GET",
    //             target="/1",
    //             headers=[("host", "a.com"), ("content-length", "5")],
    //         ),
    //         Data(data=b"12345"),
    //         EndOfMessage(),
    //     ]
    //     assert c.states[CLIENT] is DONE
    //     c.send(Response(status_code=200, headers=[]))  # type: ignore[arg-type]
    //     c.send(EndOfMessage())
    //     assert c.states[SERVER] is DONE
    //     c.start_next_cycle()
    //     assert get_all_events(c) == [
    //         Request(
    //             method="GET",
    //             target="/2",
    //             headers=[("host", "a.com"), ("content-length", "5")],
    //         ),
    //         Data(data=b"67890"),
    //         EndOfMessage(),
    //         ConnectionClosed(),
    //     ]
    //     assert c.states == {CLIENT: CLOSED, SERVER: SEND_RESPONSE}
    //     c.send(Response(status_code=200, headers=[]))  # type: ignore[arg-type]
    //     c.send(EndOfMessage())
    //     assert c.states == {CLIENT: CLOSED, SERVER: MUST_CLOSE}
    //     c.send(ConnectionClosed())
    //     assert c.states == {CLIENT: CLOSED, SERVER: CLOSED}

    #[test]
    fn test_pipelined_close() {
        let mut c = Connection::new(Role::Server, None);
        // 2 requests then a close
        c.receive_data(
            &vec![
                b"GET /1 HTTP/1.1\r\nHost: a.com\r\nContent-Length: 5\r\n\r\n".to_vec(),
                b"12345".to_vec(),
                b"GET /2 HTTP/1.1\r\nHost: a.com\r\nContent-Length: 5\r\n\r\n".to_vec(),
                b"67890".to_vec(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<u8>>(),
        )
        .unwrap();
        c.receive_data(b"").unwrap();
        assert_eq!(
            get_all_events(&mut c).unwrap(),
            vec![
                Event::Request(Request {
                    method: b"GET".to_vec(),
                    target: b"/1".to_vec(),
                    headers: vec![
                        (b"Host".to_vec(), b"a.com".to_vec()),
                        (b"Content-Length".to_vec(), b"5".to_vec())
                    ]
                    .into(),
                    http_version: b"1.1".to_vec(),
                }),
                Event::Data(Data {
                    data: b"12345".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }),
                Event::EndOfMessage(EndOfMessage::default()),
            ],
        );
        assert_eq!(c.get_their_state(), State::Done);

        c.send(
            Response {
                status_code: 200,
                headers: vec![].into(),
                http_version: b"1.1".to_vec(),
                reason: b"".to_vec(),
            }
            .into(),
        )
        .unwrap();
        c.send(EndOfMessage::default().into()).unwrap();
        assert_eq!(c.get_our_state(), State::Done);

        c.start_next_cycle().unwrap();
        assert_eq!(
            get_all_events(&mut c).unwrap(),
            vec![
                Event::Request(Request {
                    method: b"GET".to_vec(),
                    target: b"/2".to_vec(),
                    headers: vec![
                        (b"Host".to_vec(), b"a.com".to_vec()),
                        (b"Content-Length".to_vec(), b"5".to_vec())
                    ]
                    .into(),
                    http_version: b"1.1".to_vec(),
                }),
                Event::Data(Data {
                    data: b"67890".to_vec(),
                    chunk_start: false,
                    chunk_end: false,
                }),
                Event::EndOfMessage(EndOfMessage::default()),
                Event::ConnectionClosed(ConnectionClosed::default()),
            ],
        );
        assert_eq!(c.get_their_state(), State::Closed);
        assert_eq!(c.get_our_state(), State::SendResponse);
        c.send(
            Response {
                status_code: 200,
                headers: vec![].into(),
                http_version: b"1.1".to_vec(),
                reason: b"".to_vec(),
            }
            .into(),
        )
        .unwrap();
        c.send(EndOfMessage::default().into()).unwrap();
        assert_eq!(c.get_their_state(), State::Closed);
        assert_eq!(c.get_our_state(), State::MustClose);
        c.send(ConnectionClosed::default().into()).unwrap();
        assert_eq!(c.get_their_state(), State::Closed);
        assert_eq!(c.get_our_state(), State::Closed);
    }

    // def test_errors() -> None:
    //     # After a receive error, you can't receive
    //     for role in [CLIENT, SERVER]:
    //         c = Connection(our_role=role)
    //         c.receive_data(b"gibberish\r\n\r\n")
    //         with pytest.raises(RemoteProtocolError):
    //             c.next_event()
    //         # Now any attempt to receive continues to raise
    //         assert c.their_state is ERROR
    //         assert c.our_state is not ERROR
    //         print(c._cstate.states)
    //         with pytest.raises(RemoteProtocolError):
    //             c.next_event()
    //         # But we can still yell at the client for sending us gibberish
    //         if role is SERVER:
    //             assert (
    //                 c.send(Response(status_code=400, headers=[]))  # type: ignore[arg-type]
    //                 == b"HTTP/1.1 400 \r\nConnection: close\r\n\r\n"
    //             )

    //     # After an error sending, you can no longer send
    //     # (This is especially important for things like content-length errors,
    //     # where there's complex internal state being modified)
    //     def conn(role: Type[Sentinel]) -> Connection:
    //         c = Connection(our_role=role)
    //         if role is SERVER:
    //             # Put it into the state where it *could* send a response...
    //             receive_and_get(c, b"GET / HTTP/1.0\r\n\r\n")
    //             assert c.our_state is SEND_RESPONSE
    //         return c

    //     for role in [CLIENT, SERVER]:
    //         if role is CLIENT:
    //             # This HTTP/1.0 request won't be detected as bad until after we go
    //             # through the state machine and hit the writing code
    //             good = Request(method="GET", target="/", headers=[("Host", "example.com")])
    //             bad = Request(
    //                 method="GET",
    //                 target="/",
    //                 headers=[("Host", "example.com")],
    //                 http_version="1.0",
    //             )
    //         elif role is SERVER:
    //             good = Response(status_code=200, headers=[])  # type: ignore[arg-type,assignment]
    //             bad = Response(status_code=200, headers=[], http_version="1.0")  # type: ignore[arg-type,assignment]
    //         # Make sure 'good' actually is good
    //         c = conn(role)
    //         c.send(good)
    //         assert c.our_state is not ERROR
    //         # Do that again, but this time sending 'bad' first
    //         c = conn(role)
    //         with pytest.raises(LocalProtocolError):
    //             c.send(bad)
    //         assert c.our_state is ERROR
    //         assert c.their_state is not ERROR
    //         # Now 'good' is not so good
    //         with pytest.raises(LocalProtocolError):
    //             c.send(good)

    //         # And check send_failed() too
    //         c = conn(role)
    //         c.send_failed()
    //         assert c.our_state is ERROR
    //         assert c.their_state is not ERROR
    //         # This is idempotent
    //         c.send_failed()
    //         assert c.our_state is ERROR
    //         assert c.their_state is not ERROR

    #[test]
    fn test_errors() {
        // After a receive error, you can't receive
        for role in vec![Role::Client, Role::Server] {
            let mut c = Connection::new(role, None);
            c.receive_data(b"gibberish\r\n\r\n").unwrap();
            assert!(match c.next_event().unwrap_err() {
                ProtocolError::RemoteProtocolError(_) => true,
                _ => false,
            });
            // Now any attempt to receive continues to raise
            assert_eq!(c.get_their_state(), State::Error);
            assert_ne!(c.get_our_state(), State::Error);
            assert!(match c.next_event().unwrap_err() {
                ProtocolError::RemoteProtocolError(_) => true,
                _ => false,
            });
            // But we can still yell at the client for sending us gibberish
            if role == Role::Server {
                assert_eq!(
                    c.send(
                        Response {
                            status_code: 400,
                            headers: vec![].into(),
                            http_version: b"1.1".to_vec(),
                            reason: b"".to_vec(),
                        }
                        .into()
                    )
                    .unwrap()
                    .unwrap(),
                    b"HTTP/1.1 400 \r\nconnection: close\r\n\r\n".to_vec()
                );
            }
        }

        // After an error sending, you can no longer send
        // (This is especially important for things like content-length errors,
        // where there's complex internal state being modified)
        let conn = |role: Role| -> Connection {
            let mut c = Connection::new(role, None);
            if role == Role::Server {
                // Put it into the state where it *could* send a response...
                receive_and_get(
                    &mut c,
                    &b"GET / HTTP/1.0\r\n\r\n"
                        .to_vec()
                        .into_iter()
                        .collect::<Vec<u8>>(),
                )
                .unwrap();
                assert_eq!(c.get_our_state(), State::SendResponse);
            }
            return c;
        };

        for role in vec![Role::Client, Role::Server] {
            let (good, bad): (Event, Event) = if role == Role::Client {
                // This HTTP/1.0 request won't be detected as bad until after we go
                // through the state machine and hit the writing code
                (
                    Request::new(
                        b"GET".to_vec(),
                        vec![(b"Host".to_vec(), b"example.com".to_vec())].into(),
                        b"/".to_vec(),
                        b"1.1".to_vec(),
                    )
                    .unwrap()
                    .into(),
                    Request::new(
                        b"GET".to_vec(),
                        vec![(b"Host".to_vec(), b"example.com".to_vec())].into(),
                        b"/".to_vec(),
                        b"1.0".to_vec(),
                    )
                    .unwrap()
                    .into(),
                )
            } else {
                (
                    Response {
                        status_code: 200,
                        headers: vec![].into(),
                        http_version: b"1.1".to_vec(),
                        reason: b"".to_vec(),
                    }
                    .into(),
                    Response {
                        status_code: 200,
                        headers: vec![].into(),
                        http_version: b"1.0".to_vec(),
                        reason: b"".to_vec(),
                    }
                    .into(),
                )
            };
            // Make sure 'good' actually is good
            let mut c = conn(role);
            c.send(good.clone()).unwrap();
            assert_ne!(c.get_our_state(), State::Error);
            // Do that again, but this time sending 'bad' first
            let mut c = conn(role);
            assert!(match c.send(bad.clone()) {
                Err(ProtocolError::LocalProtocolError(_)) => true,
                _ => false,
            });
            assert_eq!(c.get_our_state(), State::Error);
            assert_ne!(c.get_their_state(), State::Error);
            // Now 'good' is not so good
            assert!(match c.send(good.clone()) {
                Err(ProtocolError::LocalProtocolError(_)) => true,
                _ => false,
            });

            // And check send_failed() too
            let mut c = conn(role);
            c.send_failed();
            assert_eq!(c.get_our_state(), State::Error);
            assert_ne!(c.get_their_state(), State::Error);
            // This is idempotent
            c.send_failed();
            assert_eq!(c.get_our_state(), State::Error);
            assert_ne!(c.get_their_state(), State::Error);
        }
    }

    // def test_idle_receive_nothing() -> None:
    //     # At one point this incorrectly raised an error
    //     for role in [CLIENT, SERVER]:
    //         c = Connection(role)
    //         assert c.next_event() is NEED_DATA

    #[test]
    fn test_idle_receive_nothing() {
        // At one point this incorrectly raised an error
        for role in vec![Role::Client, Role::Server] {
            let mut c = Connection::new(role, None);
            assert_eq!(c.next_event().unwrap(), Event::NeedData {});
        }
    }

    // def test_connection_drop() -> None:
    //     c = Connection(SERVER)
    //     c.receive_data(b"GET /")
    //     assert c.next_event() is NEED_DATA
    //     c.receive_data(b"")
    //     with pytest.raises(RemoteProtocolError):
    //         c.next_event()

    #[test]
    fn test_connection_drop() {
        let mut c = Connection::new(Role::Server, None);
        c.receive_data(b"GET /").unwrap();
        assert_eq!(c.next_event().unwrap(), Event::NeedData {});
        c.receive_data(b"").unwrap();
        assert!(match c.next_event().unwrap_err() {
            ProtocolError::RemoteProtocolError(_) => true,
            _ => false,
        });
    }

    // def test_408_request_timeout() -> None:
    //     # Should be able to send this spontaneously as a server without seeing
    //     # anything from client
    //     p = ConnectionPair()
    //     p.send(SERVER, Response(status_code=408, headers=[(b"connection", b"close")]))

    #[test]
    fn test_408_request_timeout() {
        // Should be able to send this spontaneously as a server without seeing
        // anything from client
        let mut p = ConnectionPair::new();
        p.send(
            Role::Server,
            vec![Response {
                status_code: 408,
                headers: vec![(b"connection".to_vec(), b"close".to_vec())].into(),
                http_version: b"1.1".to_vec(),
                reason: b"".to_vec(),
            }
            .into()],
            None,
        )
        .unwrap();
    }

    // # This used to raise IndexError
    // def test_empty_request() -> None:
    //     c = Connection(SERVER)
    //     c.receive_data(b"\r\n")
    //     with pytest.raises(RemoteProtocolError):
    //         c.next_event()

    #[test]
    fn test_empty_request() {
        let mut c = Connection::new(Role::Server, None);
        c.receive_data(b"\r\n").unwrap();
        assert!(match c.next_event().unwrap_err() {
            ProtocolError::RemoteProtocolError(_) => true,
            _ => false,
        });
    }

    // # This used to raise IndexError
    // def test_empty_response() -> None:
    //     c = Connection(CLIENT)
    //     c.send(Request(method="GET", target="/", headers=[("Host", "a")]))
    //     c.receive_data(b"\r\n")
    //     with pytest.raises(RemoteProtocolError):
    //         c.next_event()

    #[test]
    fn test_empty_response() {
        let mut c = Connection::new(Role::Client, None);
        c.send(
            Request::new(
                b"GET".to_vec(),
                vec![(b"Host".to_vec(), b"a".to_vec())].into(),
                b"/".to_vec(),
                b"1.1".to_vec(),
            )
            .unwrap()
            .into(),
        )
        .unwrap();
        c.receive_data(b"\r\n").unwrap();
        assert!(match c.next_event().unwrap_err() {
            ProtocolError::RemoteProtocolError(_) => true,
            _ => false,
        });
    }

    // @pytest.mark.parametrize(
    //     "data",
    //     [
    //         b"\x00",
    //         b"\x20",
    //         b"\x16\x03\x01\x00\xa5",  # Typical start of a TLS Client Hello
    //     ],
    // )
    // def test_early_detection_of_invalid_request(data: bytes) -> None:
    //     c = Connection(SERVER)
    //     # Early detection should occur before even receiving a `\r\n`
    //     c.receive_data(data)
    //     with pytest.raises(RemoteProtocolError):
    //         c.next_event()

    #[test]
    fn test_early_detection_of_invalid_request() {
        let data = vec![
            b"\x00".to_vec(),
            b"\x20".to_vec(),
            b"\x16\x03\x01\x00\xa5".to_vec(), // Typical start of a TLS Client Hello
        ];
        for data in data {
            let mut c = Connection::new(Role::Server, None);
            // Early detection should occur before even receiving a `\r\n`
            c.receive_data(&data).unwrap();
            assert!(match c.next_event().unwrap_err() {
                ProtocolError::RemoteProtocolError(_) => true,
                _ => false,
            });
        }
    }

    // @pytest.mark.parametrize(
    //     "data",
    //     [
    //         b"\x00",
    //         b"\x20",
    //         b"\x16\x03\x03\x00\x31",  # Typical start of a TLS Server Hello
    //     ],
    // )
    // def test_early_detection_of_invalid_response(data: bytes) -> None:
    //     c = Connection(CLIENT)
    //     # Early detection should occur before even receiving a `\r\n`
    //     c.receive_data(data)
    //     with pytest.raises(RemoteProtocolError):
    //         c.next_event()

    #[test]
    fn test_early_detection_of_invalid_response() {
        let data = vec![
            b"\x00".to_vec(),
            b"\x20".to_vec(),
            b"\x16\x03\x03\x00\x31".to_vec(), // Typical start of a TLS Server Hello
        ];
        for data in data {
            let mut c = Connection::new(Role::Client, None);
            // Early detection should occur before even receiving a `\r\n`
            c.receive_data(&data).unwrap();
            assert!(match c.next_event().unwrap_err() {
                ProtocolError::RemoteProtocolError(_) => true,
                _ => false,
            });
        }
    }

    // # This used to give different headers for HEAD and GET.
    // # The correct way to handle HEAD is to put whatever headers we *would* have
    // # put if it were a GET -- even though we know that for HEAD, those headers
    // # will be ignored.
    // def test_HEAD_framing_headers() -> None:
    //     def setup(method: bytes, http_version: bytes) -> Connection:
    //         c = Connection(SERVER)
    //         c.receive_data(
    //             method + b" / HTTP/" + http_version + b"\r\n" + b"Host: example.com\r\n\r\n"
    //         )
    //         assert type(c.next_event()) is Request
    //         assert type(c.next_event()) is EndOfMessage
    //         return c

    //     for method in [b"GET", b"HEAD"]:
    //         # No Content-Length, HTTP/1.1 peer, should use chunked
    //         c = setup(method, b"1.1")
    //         assert (
    //             c.send(Response(status_code=200, headers=[])) == b"HTTP/1.1 200 \r\n"  # type: ignore[arg-type]
    //             b"Transfer-Encoding: chunked\r\n\r\n"
    //         )

    //         # No Content-Length, HTTP/1.0 peer, frame with connection: close
    //         c = setup(method, b"1.0")
    //         assert (
    //             c.send(Response(status_code=200, headers=[])) == b"HTTP/1.1 200 \r\n"  # type: ignore[arg-type]
    //             b"Connection: close\r\n\r\n"
    //         )

    //         # Content-Length + Transfer-Encoding, TE wins
    //         c = setup(method, b"1.1")
    //         assert (
    //             c.send(
    //                 Response(
    //                     status_code=200,
    //                     headers=[
    //                         ("Content-Length", "100"),
    //                         ("Transfer-Encoding", "chunked"),
    //                     ],
    //                 )
    //             )
    //             == b"HTTP/1.1 200 \r\n"
    //             b"Transfer-Encoding: chunked\r\n\r\n"
    //         )

    #[test]
    fn test_head_framing_headers() {
        let setup = |method: &[u8], http_version: &[u8]| -> Connection {
            let mut c = Connection::new(Role::Server, None);
            c.receive_data(
                &vec![
                    method.to_vec(),
                    b" / HTTP/".to_vec(),
                    http_version.to_vec(),
                    b"\r\n".to_vec(),
                    b"Host: example.com\r\n\r\n".to_vec(),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<u8>>(),
            )
            .unwrap();
            assert!(match c.next_event().unwrap() {
                Event::Request(_) => true,
                _ => false,
            });
            assert!(match c.next_event().unwrap() {
                Event::EndOfMessage(_) => true,
                _ => false,
            });
            return c;
        };

        for method in vec![b"GET".to_vec(), b"HEAD".to_vec()] {
            // No Content-Length, HTTP/1.1 peer, should use chunked
            let mut c = setup(&method, &b"1.1".to_vec());
            assert_eq!(
                c.send(
                    Response {
                        status_code: 200,
                        headers: vec![].into(),
                        http_version: b"1.1".to_vec(),
                        reason: b"".to_vec(),
                    }
                    .into()
                )
                .unwrap()
                .unwrap(),
                b"HTTP/1.1 200 \r\ntransfer-encoding: chunked\r\n\r\n".to_vec()
            );

            // No Content-Length, HTTP/1.0 peer, frame with connection: close
            let mut c = setup(&method, &b"1.0".to_vec());
            assert_eq!(
                c.send(
                    Response {
                        status_code: 200,
                        headers: vec![(b"connection".to_vec(), b"close".to_vec())].into(),
                        http_version: b"1.1".to_vec(),
                        reason: b"".to_vec(),
                    }
                    .into()
                )
                .unwrap()
                .unwrap(),
                b"HTTP/1.1 200 \r\nconnection: close\r\n\r\n".to_vec()
            );

            // Content-Length + Transfer-Encoding, TE wins
            let mut c = setup(&method, &b"1.1".to_vec());
            assert_eq!(
                c.send(
                    Response {
                        status_code: 200,
                        headers: vec![
                            (b"Content-Length".to_vec(), b"100".to_vec()),
                            (b"Transfer-Encoding".to_vec(), b"chunked".to_vec())
                        ]
                        .into(),
                        http_version: b"1.1".to_vec(),
                        reason: b"".to_vec(),
                    }
                    .into()
                )
                .unwrap()
                .unwrap(),
                b"HTTP/1.1 200 \r\ntransfer-encoding: chunked\r\n\r\n".to_vec()
            );
        }
    }

    // def test_special_exceptions_for_lost_connection_in_message_body() -> None:
    //     c = Connection(SERVER)
    //     c.receive_data(
    //         b"POST / HTTP/1.1\r\n" b"Host: example.com\r\n" b"Content-Length: 100\r\n\r\n"
    //     )
    //     assert type(c.next_event()) is Request
    //     assert c.next_event() is NEED_DATA
    //     c.receive_data(b"12345")
    //     assert c.next_event() == Data(data=b"12345")
    //     c.receive_data(b"")
    //     with pytest.raises(RemoteProtocolError) as excinfo:
    //         c.next_event()
    //     assert "received 5 bytes" in str(excinfo.value)
    //     assert "expected 100" in str(excinfo.value)

    //     c = Connection(SERVER)
    //     c.receive_data(
    //         b"POST / HTTP/1.1\r\n"
    //         b"Host: example.com\r\n"
    //         b"Transfer-Encoding: chunked\r\n\r\n"
    //     )
    //     assert type(c.next_event()) is Request
    //     assert c.next_event() is NEED_DATA
    //     c.receive_data(b"8\r\n012345")
    //     assert c.next_event().data == b"012345"  # type: ignore
    //     c.receive_data(b"")
    //     with pytest.raises(RemoteProtocolError) as excinfo:
    //         c.next_event()
    //     assert "incomplete chunked read" in str(excinfo.value)

    #[test]
    fn test_special_exceptions_for_lost_connection_in_message_body() {
        let mut c = Connection::new(Role::Server, None);
        c.receive_data(
            &vec![
                b"POST / HTTP/1.1\r\n".to_vec(),
                b"Host: example.com\r\n".to_vec(),
                b"Content-Length: 100\r\n\r\n".to_vec(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<u8>>(),
        )
        .unwrap();
        assert!(match c.next_event().unwrap() {
            Event::Request(_) => true,
            _ => false,
        });
        assert_eq!(c.next_event().unwrap(), Event::NeedData {});
        c.receive_data(b"12345").unwrap();
        assert_eq!(
            c.next_event().unwrap(),
            Event::Data(Data {
                data: b"12345".to_vec(),
                chunk_start: false,
                chunk_end: false,
            })
        );
        c.receive_data(b"").unwrap();
        assert!(match c.next_event().unwrap_err() {
            ProtocolError::RemoteProtocolError(_) => true,
            _ => false,
        });

        let mut c = Connection::new(Role::Server, None);
        c.receive_data(
            &vec![
                b"POST / HTTP/1.1\r\n".to_vec(),
                b"Host: example.com\r\n".to_vec(),
                b"Transfer-Encoding: chunked\r\n\r\n".to_vec(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<u8>>(),
        )
        .unwrap();
        assert!(match c.next_event().unwrap() {
            Event::Request(_) => true,
            _ => false,
        });
        assert_eq!(c.next_event().unwrap(), Event::NeedData {});
        c.receive_data(b"8\r\n012345").unwrap();
        assert_eq!(
            match c.next_event().unwrap() {
                Event::Data(d) => d.data,
                _ => panic!(),
            },
            b"012345".to_vec()
        );
        c.receive_data(b"").unwrap();
        assert!(match c.next_event().unwrap_err() {
            ProtocolError::RemoteProtocolError(_) => true,
            _ => false,
        });
    }
}
