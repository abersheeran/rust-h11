// # This contains the main Connection class. Everything in h11 revolves around
// # this.
// from typing import (
//     Any,
//     Callable,
//     cast,
//     Dict,
//     List,
//     Optional,
//     overload,
//     Tuple,
//     Type,
//     Union,
// )

// from ._events import (
//     ConnectionClosed,
//     Data,
//     EndOfMessage,
//     Event,
//     InformationalResponse,
//     Request,
//     Response,
// )
// from ._headers import get_comma_header, has_expect_100_continue, set_comma_header
// from ._readers import READERS, ReadersType
// from ._receivebuffer import ReceiveBuffer
// from ._state import (
//     _SWITCH_CONNECT,
//     _SWITCH_UPGRADE,
//     CLIENT,
//     ConnectionState,
//     DONE,
//     ERROR,
//     MIGHT_SWITCH_PROTOCOL,
//     SEND_BODY,
//     SERVER,
//     SWITCHED_PROTOCOL,
// )
// from ._util import (  # Import the internal things we need
//     LocalProtocolError,
//     RemoteProtocolError,
//     Sentinel,
// )
// from ._writers import WRITERS, WritersType

// # Everything in __all__ gets re-exported as part of the h11 public API.
// __all__ = ["Connection", "NEED_DATA", "PAUSED"]

// class NEED_DATA(Sentinel, metaclass=Sentinel):
//     pass

// class PAUSED(Sentinel, metaclass=Sentinel):
//     pass

// # If we ever have this much buffered without it making a complete parseable
// # event, we error out. The only time we really buffer is when reading the
// # request/response line + headers together, so this is effectively the limit on
// # the size of that.
// #
// # Some precedents for defaults:
// # - node.js: 80 * 1024
// # - tomcat: 8 * 1024
// # - IIS: 16 * 1024
// # - Apache: <8 KiB per line>
// DEFAULT_MAX_INCOMPLETE_EVENT_SIZE = 16 * 1024

// # RFC 7230's rules for connection lifecycles:
// # - If either side says they want to close the connection, then the connection
// #   must close.
// # - HTTP/1.1 defaults to keep-alive unless someone says Connection: close
// # - HTTP/1.0 defaults to close unless both sides say Connection: keep-alive
// #   (and even this is a mess -- e.g. if you're implementing a proxy then
// #   sending Connection: keep-alive is forbidden).
// #
// # We simplify life by simply not supporting keep-alive with HTTP/1.0 peers. So
// # our rule is:
// # - If someone says Connection: close, we will close
// # - If someone uses HTTP/1.0, we will close.
// def _keep_alive(event: Union[Request, Response]) -> bool:
//     connection = get_comma_header(event.headers, b"connection")
//     if b"close" in connection:
//         return False
//     if getattr(event, "http_version", b"1.1") < b"1.1":
//         return False
//     return True

// def _body_framing(
//     request_method: bytes, event: Union[Request, Response]
// ) -> Tuple[str, Union[Tuple[()], Tuple[int]]]:
//     # Called when we enter SEND_BODY to figure out framing information for
//     # this body.
//     #
//     # These are the only two events that can trigger a SEND_BODY state:
//     assert type(event) in (Request, Response)
//     # Returns one of:
//     #
//     #    ("content-length", count)
//     #    ("chunked", ())
//     #    ("http/1.0", ())
//     #
//     # which are (lookup key, *args) for constructing body reader/writer
//     # objects.
//     #
//     # Reference: https://tools.ietf.org/html/rfc7230#section-3.3.3
//     #
//     # Step 1: some responses always have an empty body, regardless of what the
//     # headers say.
//     if type(event) is Response:
//         if (
//             event.status_code in (204, 304)
//             or request_method == b"HEAD"
//             or (request_method == b"CONNECT" and 200 <= event.status_code < 300)
//         ):
//             return ("content-length", (0,))
//         # Section 3.3.3 also lists another case -- responses with status_code
//         # < 200. For us these are InformationalResponses, not Responses, so
//         # they can't get into this function in the first place.
//         assert event.status_code >= 200

//     # Step 2: check for Transfer-Encoding (T-E beats C-L):
//     transfer_encodings = get_comma_header(event.headers, b"transfer-encoding")
//     if transfer_encodings:
//         assert transfer_encodings == [b"chunked"]
//         return ("chunked", ())

