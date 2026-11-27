//! One deadline per node call, against local test servers that answer slowly.
//!
//! The deadline here is 300 ms and a slow server sends one byte every 50 ms, far longer in
//! total. A timeout per read would never fire against it; only one deadline for the whole call
//! ends it. Tests write only under `CARGO_TARGET_TMPDIR`, each in a new directory, and never
//! delete anything.

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use paid_services_mock::{MockIssuer, MockParams, TEST_CLIENT_NODE_ID, TEST_PROVIDER_NODE_ID};
use paid_services_sidecar::bridge::{Bridge, BridgeError, Config};
use paid_services_sidecar::rpc::{read_http_message, RpcClient, RpcError};
use paid_services_sidecar::state::Stage;
use paid_services_types::codec::Network;
use paid_services_types::rpc;

const COOKIE: &str = "__cookie__:deadline-test-cookie";
const DEADLINE: Duration = Duration::from_millis(300);
const DRIP: Duration = Duration::from_millis(50);
/// Generous, for loaded machines; still far shorter than any drip below.
const BOUND: Duration = Duration::from_millis(2500);

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn fresh_dir(name: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("rpc-deadline")
        .join(format!("{name}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// How the test server answers one request.
enum Answer {
    /// The whole answer, in three quick pieces.
    Quick(Value),
    /// The whole answer, header included, one byte at a time.
    DripAll(Value),
    /// The header at once, then the body one byte at a time.
    DripBody(Value),
}

/// What the test server saw for one request.
#[derive(Debug, PartialEq, Eq)]
enum Event {
    Answered,
    /// A write failed: the client had closed the connection.
    ClientClosed,
    /// Every byte went out.
    DripFinished,
}

/// A JSON-RPC answer whose header is padded, so dripping it takes long.
fn http_answer(result: Value) -> (Vec<u8>, Vec<u8>) {
    let body = json!({"jsonrpc": "2.0", "id": 1, "result": result}).to_string();
    let header = format!(
        "HTTP/1.1 200 OK\r\nX-Padding: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        "a".repeat(120),
        body.len()
    );
    (header.into_bytes(), body.into_bytes())
}

fn drip(stream: &mut TcpStream, bytes: &[u8]) -> Event {
    for b in bytes {
        if stream.write_all(&[*b]).is_err() {
            return Event::ClientClosed;
        }
        thread::sleep(DRIP);
    }
    Event::DripFinished
}

/// A loopback server that answers each request as `answer_for(method)` says and reports
/// what happened.
fn serve(answer_for: fn(&str) -> Answer) -> (SocketAddr, mpsc::Receiver<Event>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let Ok(request) = read_http_message(&mut stream, 64 * 1024) else {
                continue;
            };
            let method = serde_json::from_slice::<Value>(&request.body)
                .ok()
                .and_then(|v| v.get("method")?.as_str().map(str::to_string))
                .unwrap_or_default();
            let event = match answer_for(&method) {
                Answer::Quick(result) => {
                    let (header, body) = http_answer(result);
                    let all = [header, body].concat();
                    let mut sent_all = true;
                    for piece in all.chunks(all.len() / 3 + 1) {
                        sent_all &= stream.write_all(piece).is_ok();
                        thread::sleep(Duration::from_millis(20));
                    }
                    if sent_all {
                        Event::Answered
                    } else {
                        Event::ClientClosed
                    }
                }
                Answer::DripAll(result) => {
                    let (header, body) = http_answer(result);
                    drip(&mut stream, &[header, body].concat())
                }
                Answer::DripBody(result) => {
                    let (header, body) = http_answer(result);
                    if stream.write_all(&header).is_err() {
                        Event::ClientClosed
                    } else {
                        drip(&mut stream, &body)
                    }
                }
            };
            if tx.send(event).is_err() {
                return;
            }
        }
    });
    (addr, rx)
}

fn next_event(events: &mpsc::Receiver<Event>) -> Event {
    events
        .recv_timeout(Duration::from_secs(10))
        .expect("the test server reports every request")
}

