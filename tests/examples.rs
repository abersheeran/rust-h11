mod helper;
use std::collections::HashMap;

use h11::{
    ConnectionClosed, Data, EndOfMessage, Event, EventType, Headers, Request, Response, Role,
};
use helper::ConnectionPair;

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

// def test_chunked() -> None:
//     p = ConnectionPair()

//     p.send(
//         CLIENT,
//         Request(
//             method="GET",
//             target="/",
//             headers=[("Host", "example.com"), ("Transfer-Encoding", "chunked")],
//         ),
//     )
//     data = p.send(CLIENT, Data(data=b"1234567890", chunk_start=True, chunk_end=True))
//     assert data == b"a\r\n1234567890\r\n"
//     data = p.send(CLIENT, Data(data=b"abcde", chunk_start=True, chunk_end=True))
//     assert data == b"5\r\nabcde\r\n"
//     data = p.send(CLIENT, Data(data=b""), expect=[])
//     assert data == b""
//     data = p.send(CLIENT, EndOfMessage(headers=[("hello", "there")]))
//     assert data == b"0\r\nhello: there\r\n\r\n"

//     p.send(
//         SERVER, Response(status_code=200, headers=[("Transfer-Encoding", "chunked")])
//     )
//     p.send(SERVER, Data(data=b"54321", chunk_start=True, chunk_end=True))
//     p.send(SERVER, Data(data=b"12345", chunk_start=True, chunk_end=True))
//     p.send(SERVER, EndOfMessage())

//     for conn in p.conns:
//         assert conn.states == {CLIENT: DONE, SERVER: DONE}

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