//     # Step 3: check for Content-Length
//     content_lengths = get_comma_header(event.headers, b"content-length")
//     if content_lengths:
//         return ("content-length", (int(content_lengths[0]),))

//     # Step 4: no applicable headers; fallback/default depends on type
//     if type(event) is Request:
//         return ("content-length", (0,))
//     else:
//         return ("http/1.0", ())

// ################################################################
// #
// # The main Connection class
// #
// ################################################################

// class Connection:
//     """An object encapsulating the state of an HTTP connection.

//     Args:
//         our_role: If you're implementing a client, pass :data:`h11.CLIENT`. If
//             you're implementing a server, pass :data:`h11.SERVER`.

//         max_incomplete_event_size (int):
//             The maximum number of bytes we're willing to buffer of an
//             incomplete event. In practice this mostly sets a limit on the
//             maximum size of the request/response line + headers. If this is
//             exceeded, then :meth:`next_event` will raise
//             :exc:`RemoteProtocolError`.

//     """

//     def __init__(
//         self,
//         our_role: Type[Sentinel],
//         max_incomplete_event_size: int = DEFAULT_MAX_INCOMPLETE_EVENT_SIZE,
//     ) -> None:
//         self._max_incomplete_event_size = max_incomplete_event_size
//         # State and role tracking
//         if our_role not in (CLIENT, SERVER):
//             raise ValueError(f"expected CLIENT or SERVER, not {our_role!r}")
//         self.our_role = our_role
//         self.their_role: Type[Sentinel]
//         if our_role is CLIENT:
//             self.their_role = SERVER
//         else:
//             self.their_role = CLIENT
//         self._cstate = ConnectionState()

//         # Callables for converting data->events or vice-versa given the
//         # current state
//         self._writer = self._get_io_object(self.our_role, None, WRITERS)
//         self._reader = self._get_io_object(self.their_role, None, READERS)

//         # Holds any unprocessed received data
//         self._receive_buffer = ReceiveBuffer()
//         # If this is true, then it indicates that the incoming connection was
//         # closed *after* the end of whatever's in self._receive_buffer:
//         self._receive_buffer_closed = False

//         # Extra bits of state that don't fit into the state machine.
//         #
//         # These two are only used to interpret framing headers for figuring
//         # out how to read/write response bodies. their_http_version is also
//         # made available as a convenient public API.
//         self.their_http_version: Optional[bytes] = None
//         self._request_method: Optional[bytes] = None
//         # This is pure flow-control and doesn't at all affect the set of legal
//         # transitions, so no need to bother ConnectionState with it:
//         self.client_is_waiting_for_100_continue = False

//     @property
//     def states(self) -> Dict[Type[Sentinel], Type[Sentinel]]:
//         """A dictionary like::

//            {CLIENT: <client state>, SERVER: <server state>}

//         See :ref:`state-machine` for details.

//         """
//         return dict(self._cstate.states)

//     @property
//     def our_state(self) -> Type[Sentinel]:
//         """The current state of whichever role we are playing. See
//         :ref:`state-machine` for details.
//         """
//         return self._cstate.states[self.our_role]

//     @property
//     def their_state(self) -> Type[Sentinel]:
//         """The current state of whichever role we are NOT playing. See
//         :ref:`state-machine` for details.
//         """
//         return self._cstate.states[self.their_role]

//     @property
//     def they_are_waiting_for_100_continue(self) -> bool:
//         return self.their_role is CLIENT and self.client_is_waiting_for_100_continue

//     def start_next_cycle(self) -> None:
//         """Attempt to reset our connection state for a new request/response
//         cycle.

//         If both client and server are in :data:`DONE` state, then resets them
//         both to :data:`IDLE` state in preparation for a new request/response
//         cycle on this same connection. Otherwise, raises a
//         :exc:`LocalProtocolError`.

//         See :ref:`keepalive-and-pipelining`.

//         """
//         old_states = dict(self._cstate.states)
//         self._cstate.start_next_cycle()
//         self._request_method = None
//         # self.their_http_version gets left alone, since it presumably lasts
//         # beyond a single request/response cycle
//         assert not self.client_is_waiting_for_100_continue
//         self._respond_to_state_changes(old_states)