fn assert_cut_off(err: &RpcError, took: Duration) {
    assert!(
        matches!(err, RpcError::Transport(m) if m.contains("within 300 ms")),
        "{err}"
    );
    assert!(took >= DEADLINE && took < BOUND, "took {took:?}");
}

#[test]
fn a_header_sent_byte_by_byte_is_cut_off_at_the_deadline() {
    let (addr, events) = serve(|_| Answer::DripAll(json!("slow")));
    let client = RpcClient::new(addr, COOKIE).unwrap().with_timeout(DEADLINE);
    let start = Instant::now();
    let err = client.call("getblockcount", json!([])).unwrap_err();
    assert_cut_off(&err, start.elapsed());
    // The client closed the connection; nothing kept reading after the call returned.
    assert_eq!(next_event(&events), Event::ClientClosed);
}

#[test]
fn a_body_sent_byte_by_byte_is_cut_off_at_the_deadline() {
    let (addr, events) = serve(|_| Answer::DripBody(json!("b".repeat(400))));
    let client = RpcClient::new(addr, COOKIE).unwrap().with_timeout(DEADLINE);
    let start = Instant::now();
    let err = client.call("getblockcount", json!([])).unwrap_err();
    assert_cut_off(&err, start.elapsed());
    assert_eq!(next_event(&events), Event::ClientClosed);
}

#[test]
fn a_normal_answer_in_pieces_arrives_within_one_deadline() {
    let (addr, events) = serve(|_| Answer::Quick(json!("ok")));
    let client = RpcClient::new(addr, COOKIE).unwrap().with_timeout(DEADLINE);
    assert_eq!(client.call("getblockcount", json!([])), Ok(json!("ok")));
    assert_eq!(next_event(&events), Event::Answered);
}

#[test]
fn a_timed_out_invoice_request_keeps_its_request_id_and_releases_the_lock() {
    // The node ID comes back at once; the invoice answer is sent byte by byte.
    let (addr, events) = serve(|method| {
        if method == "getminermeshinfo" {
            Answer::Quick(json!({"node_id": hex::encode(TEST_CLIENT_NODE_ID)}))
        } else {
            Answer::DripAll(json!("slow"))
        }
    });
    let dir = fresh_dir("timed-out-buy");
    let cookie_file = dir.join("node.cookie");
    std::fs::write(&cookie_file, COOKIE).unwrap();
    let cfg = Config {
        node_rpc: addr,
        cookie_file,
        state_file: dir.join("state.json"),
        network: Network::Test,
        provider_node_id: hex::encode(TEST_PROVIDER_NODE_ID),
        issuer_public_key: hex::encode(MockIssuer::new(MockParams::default()).public_key()),
        purchase_limit_zat: 100_000,
        wallet_expiry_profile: "librustzcash-default".into(),
        offer_margin_blocks: 10, // test data
    };

    let request_id = {
        let mut b = Bridge::open(cfg.clone()).unwrap();
        b.set_rpc_timeout(DEADLINE);
        let start = Instant::now();
        let err = b
            .buy(paid_services_relay::SERVICE_KIND, "relay-10")
            .unwrap_err();
        let took = start.elapsed();
        assert!(
            matches!(&err, BridgeError::NodeUnreachable(m)
                if m.contains(rpc::REQUEST_INVOICE) && m.contains("within 300 ms")),
            "{err}"
        );
        assert!(took < BOUND, "took {took:?}");
        assert_eq!(b.state().purchases[0].stage, Stage::Requested);
        b.state().purchases[0].request_id.clone()
    }; // The command ends here and drops its lock.

    assert_eq!(next_event(&events), Event::Answered); // getminermeshinfo
    assert_eq!(next_event(&events), Event::ClientClosed); // the invoice request

    let b = Bridge::open(cfg).expect("the lock is free once the command has ended");
    assert_eq!(b.state().purchases.len(), 1);
    assert_eq!(b.state().purchases[0].stage, Stage::Requested);
    assert_eq!(b.state().purchases[0].request_id, request_id);
}
