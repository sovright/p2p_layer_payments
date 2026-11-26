//! End-to-end tests against the mock node and mock issuer.
//!
//! Every height, price, margin and confirmation count here is test data. Tests write only
//! under `CARGO_TARGET_TMPDIR`, each in a new directory, and never delete anything.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use ed25519_dalek::VerifyingKey;
use serde_json::json;

use paid_stream_bridge::bridge::{
    Bridge, BridgeError, Config, CrashPoint, PaymentRequest, PurchaseView,
};
use paid_stream_bridge::codec::{self, Invoice, Network};
use paid_stream_bridge::contract;
use paid_stream_bridge::hexutil;
use paid_stream_bridge::mock::{
    self, MockError, MockIssuer, MockNode, MockParams, Tamper, OTHER_CLIENT_NODE_ID,
    TEST_CLIENT_NODE_ID, TEST_PROVIDER_NODE_ID,
};
use paid_stream_bridge::rpc::{RpcClient, RpcError};
use paid_stream_bridge::state::{Stage, StateError};
use paid_stream_bridge::uri;

const BIN: &str = env!("CARGO_BIN_EXE_paid-stream-bridge");
const COOKIE: &str = "__cookie__:flow-test-cookie";
const SERVICE: &str = "synthetic-10";

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn fresh_dir(name: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("flow")
        .join(format!("{name}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Env {
    issuer: MockIssuer,
    node: MockNode,
    cfg: Config,
    dir: PathBuf,
}

fn spawn_node(issuer: &MockIssuer, client: [u8; 32], controls: bool) -> MockNode {
    MockNode::spawn(
        issuer.clone(),
        client,
        COOKIE,
        "127.0.0.1:0".parse().unwrap(),
        controls,
    )
    .unwrap()
}

fn env_with(name: &str, limit_zat: u64) -> Env {
    let issuer = MockIssuer::new(MockParams::default());
    let node = spawn_node(&issuer, TEST_CLIENT_NODE_ID, true);
    let dir = fresh_dir(name);
    let cookie_file = dir.join("node.cookie");
    std::fs::write(&cookie_file, COOKIE).unwrap();
    let cfg = Config {
        node_rpc: node.addr,
        cookie_file,
        state_file: dir.join("state.json"),
        network: Network::Test,
        provider_node_id: hexutil::encode(&TEST_PROVIDER_NODE_ID),
        issuer_public_key: hexutil::encode(&issuer.public_key()),
        purchase_limit_zat: limit_zat,
    };
    Env {
        issuer,
        node,
        cfg,
        dir,
    }
}

fn env(name: &str) -> Env {
    env_with(name, 100_000)
}

fn open(env: &Env) -> Bridge {
    Bridge::open(env.cfg.clone()).unwrap()
}

fn offered(v: &PurchaseView) -> (String, bool) {
    match &v.payment {
        Some(PaymentRequest::Offered { uri, reshown, .. }) => (uri.clone(), *reshown),
        other => panic!("expected a payment request, got {other:?}"),
    }
}

fn withheld(v: &PurchaseView) -> String {
    match &v.payment {
        Some(PaymentRequest::Withheld(reason)) => reason.clone(),
        other => panic!("expected a withheld payment request, got {other:?}"),
    }
}

fn pid_of(v: &PurchaseView) -> [u8; 16] {
    hexutil::decode_array(v.purchase_id.as_deref().expect("purchase_id")).unwrap()
}

fn invoice_of(env: &Env, pid: [u8; 16]) -> Invoice {
    let key = VerifyingKey::from_bytes(&env.issuer.public_key()).unwrap();
    codec::verify_invoice(&env.issuer.invoice_bytes(pid).unwrap(), &key).unwrap()
}

/// Mock payment included in the next block, then mined to 3 confirmations (test data).
fn pay_and_confirm(env: &Env, pid: [u8; 16]) {
    env.issuer.pay(pid, None, None).unwrap();
    env.issuer.mine(2);
}

fn write_config(env: &Env) -> PathBuf {
    let path = env.dir.join("bridge.json");
    std::fs::write(&path, serde_json::to_string_pretty(&env.cfg).unwrap()).unwrap();
    path
}

fn cli(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn signed_invoice_to_uri_pending_issued_verified_grant_mock_install() {
    let env = env("happy");
    let mut b = open(&env);
    let v = b.buy(SERVICE).unwrap();
    assert_eq!(v.stage, Stage::Invoiced);
    let (request, reshown) = offered(&v);
    assert!(!reshown);
    let pid = pid_of(&v);
    let inv = invoice_of(&env, pid);

    // An independent read-back of the request with the ZIP 321 reference parser.
    let parsed = zip321::TransactionRequest::from_uri(&request).unwrap();
    assert_eq!(parsed.payments().len(), 1);
    let payment = parsed.payments().values().next().unwrap();
    assert_eq!(payment.recipient_address().encode(), inv.destination);
    assert_eq!(payment.amount().unwrap().into_u64(), inv.amount_zat);
    assert_eq!(
        payment.memo().unwrap().as_slice()[..inv.memo.len()],
        inv.memo[..]
    );

    let v = b.status(None).unwrap().remove(0);
    assert_eq!(v.last_status.as_deref(), Some("Pending"));
    assert_eq!(v.stage, Stage::Invoiced);

    env.issuer.pay(pid, None, None).unwrap();
    env.issuer.mine(1); // 2 of 3 confirmations
    let v = b.status(None).unwrap().remove(0);
    assert_eq!(v.last_status.as_deref(), Some("Pending"));
    assert_eq!(v.stage, Stage::Invoiced);
    assert_eq!(env.node.installed_count(), 0);

    env.issuer.mine(1); // 3 of 3
    let v = b.status(None).unwrap().remove(0);
    assert_eq!(v.stage, Stage::Installed);
    assert_eq!(v.last_status.as_deref(), Some("Issued"));
    let grant_bytes = env.issuer.grant_bytes(pid).unwrap();
    assert_eq!(env.node.installed(pid).unwrap(), grant_bytes);
    assert_eq!(
        b.state().purchases[0].grant.as_deref().unwrap(),
        hexutil::encode(&grant_bytes)
    );
    let key = VerifyingKey::from_bytes(&env.issuer.public_key()).unwrap();
    let grant = codec::verify_grant(&grant_bytes, &key).unwrap();
    let install = v.install.unwrap();
    assert_eq!(
        (install.start_height, install.end_height),
        (grant.start_height, grant.end_height)
    );

    env.issuer.mine(inv.access_blocks);
    let v = b.status(None).unwrap().remove(0);
    assert_eq!(v.stage, Stage::Expired);
}

#[test]
fn lost_requestinvoice_answer_is_retried_with_the_same_request_id() {
    let env = env("lost-requestinvoice");
    env.node.drop_next("paidstream_requestinvoice");
    let mut b = open(&env);
    let err = b.buy(SERVICE).unwrap_err();
    assert!(matches!(err, BridgeError::NodeUnreachable(_)), "{err}");
    assert_eq!(b.state().purchases[0].stage, Stage::Requested);
    assert_eq!(
        env.issuer.invoices_created(),
        1,
        "the provider did process it"
    );
    let request_id = b.state().purchases[0].request_id.clone();

    let v = b.resume().unwrap().remove(0);
    assert_eq!(v.stage, Stage::Invoiced);
    assert_eq!(v.request_id, request_id);
    assert_eq!(env.issuer.invoices_created(), 1);
    let pid = pid_of(&v);
    assert_eq!(
        b.state().purchases[0].invoice.as_deref().unwrap(),
        hexutil::encode(&env.issuer.invoice_bytes(pid).unwrap())
    );
    let (first, _) = offered(&v);

    // Buying the same service again continues the same purchase with the same request.
    let again = b.buy(SERVICE).unwrap();
    assert_eq!(again.purchase_id, v.purchase_id);
    let (second, reshown) = offered(&again);
    assert_eq!(first, second);
    assert!(reshown);
    assert_eq!(env.issuer.invoices_created(), 1);
    assert_eq!(b.state().purchases.len(), 1);
}

#[test]
fn lost_getgrant_answer_changes_nothing() {
    let env = env("lost-getgrant");
    let mut b = open(&env);
    let pid = pid_of(&b.buy(SERVICE).unwrap());
    pay_and_confirm(&env, pid);
    env.node.drop_next("paidstream_getgrant");
    let v = b.status(None).unwrap().remove(0);
    assert_eq!(v.stage, Stage::Invoiced);
    assert!(v.note.unwrap().contains("got no answer"));
    assert_eq!(env.node.installed_count(), 0);

    let v = b.status(None).unwrap().remove(0);
    assert_eq!(v.stage, Stage::Installed);
    assert_eq!(
        env.node.installed(pid).unwrap(),
        env.issuer.grant_bytes(pid).unwrap()
    );
}

#[test]
fn lost_setgrant_answer_reinstalls_the_same_grant() {
    let env = env("lost-setgrant");
    let mut b = open(&env);
    let pid = pid_of(&b.buy(SERVICE).unwrap());
    pay_and_confirm(&env, pid);
    env.node.drop_next("paidstream_setgrant");
    let v = b.status(None).unwrap().remove(0);
    assert_eq!(v.stage, Stage::Granted);
    assert!(v.note.unwrap().contains("setgrant got no answer"));
    assert_eq!(env.node.installed_count(), 1, "the node did process it");

    let v = b.resume().unwrap().remove(0);
    assert_eq!(v.stage, Stage::Installed);
    assert_eq!(env.node.calls("paidstream_setgrant"), 2);
    assert_eq!(env.node.installed_count(), 1);
    assert_eq!(
        env.node.installed(pid).unwrap(),
        env.issuer.grant_bytes(pid).unwrap()
    );
}

fn stage_on_disk(path: &Path) -> String {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    v["purchases"][0]["stage"].as_str().unwrap().to_string()
}

#[test]
fn restart_after_each_persisted_step() {
    for point in [
        CrashPoint::AfterRequested,
        CrashPoint::AfterInvoiced,
        CrashPoint::AfterGranted,
        CrashPoint::AfterInstalled,
    ] {
        let env = env(&format!("restart-{point:?}"));
        {
            let mut b = open(&env);
            let err = match point {
                CrashPoint::AfterRequested | CrashPoint::AfterInvoiced => {
                    b.set_crash_point(Some(point));
                    b.buy(SERVICE).unwrap_err()
                }
                CrashPoint::AfterGranted | CrashPoint::AfterInstalled => {
                    let pid = pid_of(&b.buy(SERVICE).unwrap());
                    pay_and_confirm(&env, pid);
                    b.set_crash_point(Some(point));
                    b.status(None).unwrap_err()
                }
            };
            assert!(
                matches!(err, BridgeError::SimulatedCrash(p) if p == point),
                "{point:?}: {err}"
            );
        } // Dropping the bridge releases the lock; the next run sees only what is on disk.

        let expected_on_disk = match point {
            CrashPoint::AfterRequested => "requested",
            CrashPoint::AfterInvoiced => "invoiced",
            CrashPoint::AfterGranted => "granted",
            CrashPoint::AfterInstalled => "installed",
        };
        assert_eq!(stage_on_disk(&env.cfg.state_file), expected_on_disk);
        let invoices_before = env.issuer.invoices_created();
        assert_eq!(
            invoices_before,
            usize::from(point != CrashPoint::AfterRequested),
            "{point:?}"
        );

        let mut b = open(&env);
        let v = b.resume().unwrap().remove(0);
        assert_eq!(b.state().purchases.len(), 1, "{point:?}");
        assert_eq!(env.issuer.invoices_created(), 1, "{point:?}");
        match point {
            CrashPoint::AfterRequested | CrashPoint::AfterInvoiced => {
                assert_eq!(v.stage, Stage::Invoiced, "{point:?}");
                offered(&v);
            }
            CrashPoint::AfterGranted | CrashPoint::AfterInstalled => {
                assert_eq!(v.stage, Stage::Installed, "{point:?}");
                let pid = pid_of(&v);
                assert_eq!(env.node.installed_count(), 1);
                assert_eq!(
                    env.node.installed(pid).unwrap(),
                    env.issuer.grant_bytes(pid).unwrap()
                );
            }
        }
    }
}

#[test]
fn same_request_id_repeats_and_conflicting_terms() {
    // The mock contract: same client and request_id, same answer bytes; other terms, an error.
    let issuer = MockIssuer::new(MockParams::default());
    let request_id = [7u8; 16];
    let first = issuer
        .request_invoice(TEST_CLIENT_NODE_ID, "synthetic-10", request_id)
        .unwrap();
    let again = issuer
        .request_invoice(TEST_CLIENT_NODE_ID, "synthetic-10", request_id)
        .unwrap();
    assert_eq!(first, again);
    assert_eq!(issuer.invoices_created(), 1);
    assert_eq!(
        issuer.request_invoice(TEST_CLIENT_NODE_ID, "synthetic-20", request_id),
        Err(MockError::RequestConflict)
    );

    // The bridge: a stored request whose terms now conflict is rejected and shows nothing.
    let env = env("conflict");
    env.issuer
        .request_invoice(TEST_CLIENT_NODE_ID, "synthetic-10", request_id)
        .unwrap();
    let state = json!({
        "format": 1,
        "purchases": [{
            "request_id": hexutil::encode(&request_id),
            "service": "synthetic-20",
            "stage": "requested"
        }]
    });
    std::fs::write(&env.cfg.state_file, state.to_string()).unwrap();
    let mut b = open(&env);
    let v = b.resume().unwrap().remove(0);
    assert_eq!(v.stage, Stage::Rejected);
    assert!(v.note.unwrap().contains("RequestConflict"));
    assert!(v.payment.is_none());
    let shown = b.show(&hexutil::encode(&request_id)).unwrap();
    assert!(withheld(&shown).contains("failed a check"));
}

#[test]
fn tampered_invoices_never_become_payment_requests() {
    for (tamper, expected) in [
        (Tamper::InvoiceWrongKey, "signature does not verify"),
        (Tamper::InvoiceGrantDomain, "signature does not verify"),
        (Tamper::InvoiceWrongNetwork, "another network"),
        (Tamper::InvoiceWrongProvider, "another provider NodeId"),
        (Tamper::InvoiceWrongClient, "another client NodeId"),
        (Tamper::InvoiceWrongService, "is for service"),
    ] {
        let env = env(&format!("tamper-{tamper:?}"));
        env.node.set_tamper(tamper);
        let mut b = open(&env);
        match b.buy(SERVICE).unwrap_err() {
            BridgeError::Rejected(msg) => assert!(msg.contains(expected), "{tamper:?}: {msg}"),
            other => panic!("{tamper:?}: {other}"),
        }
        assert_eq!(b.state().purchases[0].stage, Stage::Rejected);
        assert!(b.state().purchases[0].invoice.is_none());
        let request_id = b.state().purchases[0].request_id.clone();
        let shown = b.show(&request_id).unwrap();
        withheld(&shown);
    }
}

#[test]
fn tampered_grants_are_not_installed() {
    for (tamper, expected) in [
        (Tamper::GrantWrongKey, "signature does not verify"),
        (Tamper::GrantInvoiceDomain, "signature does not verify"),
        (Tamper::GrantOtherClient, "another client NodeId"),
        (Tamper::IssuedWithoutGrant, "carried no grant"),
    ] {
        let env = env(&format!("tamper-{tamper:?}"));
        let mut b = open(&env);
        let pid = pid_of(&b.buy(SERVICE).unwrap());
        env.node.set_tamper(tamper);
        pay_and_confirm(&env, pid);
        let v = b.status(None).unwrap().remove(0);
        assert_eq!(v.stage, Stage::Invoiced, "{tamper:?}");
        let note = v.note.unwrap();
        assert!(note.contains(expected), "{tamper:?}: {note}");
        assert!(b.state().purchases[0].grant.is_none());
        assert_eq!(env.node.installed_count(), 0);
        assert_eq!(env.node.calls("paidstream_setgrant"), 0);
    }
}

#[test]
fn mock_contract_setgrant_refuses_a_grant_for_another_node() {
    let env = env("foreign-grant");
    let mut b = open(&env);
    let pid = pid_of(&b.buy(SERVICE).unwrap());
    pay_and_confirm(&env, pid);
    assert_eq!(b.status(None).unwrap()[0].stage, Stage::Installed);
    let grant = env.issuer.grant_bytes(pid).unwrap();

    let other = spawn_node(&env.issuer, OTHER_CLIENT_NODE_ID, false);
    let client = RpcClient::new(other.addr, COOKIE).unwrap();
    let err = client
        .call(
            "paidstream_setgrant",
            json!({"grant": hexutil::encode(&grant)}),
        )
        .unwrap_err();
    assert!(
        matches!(&err, RpcError::Rpc { code, .. } if *code == mock::CODE_INVALID_GRANT),
        "{err}"
    );
    assert_eq!(other.installed_count(), 0);
}

/// Mock-contract test only: the other node's identity is a configured value standing in for
/// a TLS-proven NodeId. It does not test real P2P authentication.
#[test]
fn mock_contract_getgrant_from_another_nodeid_is_not_found() {
    let env = env("other-nodeid");
    let mut b = open(&env);
    let pid = pid_of(&b.buy(SERVICE).unwrap());
    pay_and_confirm(&env, pid);

    let other = spawn_node(&env.issuer, OTHER_CLIENT_NODE_ID, false);
    let client = RpcClient::new(other.addr, COOKIE).unwrap();
    let answer = client
        .call(
            "paidstream_getgrant",
            json!({"purchase_id": hexutil::encode(&pid)}),
        )
        .unwrap();
    assert_eq!(answer, json!({"status": "NotFound"}));
    let unknown = client
        .call(
            "paidstream_getgrant",
            json!({"purchase_id": hexutil::encode(&[9u8; 16])}),
        )
        .unwrap();
    assert_eq!(
        unknown, answer,
        "unknown and foreign purchases look the same"
    );
    // Mock controls are off on this node.
    let err = client.call("mock_mine", json!({"blocks": 1})).unwrap_err();
    assert!(matches!(err, RpcError::Rpc { code: -32601, .. }), "{err}");
}

#[test]
fn invoice_above_the_purchase_limit_is_rejected() {
    let env = env_with("over-limit", 10_000);
    let mut b = open(&env);
    match b.buy(SERVICE).unwrap_err() {
        BridgeError::Rejected(msg) => assert!(msg.contains("purchase limit"), "{msg}"),
        other => panic!("{other}"),
    }
    assert_eq!(b.state().purchases[0].stage, Stage::Rejected);
}

#[test]
fn corrupt_state_file_is_refused_and_left_untouched() {
    for (name, contents) in [
        ("garbage", "{not json".to_string()),
        (
            "inconsistent",
            json!({"format": 1, "purchases": [{
                "request_id": "00112233445566778899aabbccddeeff",
                "service": "synthetic-10",
                "stage": "installed"
            }]})
            .to_string(),
        ),
        (
            "unknown-format",
            json!({"format": 99, "purchases": []}).to_string(),
        ),
    ] {
        let env = env(&format!("corrupt-{name}"));
        std::fs::write(&env.cfg.state_file, &contents).unwrap();
        let err = Bridge::open(env.cfg.clone()).err().expect("open must fail");
        assert!(
            matches!(err, BridgeError::State(StateError::Corrupt { .. })),
            "{name}: {err}"
        );
        let cfg_path = write_config(&env);
        let out = cli(&["--config", cfg_path.to_str().unwrap(), "status"]);
        assert_eq!(out.status.code(), Some(4), "{name}: {}", text(&out.stderr));
        assert!(text(&out.stderr).contains("left untouched"));
        assert_eq!(
            std::fs::read_to_string(&env.cfg.state_file).unwrap(),
            contents
        );
    }
}

#[test]
fn a_second_process_on_the_same_state_is_locked_out() {
    let env = env("locked");
    let held = open(&env);
    let err = Bridge::open(env.cfg.clone())
        .err()
        .expect("second open must fail");
    assert!(
        matches!(err, BridgeError::State(StateError::Locked(_))),
        "{err}"
    );

    let cfg_path = write_config(&env);
    let out = cli(&["--config", cfg_path.to_str().unwrap(), "buy", SERVICE]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("holds the lock"));
    assert_eq!(env.issuer.invoices_created(), 0);

    drop(held);
    // Lock reacquisition failed intermittently during parallel test runs.
    // Retry only Locked within a fixed bound; other errors fail immediately.
    let start = Instant::now();
    loop {
        match Bridge::open(env.cfg.clone()) {
            Ok(_) => break,
            Err(BridgeError::State(StateError::Locked(_)))
                if start.elapsed() < Duration::from_secs(5) =>
            {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => panic!("the lock was not released: {e}"),
        }
    }
}

#[test]
fn expired_invoice_is_not_offered_again() {
    let env = env("expired");
    let mut b = open(&env);
    let v = b.buy(SERVICE).unwrap();
    offered(&v);
    let pid = pid_of(&v);
    let inv = invoice_of(&env, pid);

    // The tip reaches the deadline: the local height check withholds the request.
    env.issuer.mine(inv.deadline_height - env.issuer.tip());
    let shown = b.show(v.purchase_id.as_deref().unwrap()).unwrap();
    assert!(withheld(&shown).contains("reached the deadline height"));

    // One block later the provider reports Expired.
    env.issuer.mine(1);
    let shown = b.show(v.purchase_id.as_deref().unwrap()).unwrap();
    assert!(withheld(&shown).contains("status is Expired"));
    let v = b.status(None).unwrap().remove(0);
    assert_eq!(v.stage, Stage::Expired);
    let shown = b.show(v.purchase_id.as_deref().unwrap()).unwrap();
    assert!(withheld(&shown).contains("expired"));
    assert!(
        b.buy(SERVICE).is_ok(),
        "a new purchase may start after expiry"
    );
}

#[test]
fn no_grant_from_a_local_txid_or_a_user_claim() {
    // The CLI has no way to report a payment: such arguments are usage errors that change nothing.
    let env = env("no-local-claims");
    let cfg_path = write_config(&env);
    let cfg = cfg_path.to_str().unwrap();
    for args in [
        vec!["--config", cfg, "status", "--txid", "00ff"],
        vec!["--config", cfg, "status", "--paid"],
        vec!["--config", cfg, "buy", SERVICE, "--txid", "00ff"],
        vec!["--config", cfg, "show", "--paid"],
    ] {
        let out = cli(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
    assert!(!env.cfg.state_file.exists());

    // Without a verified grant nothing is installed, however far the chain moves.
    let mut b = open(&env);
    b.buy(SERVICE).unwrap();
    env.issuer.mine(400);
    let v = b.status(None).unwrap().remove(0);
    assert_eq!(v.stage, Stage::Expired);
    assert!(b.state().purchases[0].grant.is_none());
    assert_eq!(env.node.installed_count(), 0);
}

#[test]
fn underpaid_and_noncompliant_payments_get_no_grant_and_no_new_request() {
    for (name, amount, expiry, status) in [
        ("underpaid", Some(1_000), None, "Underpaid"),
        ("expiry-zero", None, Some(0), "NeedsReconciliation"),
        (
            "expiry-above-deadline",
            None,
            Some(u32::MAX),
            "NeedsReconciliation",
        ),
    ] {
        let env = env(name);
        let mut b = open(&env);
        let v = b.buy(SERVICE).unwrap();
        let pid = pid_of(&v);
        env.issuer.pay(pid, amount, expiry).unwrap();
        env.issuer.mine(5);
        let v = b.status(None).unwrap().remove(0);
        assert_eq!(v.last_status.as_deref(), Some(status), "{name}");
        assert_eq!(v.stage, Stage::Invoiced, "{name}");
        assert_eq!(env.node.installed_count(), 0, "{name}");
        let shown = b.show(v.purchase_id.as_deref().unwrap()).unwrap();
        assert!(withheld(&shown).contains(status), "{name}");
    }
}

#[test]
fn destinations_and_rpc_endpoints_are_checked() {
    let main = mock::mock_destination(Network::Main, 0);
    let err = uri::check_destination(&main, Network::Test).unwrap_err();
    assert!(err.contains("network"), "{err}");
    assert!(
        uri::check_destination(&mock::mock_destination(Network::Test, 0), Network::Test).is_ok()
    );
    assert!(uri::check_destination("not-an-address", Network::Test).is_err());

    assert!(RpcClient::new("10.0.0.1:8232".parse().unwrap(), COOKIE).is_err());
    let env = env("bad-cookie");
    let client = RpcClient::new(env.node.addr, "__cookie__:wrong").unwrap();
    let err = client.call("getblockcount", json!([])).unwrap_err();
    assert!(
        matches!(&err, RpcError::Protocol(m) if m.contains("401")),
        "{err}"
    );
}

const REQUEST_INVOICE: &str = "paidstream_requestinvoice";

#[test]
fn transient_error_before_the_provider_keeps_the_request_for_retry() {
    let env = env("transient-before");
    env.node.fail_next(
        REQUEST_INVOICE,
        contract::CODE_PROVIDER_UNREACHABLE,
        "provider did not answer",
        false,
    );
    let mut b = open(&env);
    let err = b.buy(SERVICE).unwrap_err();
    assert!(matches!(err, BridgeError::Retryable(_)), "{err}");
    assert_eq!(err.exit_code(), 5);
    assert_eq!(b.state().purchases[0].stage, Stage::Requested);
    let request_id = b.state().purchases[0].request_id.clone();
    assert_eq!(env.issuer.invoices_created(), 0);

    let v = b.resume().unwrap().remove(0);
    assert_eq!(v.stage, Stage::Invoiced);
    assert_eq!(v.request_id, request_id);
    assert!(v.note.is_none());
    offered(&v);
    assert_eq!(env.issuer.invoices_created(), 1);
    assert_eq!(b.state().purchases.len(), 1);
}

#[test]
fn transient_error_after_the_provider_recorded_the_request_returns_the_same_invoice() {
    let env = env("transient-after");
    env.node.fail_next(
        REQUEST_INVOICE,
        contract::CODE_PROVIDER_UNREACHABLE,
        "answer lost on the way back",
        true,
    );
    let mut b = open(&env);
    let err = b.buy(SERVICE).unwrap_err();
    assert!(matches!(err, BridgeError::Retryable(_)), "{err}");
    assert_eq!(b.state().purchases[0].stage, Stage::Requested);
    assert_eq!(env.issuer.invoices_created(), 1, "the provider recorded it");
    let request_id = b.state().purchases[0].request_id.clone();

    // The next buy continues the same purchase; it does not start a new request.
    let v = b.buy(SERVICE).unwrap();
    assert_eq!(v.stage, Stage::Invoiced);
    assert_eq!(v.request_id, request_id);
    assert_eq!(b.state().purchases.len(), 1);
    assert_eq!(env.issuer.invoices_created(), 1);
    let pid = pid_of(&v);
    assert_eq!(
        b.state().purchases[0].invoice.as_deref().unwrap(),
        hexutil::encode(&env.issuer.invoice_bytes(pid).unwrap())
    );
}

#[test]
fn definitive_provider_rejection_is_final_and_reported() {
    let env = env("definitive-reject");
    let mut b = open(&env);
    match b.buy("no-such-service").unwrap_err() {
        BridgeError::Rejected(msg) => {
            assert!(
                msg.contains("code 3") && msg.contains("UnknownService"),
                "{msg}"
            )
        }
        other => panic!("{other}"),
    }
    assert_eq!(b.state().purchases[0].stage, Stage::Rejected);
    assert!(b.state().purchases[0]
        .note
        .as_deref()
        .unwrap()
        .contains("UnknownService"));
    drop(b);

    let cfg_path = write_config(&env);
    let out = cli(&[
        "--config",
        cfg_path.to_str().unwrap(),
        "buy",
        "another-unknown-service",
    ]);
    assert_eq!(out.status.code(), Some(6), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("rejected") && stderr.contains("UnknownService"),
        "{stderr}"
    );
}

#[test]
fn unrecognized_or_nonfinal_error_codes_keep_the_request() {
    for (code, expected) in [
        (42, "unrecognized error code"),
        (-32601, "unrecognized error code"),
        (contract::CODE_DISABLED, "disabled on the node"),
    ] {
        let env = env(&format!("nonfinal-{code}"));
        env.node
            .fail_next(REQUEST_INVOICE, code, "test error", false);
        let mut b = open(&env);
        match b.buy(SERVICE).unwrap_err() {
            BridgeError::Retryable(msg) => {
                assert!(
                    msg.contains(expected) && msg.contains(&format!("code {code}")),
                    "{msg}"
                )
            }
            other => panic!("code {code}: {other}"),
        }
        let p = &b.state().purchases[0];
        assert_eq!(p.stage, Stage::Requested, "code {code}");
        assert!(p.note.as_deref().unwrap().contains(&format!("code {code}")));
        let request_id = p.request_id.clone();

        let v = b.resume().unwrap().remove(0);
        assert_eq!(v.stage, Stage::Invoiced, "code {code}");
        assert_eq!(v.request_id, request_id);
        assert_eq!(b.state().purchases.len(), 1);
        assert_eq!(env.issuer.invoices_created(), 1);
    }
}

fn wait_for(path: &Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(100));
}

#[test]
fn cli_demo_end_to_end_against_the_mock_node() {
    let dir = fresh_dir("cli-demo").join("demo");
    let mut child = Command::new(BIN)
        .args([
            "mock-node",
            "--dir",
            dir.to_str().unwrap(),
            "--serve-secs",
            "60",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let cfg_path = dir.join("bridge.json");
    wait_for(&cfg_path);
    let cfg = cfg_path.to_str().unwrap();

    let out = cli(&["--config", cfg, "buy", SERVICE]);
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("SIMULATION ONLY"));
    assert!(stdout.contains("zcash:utest1"), "{stdout}");
    assert!(stdout.contains("do not pay"), "{stdout}");
    let pid = stdout
        .lines()
        .find_map(|l| l.strip_prefix("purchase "))
        .and_then(|l| l.split_whitespace().next())
        .unwrap()
        .to_string();

    for args in [
        vec!["mock-control", "--config", cfg, "pay", pid.as_str()],
        vec!["mock-control", "--config", cfg, "mine", "2"],
    ] {
        let out = cli(&args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: {}",
            text(&out.stderr)
        );
    }
    let out = cli(&["--config", cfg, "status"]);
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(stdout.contains("stage installed"), "{stdout}");
    assert!(stdout.contains("mock install"), "{stdout}");

    let out = cli(&["--config", cfg, "show", &pid]);
    assert!(text(&out.stdout).contains("already issued"));

    child.kill().unwrap();
    child.wait().unwrap();
}