//     def _process_error(self, role: Type[Sentinel]) -> None:
//         old_states = dict(self._cstate.states)
//         self._cstate.process_error(role)
//         self._respond_to_state_changes(old_states)

//     def _server_switch_event(self, event: Event) -> Optional[Type[Sentinel]]:
//         if type(event) is InformationalResponse and event.status_code == 101:
//             return _SWITCH_UPGRADE
//         if type(event) is Response:
//             if (
//                 _SWITCH_CONNECT in self._cstate.pending_switch_proposals
//                 and 200 <= event.status_code < 300
//             ):
//                 return _SWITCH_CONNECT
//         return None

//     # All events go through here
//     def _process_event(self, role: Type[Sentinel], event: Event) -> None:
//         # First, pass the event through the state machine to make sure it
//         # succeeds.
//         old_states = dict(self._cstate.states)
//         if role is CLIENT and type(event) is Request:
//             if event.method == b"CONNECT":
//                 self._cstate.process_client_switch_proposal(_SWITCH_CONNECT)
//             if get_comma_header(event.headers, b"upgrade"):
//                 self._cstate.process_client_switch_proposal(_SWITCH_UPGRADE)
//         server_switch_event = None
//         if role is SERVER:
//             server_switch_event = self._server_switch_event(event)
//         self._cstate.process_event(role, type(event), server_switch_event)

//         # Then perform the updates triggered by it.

//         if type(event) is Request:
//             self._request_method = event.method

//         if role is self.their_role and type(event) in (
//             Request,
//             Response,
//             InformationalResponse,
//         ):
//             event = cast(Union[Request, Response, InformationalResponse], event)
//             self.their_http_version = event.http_version

//         # Keep alive handling
//         #
//         # RFC 7230 doesn't really say what one should do if Connection: close
//         # shows up on a 1xx InformationalResponse. I think the idea is that
//         # this is not supposed to happen. In any case, if it does happen, we
//         # ignore it.
//         if type(event) in (Request, Response) and not _keep_alive(
//             cast(Union[Request, Response], event)
//         ):
//             self._cstate.process_keep_alive_disabled()

//         # 100-continue
//         if type(event) is Request and has_expect_100_continue(event):
//             self.client_is_waiting_for_100_continue = True
//         if type(event) in (InformationalResponse, Response):
//             self.client_is_waiting_for_100_continue = False
//         if role is CLIENT and type(event) in (Data, EndOfMessage):
//             self.client_is_waiting_for_100_continue = False

//         self._respond_to_state_changes(old_states, event)

//     def _get_io_object(
//         self,
//         role: Type[Sentinel],
//         event: Optional[Event],
//         io_dict: Union[ReadersType, WritersType],
//     ) -> Optional[Callable[..., Any]]:
//         # event may be None; it's only used when entering SEND_BODY
//         state = self._cstate.states[role]
//         if state is SEND_BODY:
//             # Special case: the io_dict has a dict of reader/writer factories
//             # that depend on the request/response framing.
//             framing_type, args = _body_framing(
//                 cast(bytes, self._request_method), cast(Union[Request, Response], event)
//             )
//             return io_dict[SEND_BODY][framing_type](*args)  # type: ignore[index]
//         else:
//             # General case: the io_dict just has the appropriate reader/writer
//             # for this state
//             return io_dict.get((role, state))  # type: ignore[return-value]

//     # This must be called after any action that might have caused
//     # self._cstate.states to change.
//     def _respond_to_state_changes(
//         self,
//         old_states: Dict[Type[Sentinel], Type[Sentinel]],
//         event: Optional[Event] = None,
//     ) -> None:
//         # Update reader/writer
//         if self.our_state != old_states[self.our_role]:
//             self._writer = self._get_io_object(self.our_role, event, WRITERS)
//         if self.their_state != old_states[self.their_role]:
//             self._reader = self._get_io_object(self.their_role, event, READERS)

//     @property
//     def trailing_data(self) -> Tuple[bytes, bool]:
//         """Data that has been received, but not yet processed, represented as
//         a tuple with two elements, where the first is a byte-string containing
//         the unprocessed data itself, and the second is a bool that is True if
//         the receive connection was closed.

