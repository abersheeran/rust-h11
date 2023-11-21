mod helper;
use std::collections::HashMap;

use h11::{
    Connection, ConnectionClosed, Data, EndOfMessage, Event, EventType, Headers, ProtocolError,
    Request, Response, Role,
};
use helper::{get_all_events, receive_and_get, ConnectionPair};

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
                (Role::Client, h11::State::SendBody),
                (Role::Server, h11::State::SendResponse),
            ])
        );
    }
    assert_eq!(p.conn[&Role::Client].get_our_state(), h11::State::SendBody);
    assert_eq!(
        p.conn[&Role::Client].get_their_state(),
        h11::State::SendResponse
    );
    assert_eq!(
        p.conn[&Role::Server].get_our_state(),
        h11::State::SendResponse
    );
    assert_eq!(
        p.conn[&Role::Server].get_their_state(),
        h11::State::SendBody
    );
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
                (Role::Client, h11::State::SendBody),
                (Role::Server, h11::State::SendBody),
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
            HashMap::from([
                (Role::Client, h11::State::Done),
                (Role::Server, h11::State::SendBody),
            ])
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
            HashMap::from([
                (Role::Client, h11::State::Done),
                (Role::Server, h11::State::Done),
            ])
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
            HashMap::from([
                (Role::Client, h11::State::Done),
                (Role::Server, h11::State::Done),
            ])
        );
    }
}

#[test]
fn test_chunk_boundaries() {
    let mut conn = Connection::new(Role::Server, None);

    let request = b"POST / HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n";
    conn.receive_data(request);
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

    conn.receive_data(b"5\r\nhello\r\n");
    assert_eq!(
        conn.next_event().unwrap(),
        Event::Data(Data {
            data: b"hello".to_vec(),
            chunk_start: true,
            chunk_end: true,
        })
    );

    conn.receive_data(b"5\r\nhel");
    assert_eq!(
        conn.next_event().unwrap(),
        Event::Data(Data {
            data: b"hel".to_vec(),
            chunk_start: true,
            chunk_end: false,
        })
    );

    conn.receive_data(b"l");
    assert_eq!(
        conn.next_event().unwrap(),
        Event::Data(Data {
            data: b"l".to_vec(),
            chunk_start: false,
            chunk_end: false,
        })
    );

    conn.receive_data(b"o\r\n");
    assert_eq!(
        conn.next_event().unwrap(),
        Event::Data(Data {
            data: b"o".to_vec(),
            chunk_start: false,
            chunk_end: true,
        })
    );

    conn.receive_data(b"5\r\nhello");
    assert_eq!(
        conn.next_event().unwrap(),
        Event::Data(Data {
            data: b"hello".to_vec(),
            chunk_start: true,
            chunk_end: true,
        })
    );

    conn.receive_data(b"\r\n");
    assert_eq!(conn.next_event().unwrap(), Event::NeedData {});

    conn.receive_data(b"0\r\n\r\n");
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
    assert_eq!(c.get_our_state(), h11::State::Done);
    assert_eq!(
        receive_and_get(&mut c, b"HTTP/1.0 200 OK\r\n\r\n").unwrap(),
        vec![Event::NormalResponse(Response {
            status_code: 200,
            headers: vec![].into(),
            http_version: b"1.0".to_vec(),
            reason: b"OK".to_vec(),
        })],
    );
    assert_eq!(c.get_our_state(), h11::State::MustClose);
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
    assert_eq!(c.get_their_state(), h11::State::Closed);
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
    assert_eq!(c.get_their_state(), h11::State::MustClose);

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
    assert_eq!(c.get_our_state(), h11::State::MustClose);

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
    assert_eq!(c.get_their_state(), h11::State::MustClose);
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
        assert_eq!(
            connection.get_states()[&Role::Client],
            h11::State::MustClose
        );
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
                (Role::Client, h11::State::MustClose),
                (Role::Server, h11::State::MustClose),
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
    c.receive_data(b"GET / HTTP/1.0\r\nEndless: ");
    assert_eq!(c.next_event().unwrap(), Event::NeedData {});

    // Checking that the same header is accepted / rejected depending on the
    // max_incomplete_event_size setting:
    let mut c = Connection::new(Role::Server, Some(5000));
    c.receive_data(b"GET / HTTP/1.0\r\nBig: ");
    c.receive_data(&vec![b'a'; 4000]);
    c.receive_data(b"\r\n\r\n");
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
    c.receive_data(b"GET / HTTP/1.0\r\nBig: ");
    c.receive_data(&vec![b'a'; 4000]);
    assert!(match c.next_event().unwrap_err() {
        ProtocolError::RemoteProtocolError(_) => true,
        _ => false,
    });

    // Temporarily exceeding the size limit is fine, as long as its done with
    // complete events:
    let mut c = Connection::new(Role::Server, Some(5000));
    c.receive_data(b"GET / HTTP/1.0\r\nContent-Length: 10000");
    c.receive_data(b"\r\n\r\n");
    c.receive_data(&vec![b'a'; 10000]);
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
    );
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
    c.receive_data(&vec![b'X'; 1000]);
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
    );
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
    c.receive_data(&vec![b'X'; 1000]);
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
            HashMap::from([
                (Role::Client, h11::State::Done),
                (Role::Server, h11::State::Done),
            ])
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
    );
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
    assert_eq!(c.get_their_state(), h11::State::Done);
    assert_eq!(c.get_our_state(), h11::State::SendResponse);

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
    assert_eq!(c.get_their_state(), h11::State::Done);
    assert_eq!(c.get_our_state(), h11::State::Done);

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
    c.receive_data(b"SADF");
    assert_eq!(c.next_event().unwrap(), Event::Paused {});
    assert_eq!(c.get_trailing_data(), (b"SADF".to_vec(), false));
    // If EOF arrives while paused, we don't see that either:
    c.receive_data(b"");
    assert_eq!(c.get_trailing_data(), (b"SADF".to_vec(), true));
    assert_eq!(c.next_event().unwrap(), Event::Paused {});
    c.receive_data(b"");
    assert_eq!(c.next_event().unwrap(), Event::Paused {});
}
