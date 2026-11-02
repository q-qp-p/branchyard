//! Connection reuse of a `Client`, against a keep-alive `MockHttp`: requests
//! share a connection only when the response leaves it open and the body
//! was read to its end, and only a `GET` or `HEAD` is sent again.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use branchyard_testkit::{wait, MockHttp, Response as Canned};
use serde_json::json;

use crate::{Client, Error};

const TIMEOUT: Duration = Duration::from_secs(10);

/// A `GET /v1/repos` answer that keeps its connection open.
fn repos() -> Canned {
    Canned::json(200, &json!({"repos": []})).keep_alive()
}

/// A `POST` or `DELETE` on the path below, sent with no idempotency key:
/// the one request the client never sends again. Returns the status.
fn unkeyed(client: &Client, method: &str) -> u16 {
    let response = client
        .call_raw(
            method,
            "/v1/anything",
            Some(b"{}"),
            Some("application/json"),
            &[],
            TIMEOUT,
        )
        .unwrap();
    let status = response.status;
    response.read_body(64 * 1024).unwrap();
    status
}

fn client(server: &MockHttp) -> Client {
    Client::new(&server.url(), "token").unwrap()
}

#[test]
fn sequential_requests_share_one_connection() {
    let server = MockHttp::start(|_| repos());
    let client = client(&server);
    client.repos().unwrap();
    client.repos().unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        server.connections(),
        1,
        "the second request reused the first connection"
    );
    assert!(
        requests.iter().all(|r| r.header("connection").is_none()),
        "a keep-alive request must not send Connection: close"
    );
    drop(client);
    server.finish();
}

#[test]
fn a_response_that_closes_its_connection_is_not_reused() {
    // The mock answers `Connection: close` unless asked to keep alive.
    let server = MockHttp::start(|_| Canned::json(200, &json!({"repos": []})));
    let client = client(&server);
    client.repos().unwrap();
    client.repos().unwrap();
    assert_eq!(server.connections(), 2);
    drop(client);
    server.finish();
}

#[test]
fn a_get_on_a_connection_the_server_dropped_is_sent_once_more() {
    // The second request arrives on the kept connection, which the server
    // closes without answering. The client's one retry, on a fresh
    // connection, gets the answer.
    let seen = AtomicUsize::new(0);
    let server = MockHttp::start(move |_| {
        if seen.fetch_add(1, Ordering::SeqCst) == 1 {
            Canned::close()
        } else {
            repos()
        }
    });
    let client = client(&server);
    client.repos().unwrap();
    client.repos().unwrap();
    assert_eq!(
        server.requests().len(),
        3,
        "the dropped request, then its retry"
    );
    assert_eq!(server.connections(), 2);
    drop(client);
    server.finish();
}

#[test]
fn a_post_on_a_connection_the_server_dropped_is_not_sent_again() {
    // The POST reaches the server on the kept connection, which the server
    // closes without answering. The error is returned, and the POST is not
    // replayed on a fresh connection.
    let server = MockHttp::start(|request| match request.method.as_str() {
        "POST" => Canned::close(),
        _ => repos(),
    });
    let client = client(&server);
    client.repos().unwrap();
    let posted = client.call_raw(
        "POST",
        "/v1/anything",
        Some(b"{}"),
        Some("application/json"),
        &[],
        TIMEOUT,
    );
    assert!(
        matches!(posted, Err(Error::Transport { .. })),
        "a POST whose connection dropped must fail"
    );
    let requests = server.requests();
    assert_eq!(requests.iter().filter(|r| r.method == "POST").count(), 1);
    assert_eq!(server.connections(), 1, "no second connection for the POST");
    drop(client);
    server.finish();
}

#[test]
fn a_body_not_read_to_its_end_is_not_reused() {
    let server = MockHttp::start(|_| repos());
    let client = client(&server);
    // Dropped unread: only the head was read.
    drop(client.call("GET", "/v1/repos", None, &[], TIMEOUT).unwrap());
    client.repos().unwrap();
    assert_eq!(server.connections(), 2);
    // Refused past its limit: the body is cut off and the connection dropped.
    let response = client.call("GET", "/v1/repos", None, &[], TIMEOUT).unwrap();
    assert!(response.read_body(4).is_err());
    client.repos().unwrap();
    assert_eq!(server.connections(), 3);
    drop(client);
    server.finish();
}