//         See :ref:`switching-protocols` for discussion of why you'd want this.
//         """
//         return (bytes(self._receive_buffer), self._receive_buffer_closed)

//     def receive_data(self, data: bytes) -> None:
//         """Add data to our internal receive buffer.

//         This does not actually do any processing on the data, just stores
//         it. To trigger processing, you have to call :meth:`next_event`.

//         Args:
//             data (:term:`bytes-like object`):
//                 The new data that was just received.

//                 Special case: If *data* is an empty byte-string like ``b""``,
//                 then this indicates that the remote side has closed the
//                 connection (end of file). Normally this is convenient, because
//                 standard Python APIs like :meth:`file.read` or
//                 :meth:`socket.recv` use ``b""`` to indicate end-of-file, while
//                 other failures to read are indicated using other mechanisms
//                 like raising :exc:`TimeoutError`. When using such an API you
//                 can just blindly pass through whatever you get from ``read``
//                 to :meth:`receive_data`, and everything will work.

//                 But, if you have an API where reading an empty string is a
//                 valid non-EOF condition, then you need to be aware of this and
//                 make sure to check for such strings and avoid passing them to
//                 :meth:`receive_data`.

//         Returns:
//             Nothing, but after calling this you should call :meth:`next_event`
//             to parse the newly received data.

//         Raises:
//             RuntimeError:
//                 Raised if you pass an empty *data*, indicating EOF, and then
//                 pass a non-empty *data*, indicating more data that somehow
//                 arrived after the EOF.

//                 (Calling ``receive_data(b"")`` multiple times is fine,
//                 and equivalent to calling it once.)

//         """
//         if data:
//             if self._receive_buffer_closed:
//                 raise RuntimeError("received close, then received more data?")
//             self._receive_buffer += data
//         else:
//             self._receive_buffer_closed = True

//     def _extract_next_receive_event(
//         self,
//     ) -> Union[Event, Type[NEED_DATA], Type[PAUSED]]:
//         state = self.their_state
//         # We don't pause immediately when they enter DONE, because even in
//         # DONE state we can still process a ConnectionClosed() event. But
//         # if we have data in our buffer, then we definitely aren't getting
//         # a ConnectionClosed() immediately and we need to pause.
//         if state is DONE and self._receive_buffer:
//             return PAUSED
//         if state is MIGHT_SWITCH_PROTOCOL or state is SWITCHED_PROTOCOL:
//             return PAUSED
//         assert self._reader is not None
//         event = self._reader(self._receive_buffer)
//         if event is None:
//             if not self._receive_buffer and self._receive_buffer_closed:
//                 # In some unusual cases (basically just HTTP/1.0 bodies), EOF
//                 # triggers an actual protocol event; in that case, we want to
//                 # return that event, and then the state will change and we'll
//                 # get called again to generate the actual ConnectionClosed().
//                 if hasattr(self._reader, "read_eof"):
//                     event = self._reader.read_eof()  # type: ignore[attr-defined]
//                 else:
//                     event = ConnectionClosed()
//         if event is None:
//             event = NEED_DATA
//         return event  # type: ignore[no-any-return]

//     def next_event(self) -> Union[Event, Type[NEED_DATA], Type[PAUSED]]:
//         """Parse the next event out of our receive buffer, update our internal
//         state, and return it.

//         This is a mutating operation -- think of it like calling :func:`next`
//         on an iterator.

//         Returns:
//             : One of three things:

//             1) An event object -- see :ref:`events`.

//             2) The special constant :data:`NEED_DATA`, which indicates that
//                you need to read more data from your socket and pass it to
//                :meth:`receive_data` before this method will be able to return
//                any more events.

//             3) The special constant :data:`PAUSED`, which indicates that we
//                are not in a state where we can process incoming data (usually
//                because the peer has finished their part of the current
//                request/response cycle, and you have not yet called
//                :meth:`start_next_cycle`). See :ref:`flow-control` for details.

//         Raises:
//             RemoteProtocolError:
//                 The peer has misbehaved. You should close the connection
//                 (possibly after sending some kind of 4xx response).

//         Once this method returns :class:`ConnectionClosed` once, then all
//         subsequent calls will also return :class:`ConnectionClosed`.

//         If this method raises any exception besides :exc:`RemoteProtocolError`
//         then that's a bug -- if it happens please file a bug report!

//         If this method raises any exception then it also sets
//         :attr:`Connection.their_state` to :data:`ERROR` -- see
//         :ref:`error-handling` for discussion.

//         """

//         if self.their_state is ERROR:
//             raise RemoteProtocolError("Can't receive data when peer state is ERROR")
//         try:
//             event = self._extract_next_receive_event()
//             if event not in [NEED_DATA, PAUSED]:
//                 self._process_event(self.their_role, cast(Event, event))
//             if event is NEED_DATA:
//                 if len(self._receive_buffer) > self._max_incomplete_event_size:
//                     # 431 is "Request header fields too large" which is pretty
//                     # much the only situation where we can get here
//                     raise RemoteProtocolError(
//                         "Receive buffer too long", error_status_hint=431
//                     )
//                 if self._receive_buffer_closed:
//                     # We're still trying to complete some event, but that's
//                     # never going to happen because no more data is coming
//                     raise RemoteProtocolError("peer unexpectedly closed connection")
//             return event
//         except BaseException as exc:
//             self._process_error(self.their_role)
//             if isinstance(exc, LocalProtocolError):
//                 exc._reraise_as_remote_protocol_error()
//             else:
//                 raise

//     @overload
//     def send(self, event: ConnectionClosed) -> None:
//         ...

//     @overload
//     def send(self, event: Event) -> bytes:
//         ...

//     def send(self, event: Event) -> Optional[bytes]:
//         """Convert a high-level event into bytes that can be sent to the peer,
//         while updating our internal state machine.

//         Args:
//             event: The :ref:`event <events>` to send.

//         Returns:
//             If ``type(event) is ConnectionClosed``, then returns
//             ``None``. Otherwise, returns a :term:`bytes-like object`.

//         Raises:
//             LocalProtocolError:
//                 Sending this event at this time would violate our
//                 understanding of the HTTP/1.1 protocol.

//         If this method raises any exception then it also sets
//         :attr:`Connection.our_state` to :data:`ERROR` -- see
//         :ref:`error-handling` for discussion.

//         """
//         data_list = self.send_with_data_passthrough(event)
//         if data_list is None:
//             return None
//         else:
//             return b"".join(data_list)

//     def send_with_data_passthrough(self, event: Event) -> Optional[List[bytes]]:
//         """Identical to :meth:`send`, except that in situations where
//         :meth:`send` returns a single :term:`bytes-like object`, this instead
//         returns a list of them -- and when sending a :class:`Data` event, this
//         list is guaranteed to contain the exact object you passed in as
//         :attr:`Data.data`. See :ref:`sendfile` for discussion.

//         """
//         if self.our_state is ERROR:
//             raise LocalProtocolError("Can't send data when our state is ERROR")
//         try:
//             if type(event) is Response:
//                 event = self._clean_up_response_headers_for_sending(event)
//             # We want to call _process_event before calling the writer,
//             # because if someone tries to do something invalid then this will
//             # give a sensible error message, while our writers all just assume
//             # they will only receive valid events. But, _process_event might
//             # change self._writer. So we have to do a little dance:
//             writer = self._writer
//             self._process_event(self.our_role, event)
//             if type(event) is ConnectionClosed:
//                 return None
//             else:
//                 # In any situation where writer is None, process_event should
//                 # have raised ProtocolError
//                 assert writer is not None
//                 data_list: List[bytes] = []
//                 writer(event, data_list.append)
//                 return data_list
//         except:
//             self._process_error(self.our_role)
//             raise

//     def send_failed(self) -> None:
//         """Notify the state machine that we failed to send the data it gave
//         us.

//         This causes :attr:`Connection.our_state` to immediately become
//         :data:`ERROR` -- see :ref:`error-handling` for discussion.

//         """
//         self._process_error(self.our_role)

//     # When sending a Response, we take responsibility for a few things:
//     #
//     # - Sometimes you MUST set Connection: close. We take care of those
//     #   times. (You can also set it yourself if you want, and if you do then
//     #   we'll respect that and close the connection at the right time. But you
//     #   don't have to worry about that unless you want to.)
//     #
//     # - The user has to set Content-Length if they want it. Otherwise, for
//     #   responses that have bodies (e.g. not HEAD), then we will automatically
//     #   select the right mechanism for streaming a body of unknown length,
//     #   which depends on depending on the peer's HTTP version.
//     #
//     # This function's *only* responsibility is making sure headers are set up
//     # right -- everything downstream just looks at the headers. There are no
//     # side channels.
//     def _clean_up_response_headers_for_sending(self, response: Response) -> Response:
//         assert type(response) is Response