#[test]
fn concurrent_requests_from_clones_each_get_a_working_connection() {
    let server = MockHttp::start(|_| repos());
    let client = client(&server);
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let client = client.clone();
            scope.spawn(move || {
                for _ in 0..5 {
                    client.repos().unwrap();
                }
            });
        }
    });
    assert_eq!(server.requests().len(), 10);
    drop(client);
    server.finish();
}

#[test]
fn a_stream_asks_for_a_connection_of_its_own() {
    let server = MockHttp::start(|_| {
        Canned::new(200, "event: open\nid: 0\ndata: {\"head\":0}\n\n")
            .header("Content-Type", "text/event-stream")
    });
    let client = client(&server);
    let mut stream = client.repo("main").stream(None);
    assert_eq!(stream.open().unwrap(), 0);
    assert_eq!(
        server.requests()[0].header("connection"),
        Some("close"),
        "a stream closes its connection with the stream"
    );
    drop(stream);
    drop(client);
    server.finish();
}

#[test]
fn a_post_after_the_server_closed_an_idle_connection_goes_out_on_a_new_one() {
    // The GET is answered without `Connection: close`, so the client keeps
    // the connection; the server then closes it without a word. Once the
    // close is complete, the client's probe sees end of file and drops the
    // connection, so the POST is sent on a new one. The POST has no key, so
    // nothing could replay it if it had been sent on the dead connection.
    let server = MockHttp::start(|request| match request.method.as_str() {
        "GET" => Canned::json(200, &json!({"repos": []})).close_silently(),
        _ => Canned::json(200, &json!({"ok": true})),
    });
    let client = client(&server);
    client.repos().unwrap();
    wait::until("the server to close its idle connection", || {
        server.closed_silently() == 1
    });
    assert_eq!(unkeyed(&client, "POST"), 200);
    let requests = server.requests();
    assert_eq!(
        requests.iter().filter(|r| r.method == "POST").count(),
        1,
        "the POST was seen exactly once"
    );
    assert_eq!(
        server.connections(),
        2,
        "the POST went out on a second connection"
    );
    drop(client);
    server.finish();
}

#[test]
fn a_delete_after_the_server_closed_an_idle_connection_goes_out_on_a_new_one() {
    // As the POST test above, for a DELETE.
    let server = MockHttp::start(|request| match request.method.as_str() {
        "GET" => Canned::json(200, &json!({"repos": []})).close_silently(),
        _ => Canned::json(200, &json!({"ok": true})),
    });
    let client = client(&server);
    client.repos().unwrap();
    wait::until("the server to close its idle connection", || {
        server.closed_silently() == 1
    });
    assert_eq!(unkeyed(&client, "DELETE"), 200);
    let requests = server.requests();
    assert_eq!(
        requests.iter().filter(|r| r.method == "DELETE").count(),
        1,
        "the DELETE was seen exactly once"
    );
    assert_eq!(
        server.connections(),
        2,
        "the DELETE went out on a second connection"
    );
    drop(client);
    server.finish();
}

#[test]
fn a_connection_close_response_on_an_open_socket_is_not_reused() {
    // The server says `Connection: close` but leaves the socket open, so the
    // idle probe would find it quiet. Only the `Connection` header keeps the
    // client off it. A reused connection would answer the POST on the
    // first connection and fail the count below.
    let server = MockHttp::start(|request| match request.method.as_str() {
        "GET" => repos().announce_close_keep_open(),
        _ => Canned::json(200, &json!({"ok": true})),
    });
    let client = client(&server);
    client.repos().unwrap();
    assert_eq!(unkeyed(&client, "POST"), 200);
    let requests = server.requests();
    assert_eq!(
        requests.iter().filter(|r| r.method == "POST").count(),
        1,
        "the POST was seen exactly once"
    );
    assert_eq!(
        server.connections(),
        2,
        "the POST went out on a second connection"
    );
    drop(client);
    server.finish();
}