//         headers = response.headers
//         need_close = False

//         # HEAD requests need some special handling: they always act like they
//         # have Content-Length: 0, and that's how _body_framing treats
//         # them. But their headers are supposed to match what we would send if
//         # the request was a GET. (Technically there is one deviation allowed:
//         # we're allowed to leave out the framing headers -- see
//         # https://tools.ietf.org/html/rfc7231#section-4.3.2 . But it's just as
//         # easy to get them right.)
//         method_for_choosing_headers = cast(bytes, self._request_method)
//         if method_for_choosing_headers == b"HEAD":
//             method_for_choosing_headers = b"GET"
//         framing_type, _ = _body_framing(method_for_choosing_headers, response)
//         if framing_type in ("chunked", "http/1.0"):
//             # This response has a body of unknown length.
//             # If our peer is HTTP/1.1, we use Transfer-Encoding: chunked
//             # If our peer is HTTP/1.0, we use no framing headers, and close the
//             # connection afterwards.
//             #
//             # Make sure to clear Content-Length (in principle user could have
//             # set both and then we ignored Content-Length b/c
//             # Transfer-Encoding overwrote it -- this would be naughty of them,
//             # but the HTTP spec says that if our peer does this then we have
//             # to fix it instead of erroring out, so we'll accord the user the
//             # same respect).
//             headers = set_comma_header(headers, b"content-length", [])
//             if self.their_http_version is None or self.their_http_version < b"1.1":
//                 # Either we never got a valid request and are sending back an
//                 # error (their_http_version is None), so we assume the worst;
//                 # or else we did get a valid HTTP/1.0 request, so we know that
//                 # they don't understand chunked encoding.
//                 headers = set_comma_header(headers, b"transfer-encoding", [])
//                 # This is actually redundant ATM, since currently we
//                 # unconditionally disable keep-alive when talking to HTTP/1.0
//                 # peers. But let's be defensive just in case we add
//                 # Connection: keep-alive support later:
//                 if self._request_method != b"HEAD":
//                     need_close = True
//             else:
//                 headers = set_comma_header(headers, b"transfer-encoding", [b"chunked"])

//         if not self._cstate.keep_alive or need_close:
//             # Make sure Connection: close is set
//             connection = set(get_comma_header(headers, b"connection"))
//             connection.discard(b"keep-alive")
//             connection.add(b"close")
//             headers = set_comma_header(headers, b"connection", sorted(connection))

//         return Response(
//             headers=headers,
//             status_code=response.status_code,
//             http_version=response.http_version,
//             reason=response.reason,
//         )

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
    our_role: Role,
    their_role: Role,
    _cstate: ConnectionState,
    _writer: Option<Box<WriterFnMut>>,
    _reader: Option<Box<dyn Reader>>,
    _max_incomplete_event_size: usize,
    _receive_buffer: ReceiveBuffer,
    _receive_buffer_closed: bool,
    their_http_version: Option<Vec<u8>>,
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
            _writer: None,
            _reader: None,
            _max_incomplete_event_size: max_incomplete_event_size
                .unwrap_or(DEFAULT_MAX_INCOMPLETE_EVENT_SIZE),
            _receive_buffer: ReceiveBuffer::new(),
            _receive_buffer_closed: false,
            their_http_version: None,
            _request_method: None,
            client_is_waiting_for_100_continue: false,
        }
    }

    pub fn start_next_cycle(&mut self) {
        let old_states = self._cstate.states.clone();
        self._cstate.start_next_cycle().unwrap();
        self._request_method = None;
        self.their_http_version = None;
        self.client_is_waiting_for_100_continue = false;
        self._respond_to_state_changes(old_states, None);
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

    fn _process_event(&mut self, role: Role, event: Event) {
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
            .process_event(role, event.clone().into(), server_switch_event)
            .unwrap();

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
        match event.clone().into() {
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
    }

    fn _respond_to_state_changes(
        &mut self,
        old_states: HashMap<Role, State>,
        event: Option<Event>,
    ) {
        if self._cstate.states[&self.our_role] != old_states[&self.our_role] {
            let state = self._cstate.states[&self.our_role];
            self._writer = if let Some(event) = event.clone() {
                match state {
                    State::SendBody => {
                        let (framing_type, length) = _body_framing(
                            &self._request_method.as_ref().unwrap(),
                            RequestOrResponse::from(event),
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
                }
            } else {
                None
            };
        }
        if self._cstate.states[&self.their_role] != old_states[&self.their_role] {
            self._reader = if let Some(event) = event.clone() {
                match self._cstate.states[&self.their_role] {
                    State::SendBody => {
                        let (framing_type, length) = _body_framing(
                            &self._request_method.as_ref().unwrap(),
                            RequestOrResponse::from(event),
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
                        (Role::Client, State::SendResponse) => {
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
                }
            } else {
                None
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
        let state = self._cstate.states[&self.their_role];
        if state == State::Done && self._receive_buffer.len() > 0 {
            return Ok(Event::Paused());
        }
        if state == State::MightSwitchProtocol || state == State::SwitchedProtocol {
            return Ok(Event::Paused());
        }
        match self
            ._reader
            .as_mut()
            .unwrap()
            .call(&mut self._receive_buffer)
        {
            Ok(event) => {
                if event.is_none() {
                    if self._receive_buffer.len() == 0 && self._receive_buffer_closed {
                        return self._reader.as_mut().unwrap().read_eof();
                    }
                }
                Ok(event.unwrap_or(Event::NeedData()))
            }
            Err(error) => Err(error),
        }
    }

    pub fn next_event(&mut self) -> Result<Event, ProtocolError> {
        if self._cstate.states[&self.their_role] == State::Error {
            panic!("Can't receive data when peer state is ERROR");
        }
        match self._extract_next_receive_event() {
            Ok(event) => {
                match event {
                    Event::NeedData() | Event::Paused() => {
                        self._process_event(self.their_role, event.clone())
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
        if self._cstate.states[&self.our_role] == State::Error {
            return Err(ProtocolError::LocalProtocolError(
                "Can't send data when our state is ERROR".to_string().into(),
            ));
        }
        event = if let Event::NormalResponse(response) = &event {
            Event::NormalResponse(self._clean_up_response_headers_for_sending(response.clone()))
        } else {
            event
        };
        self._process_event(self.our_role, event.clone());

        let event_type: EventType = event.clone().into();
        if event_type == EventType::ConnectionClosed {
            return Ok(None);
        } else {
            match self._writer.as_mut().unwrap()(Box::new(event)) {
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

    fn _clean_up_response_headers_for_sending(&self, response: Response) -> Response {
        let mut headers = response.clone().headers;
        let mut need_close = false;
        let mut method_for_choosing_headers = self._request_method.clone().unwrap();
        if method_for_choosing_headers == b"HEAD".to_vec() {
            method_for_choosing_headers = b"GET".to_vec();
        }
        let (framing_type, _) = _body_framing(&method_for_choosing_headers, response.clone());
        if framing_type == "chunked" || framing_type == "http/1.0" {
            headers = set_comma_header(&headers, b"content-length".to_vec(), vec![]).unwrap();
            if self
                .their_http_version
                .clone()
                .map(|v| v < b"1.1".to_vec())
                .unwrap_or(true)
            {
                headers =
                    set_comma_header(&headers, b"transfer-encoding".to_vec(), vec![]).unwrap();
                if self._request_method.clone().unwrap() != b"HEAD".to_vec() {
                    need_close = true;
                }
            } else {
                headers = set_comma_header(
                    &headers,
                    b"transfer-encoding".to_vec(),
                    vec![b"chunked".to_vec()],
                )
                .unwrap();
            }
        }
        if !self._cstate.keep_alive || need_close {
            let mut connection = get_comma_header(&headers, b"connection".to_vec());
            connection.retain(|x| x != &b"keep-alive".to_vec());
            connection.push(b"close".to_vec());
            headers = set_comma_header(&headers, b"connection".to_vec(), connection).unwrap();
        }
        return Response {
            headers,
            status_code: response.status_code,
            http_version: response.http_version,
            reason: response.reason,
        };
    }
}
