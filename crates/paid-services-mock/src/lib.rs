//! Mock node and mock issuer for the simulation.
//!
//! The mock node serves the proposed `paidstream_*` contracts on loopback with cookie
//! authentication. They are proposals, not Zakura RPCs. Its client identity is a configured
//! value standing in for the TLS-proven NodeId of a real P2P session, so a `NotFound` for
//! another NodeId tests this mock contract only, not real P2P authentication. A mock
//! `setgrant` install does not show that a node consumes a stream.
//!
//! Heights, prices, confirmation counts and margins here are test data, not proposals.
//! Mock destinations are synthetic testnet addresses whose Orchard receiver deliberately holds
//! an invalid point encoding, so no wallet should be able to pay them; that was not tested
//! against a wallet.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use zcash_address::unified::{self, Encoding as _};

use paid_services_types::codec::{
    self, Grant, Invoice, Network, SigningKey, VerificationKey, GRANT_DOMAIN, INVOICE_DOMAIN,
    SIGNATURE_LEN,
};
use paid_services_types::expiry::LIBRUSTZCASH_DEFAULT;
use paid_services_types::{hex_array, hex_bytes};

use crate::http::{read_http_message, write_http_response};

pub mod http;

/// Seed of the mock issuer's signing key (test data).
pub const TEST_ISSUER_SEED: [u8; 32] = *b"paid-stream-bridge test issuer!!";
/// Seed of a second key, used to forge signatures in tests.
pub const OTHER_ISSUER_SEED: [u8; 32] = [0x77; 32];
/// The mock provider's NodeId (test data).
pub const TEST_PROVIDER_NODE_ID: [u8; 32] = [0x50; 32];
/// The NodeId the mock node claims for itself (test data).
pub const TEST_CLIENT_NODE_ID: [u8; 32] = [0xc1; 32];
/// Another client NodeId, for binding tests.
pub const OTHER_CLIENT_NODE_ID: [u8; 32] = [0xc2; 32];

pub use paid_services_types::contract::{CODE_INVALID_GRANT, CODE_MOCK_CONTROL, CODE_REJECTED};

/// A service the mock issuer sells (test data).
#[derive(Clone, Debug)]
pub struct MockService {
    /// Service name.
    pub name: String,
    /// Price in zatoshis.
    pub price_zat: u64,
    /// Access length in blocks.
    pub access_blocks: u32,
}

/// Test data for the mock issuer.
#[derive(Clone, Debug)]
pub struct MockParams {
    /// Network of the mock chain.
    pub network: Network,
    /// Chain tip at start.
    pub start_tip: u32,
    /// Blocks between the tip and an invoice's deadline.
    pub deadline_margin: u32,
    /// Confirmations before a grant.
    pub confirmations: u32,
    /// Sessions a grant allows.
    pub session_allowance: u32,
    /// Services on sale.
    pub services: Vec<MockService>,
}

impl Default for MockParams {
    fn default() -> Self {
        MockParams {
            network: Network::Test,
            start_tip: 4_500_000,
            deadline_margin: 150,
            confirmations: 3,
            session_allowance: 1,
            services: vec![
                MockService {
                    name: "synthetic-10".into(),
                    price_zat: 25_000,
                    access_blocks: 10,
                },
                MockService {
                    name: "synthetic-20".into(),
                    price_zat: 40_000,
                    access_blocks: 20,
                },
            ],
        }
    }
}

/// Ways the mock node can corrupt an answer, to test the bridge's checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tamper {
    /// Honest answers.
    None,
    /// Invoices signed with another key.
    InvoiceWrongKey,
    /// Invoices signed in the grant domain.
    InvoiceGrantDomain,
    /// Invoices for another network, signed by the issuer.
    InvoiceWrongNetwork,
    /// Invoices naming another provider, signed by the issuer.
    InvoiceWrongProvider,
    /// Invoices bound to another client, signed by the issuer.
    InvoiceWrongClient,
    /// Invoices for another service, signed by the issuer.
    InvoiceWrongService,
    /// Grants signed with another key.
    GrantWrongKey,
    /// Grants signed in the invoice domain.
    GrantInvoiceDomain,
    /// Grants bound to another client, signed by the issuer.
    GrantOtherClient,
    /// `Issued` without a grant.
    IssuedWithoutGrant,
    /// A recorded invoice request answered with bad hex.
    MalformedInvoiceAnswer,
}

impl Tamper {
    /// Control names of the modes.
    pub const NAMES: &'static [(&'static str, Tamper)] = &[
        ("none", Tamper::None),
        ("invoice-wrong-key", Tamper::InvoiceWrongKey),
        ("invoice-grant-domain", Tamper::InvoiceGrantDomain),
        ("invoice-wrong-network", Tamper::InvoiceWrongNetwork),
        ("invoice-wrong-provider", Tamper::InvoiceWrongProvider),
        ("invoice-wrong-client", Tamper::InvoiceWrongClient),
        ("invoice-wrong-service", Tamper::InvoiceWrongService),
        ("grant-wrong-key", Tamper::GrantWrongKey),
        ("grant-invoice-domain", Tamper::GrantInvoiceDomain),
        ("grant-other-client", Tamper::GrantOtherClient),
        ("issued-without-grant", Tamper::IssuedWithoutGrant),
        ("malformed-invoice-answer", Tamper::MalformedInvoiceAnswer),
    ];

    /// The mode with this control name.
    pub fn parse(s: &str) -> Option<Tamper> {
        Self::NAMES.iter().find(|(n, _)| *n == s).map(|(_, t)| *t)
    }
}

/// Why the mock issuer refused a request.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MockError {
    /// The request_id was used before with other terms.
    #[error("RequestConflict: this request_id was already used with other terms")]
    RequestConflict,
    /// The mock offers no such service.
    #[error("UnknownService: {0:?}")]
    UnknownService(String),
    /// No such purchase.
    #[error("unknown purchase")]
    UnknownPurchase,
    /// The purchase already has a payment.
    #[error("the mock records one payment per purchase")]
    AlreadyPaid,
    /// A signed object could not be encoded.
    #[error("encoding failed: {0}")]
    Encode(codec::CodecError),
}

impl MockError {
    fn code(&self) -> i64 {
        match self {
            MockError::RequestConflict | MockError::UnknownService(_) => CODE_REJECTED,
            MockError::UnknownPurchase | MockError::AlreadyPaid | MockError::Encode(_) => {
                CODE_MOCK_CONTROL
            }
        }
    }

    fn message(&self) -> String {
        self.to_string()
    }
}

/// What `paidstream_getgrant` reports, following the proposal's status table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MockStatus {
    /// Unknown, or bound to another client.
    NotFound,
    /// No payment seen.
    Pending,
    /// A qualifying payment short of its confirmations.
    PaymentSeen,
    /// A payment below the price.
    Underpaid,
    /// A payment that breaks the deadline rule.
    NeedsReconciliation,
    /// Granted; the signed grant.
    Issued(Vec<u8>),
    /// Expired, with the grant if one was issued.
    Expired(Option<Vec<u8>>),
}

impl MockStatus {
    /// The status name on the wire.
    pub fn name(&self) -> &'static str {
        match self {
            MockStatus::NotFound => "NotFound",
            MockStatus::Pending => "Pending",
            MockStatus::PaymentSeen => "PaymentSeen",
            MockStatus::Underpaid => "Underpaid",
            MockStatus::NeedsReconciliation => "NeedsReconciliation",
            MockStatus::Issued(_) => "Issued",
            MockStatus::Expired(_) => "Expired",
        }
    }

    /// The `paidstream_getgrant` answer.
    pub fn to_json(&self) -> Value {
        match self {
            MockStatus::Issued(g) | MockStatus::Expired(Some(g)) => {
                json!({"status": self.name(), "grant": hex::encode(g)})
            }
            _ => json!({"status": self.name()}),
        }
    }
}

struct Payment {
    amount_zat: u64,
    included_height: u32,
    expiry_height: u32,
}

struct Record {
    client: [u8; 32],
    invoice: Invoice,
    invoice_bytes: Vec<u8>,
    payment: Option<Payment>,
    grant: Option<(Grant, Vec<u8>)>,
}

struct IssuerState {
    params: MockParams,
    key: SigningKey,
    tip: u32,
    records: Vec<Record>,
    by_request: HashMap<([u8; 32], [u8; 16]), usize>,
}

impl IssuerState {
    /// Issues one grant for each in-time, compliant, fully paid purchase with N confirmations.
    fn issue_due(&mut self) {
        let tip = self.tip;
        let n = self.params.confirmations;
        let allowance = self.params.session_allowance;
        for (i, r) in self.records.iter_mut().enumerate() {
            if r.grant.is_some() {
                continue;
            }
            let Some(p) = &r.payment else { continue };
            if !qualifies(p, &r.invoice) || p.amount_zat < r.invoice.amount_zat {
                continue;
            }
            if tip < p.included_height || tip - p.included_height + 1 < n {
                continue;
            }
            let mut grant_id = *b"mockgid:\0\0\0\0\0\0\0\0";
            grant_id[8..].copy_from_slice(&(i as u64).to_be_bytes());
            let grant = Grant {
                network: r.invoice.network,
                issuer_key: r.invoice.issuer_key,
                provider_node_id: r.invoice.provider_node_id,
                client_node_id: r.invoice.client_node_id,
                service: r.invoice.service.clone(),
                purchase_id: r.invoice.purchase_id,
                grant_id,
                start_height: tip,
                end_height: tip + r.invoice.access_blocks,
                session_allowance: allowance,
            };
            let bytes = codec::sign_grant(&grant, &self.key).expect("mock grant encodes");
            r.grant = Some((grant, bytes));
        }
    }

    fn status(&self, r: &Record) -> MockStatus {
        if let Some((grant, bytes)) = &r.grant {
            return if self.tip >= grant.end_height {
                MockStatus::Expired(Some(bytes.clone()))
            } else {
                MockStatus::Issued(bytes.clone())
            };
        }
        match &r.payment {
            None if self.tip > r.invoice.deadline_height => MockStatus::Expired(None),
            None => MockStatus::Pending,
            Some(p) if !qualifies(p, &r.invoice) => MockStatus::NeedsReconciliation,
            Some(p) if p.amount_zat < r.invoice.amount_zat => MockStatus::Underpaid,
            // A qualifying payment still short of its confirmations.
            Some(_) => MockStatus::PaymentSeen,
        }
    }
}

/// The deadline rule: `0 < nExpiryHeight <= deadline`, included at or below the deadline.
fn qualifies(p: &Payment, inv: &Invoice) -> bool {
    p.expiry_height != 0
        && p.expiry_height <= inv.deadline_height
        && p.included_height <= inv.deadline_height
}

/// A synthetic testnet unified address with one Orchard receiver whose point encoding is
/// deliberately invalid (all 0xff), so it cannot belong to any wallet.
pub fn mock_destination(network: Network, index: u64) -> String {
    let mut receiver = [0xffu8; 43];
    receiver[..8].copy_from_slice(&index.to_be_bytes());
    let ua = unified::Address::try_from_items(vec![unified::Receiver::Orchard(receiver)])
        .expect("one Orchard receiver is a valid unified address");
    ua.encode(&network.network_type())
}

/// The mock issuer: sells services, observes mock payments and signs invoices and grants.
#[derive(Clone)]
pub struct MockIssuer {
    inner: Arc<Mutex<IssuerState>>,
}

impl MockIssuer {
    /// A new issuer with the test signing key.
    pub fn new(params: MockParams) -> Self {
        let tip = params.start_tip;
        MockIssuer {
            inner: Arc::new(Mutex::new(IssuerState {
                params,
                key: SigningKey::from_bytes(&TEST_ISSUER_SEED),
                tip,
                records: Vec::new(),
                by_request: HashMap::new(),
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, IssuerState> {
        self.inner.lock().expect("mock issuer lock")
    }

    /// The issuer's public key.
    pub fn public_key(&self) -> [u8; 32] {
        codec::public_key_bytes(&self.lock().key)
    }

    /// Network of the mock chain.
    pub fn network(&self) -> Network {
        self.lock().params.network
    }

    /// The mock chain tip.
    pub fn tip(&self) -> u32 {
        self.lock().tip
    }

    /// How many purchases the issuer recorded.
    pub fn invoices_created(&self) -> usize {
        self.lock().records.len()
    }

    /// Mines `blocks` blocks and issues due grants. Returns the new tip.
    pub fn mine(&self, blocks: u32) -> u32 {
        let mut s = self.lock();
        s.tip += blocks;
        s.issue_due();
        s.tip
    }

    /// Idempotent per (client NodeId, request_id): the same answer bytes every time.
    pub fn request_invoice(
        &self,
        client: [u8; 32],
        service: &str,
        request_id: [u8; 16],
    ) -> Result<([u8; 16], Vec<u8>), MockError> {
        let mut s = self.lock();
        if let Some(&i) = s.by_request.get(&(client, request_id)) {
            let r = &s.records[i];
            if r.invoice.service != service {
                return Err(MockError::RequestConflict);
            }
            return Ok((r.invoice.purchase_id, r.invoice_bytes.clone()));
        }
        let offer = s
            .params
            .services
            .iter()
            .find(|o| o.name == service)
            .cloned()
            .ok_or_else(|| MockError::UnknownService(service.to_string()))?;
        let index = s.records.len() as u64;
        let mut purchase_id = *b"mockpid:\0\0\0\0\0\0\0\0";
        purchase_id[8..].copy_from_slice(&index.to_be_bytes());
        let invoice = Invoice {
            network: s.params.network,
            issuer_key: codec::public_key_bytes(&s.key),
            provider_node_id: TEST_PROVIDER_NODE_ID,
            client_node_id: client,
            service: service.to_string(),
            purchase_id,
            amount_zat: offer.price_zat,
            destination: mock_destination(s.params.network, index),
            memo: format!("psb-mock {}", hex::encode(purchase_id)).into_bytes(),
            deadline_height: s.tip + s.params.deadline_margin,
            access_blocks: offer.access_blocks,
            confirmations: s.params.confirmations,
        };
        let invoice_bytes = codec::sign_invoice(&invoice, &s.key).map_err(MockError::Encode)?;
        s.records.push(Record {
            client,
            invoice,
            invoice_bytes: invoice_bytes.clone(),
            payment: None,
            grant: None,
        });
        let i = s.records.len() - 1;
        s.by_request.insert((client, request_id), i);
        Ok((purchase_id, invoice_bytes))
    }

    /// Unknown purchases and purchases bound to another client get the same NotFound.
    pub fn get_grant(&self, client: [u8; 32], purchase_id: [u8; 16]) -> MockStatus {
        let mut s = self.lock();
        s.issue_due();
        match s
            .records
            .iter()
            .find(|r| r.invoice.purchase_id == purchase_id && r.client == client)
        {
            Some(r) => s.status(r),
            None => MockStatus::NotFound,
        }
    }

    /// Records a payment included in the next block (which this mines). Test control only.
    /// Without an explicit expiry, the payment carries the expiry a wallet following the
    /// librustzcash default profile sets when it sends at the current tip.
    pub fn pay(
        &self,
        purchase_id: [u8; 16],
        amount_zat: Option<u64>,
        expiry_height: Option<u32>,
    ) -> Result<u32, MockError> {
        let mut s = self.lock();
        let wallet_expiry = LIBRUSTZCASH_DEFAULT
            .expiry_for_tip(s.params.network, s.tip)
            .unwrap_or(0);
        s.tip += 1;
        let tip = s.tip;
        let r = s
            .records
            .iter_mut()
            .find(|r| r.invoice.purchase_id == purchase_id)
            .ok_or(MockError::UnknownPurchase)?;
        if r.payment.is_some() {
            return Err(MockError::AlreadyPaid);
        }
        r.payment = Some(Payment {
            amount_zat: amount_zat.unwrap_or(r.invoice.amount_zat),
            included_height: tip,
            expiry_height: expiry_height.unwrap_or(wallet_expiry),
        });
        s.issue_due();
        Ok(tip)
    }

    /// The signed invoice of a purchase.
    pub fn invoice_bytes(&self, purchase_id: [u8; 16]) -> Option<Vec<u8>> {
        self.lock()
            .records
            .iter()
            .find(|r| r.invoice.purchase_id == purchase_id)
            .map(|r| r.invoice_bytes.clone())
    }

    /// The signed grant of a purchase, once issued.
    pub fn grant_bytes(&self, purchase_id: [u8; 16]) -> Option<Vec<u8>> {
        self.lock()
            .records
            .iter()
            .find(|r| r.invoice.purchase_id == purchase_id)
            .and_then(|r| r.grant.as_ref().map(|(_, b)| b.clone()))
    }

    fn sign_raw(&self, domain: &[u8], body: &[u8]) -> Vec<u8> {
        codec::sign(domain, body, &self.lock().key)
    }
}

struct NodeState {
    node_id: [u8; 32],
    auth_header: String,
    issuer_key: VerificationKey,
    provider: [u8; 32],
    network: Network,
    controls: bool,
    installed: HashMap<[u8; 16], Vec<u8>>,
    drop_next: HashSet<String>,
    fail_next: HashMap<String, (i64, String, bool)>,
    tamper: Tamper,
    calls: HashMap<String, u64>,
}

/// A mock of the user's node: loopback JSON-RPC with cookie auth, one connection at a time.
#[derive(Clone)]
pub struct MockNode {
    /// The loopback address the node listens on.
    pub addr: SocketAddr,
    state: Arc<Mutex<NodeState>>,
    issuer: MockIssuer,
}

impl MockNode {
    /// `controls` enables the `mock_*` methods used by tests and the demo.
    pub fn spawn(
        issuer: MockIssuer,
        node_id: [u8; 32],
        cookie: &str,
        bind: SocketAddr,
        controls: bool,
    ) -> io::Result<MockNode> {
        if !bind.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the mock node only binds to loopback",
            ));
        }
        let issuer_key = codec::verification_key(issuer.public_key())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let listener = TcpListener::bind(bind)?;
        let addr = listener.local_addr()?;
        let state = Arc::new(Mutex::new(NodeState {
            node_id,
            auth_header: format!("Basic {}", STANDARD.encode(cookie.trim())),
            issuer_key,
            provider: TEST_PROVIDER_NODE_ID,
            network: issuer.network(),
            controls,
            installed: HashMap::new(),
            drop_next: HashSet::new(),
            fail_next: HashMap::new(),
            tamper: Tamper::None,
            calls: HashMap::new(),
        }));
        let node = MockNode {
            addr,
            state,
            issuer,
        };
        let server = node.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                server.handle(stream);
            }
        });
        Ok(node)
    }

    fn lock(&self) -> MutexGuard<'_, NodeState> {
        self.state.lock().expect("mock node lock")
    }

    /// The issuer behind this node.
    pub fn issuer(&self) -> &MockIssuer {
        &self.issuer
    }

    /// The NodeId this node claims.
    pub fn node_id(&self) -> [u8; 32] {
        self.lock().node_id
    }

    /// Sets how answers are corrupted.
    pub fn set_tamper(&self, tamper: Tamper) {
        self.lock().tamper = tamper;
    }

    /// The next call of `method` is processed, then the connection closes without an answer.
    pub fn drop_next(&self, method: &str) {
        self.lock().drop_next.insert(method.to_string());
    }

    /// The next call of `method` answers with JSON-RPC error `code`. With `after_processing`
    /// the call is processed first (the provider records the request), then the error is sent.
    pub fn fail_next(&self, method: &str, code: i64, message: &str, after_processing: bool) {
        self.lock().fail_next.insert(
            method.to_string(),
            (code, message.to_string(), after_processing),
        );
    }

    /// The grant installed for a purchase.
    pub fn installed(&self, purchase_id: [u8; 16]) -> Option<Vec<u8>> {
        self.lock().installed.get(&purchase_id).cloned()
    }

    /// How many grants are installed.
    pub fn installed_count(&self) -> usize {
        self.lock().installed.len()
    }

    /// How many times `method` was called.
    pub fn calls(&self, method: &str) -> u64 {
        self.lock().calls.get(method).copied().unwrap_or(0)
    }

    fn handle(&self, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        let Ok(msg) = read_http_message(&mut stream, 64 * 1024) else {
            return;
        };
        let authorized = msg.header("authorization") == Some(self.lock().auth_header.as_str());
        if !authorized {
            let _ = write_http_response(&mut stream, 401, "Unauthorized", b"{}");
            return;
        }
        let Ok(request) = serde_json::from_slice::<Value>(&msg.body) else {
            let body = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}});
            let _ = write_http_response(&mut stream, 200, "OK", body.to_string().as_bytes());
            return;
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let injected = self.lock().fail_next.remove(&method);
        let outcome = match injected {
            Some((code, message, false)) => Err((code, message)),
            Some((code, message, true)) => {
                let _ = self.dispatch(&method, &params);
                Err((code, message))
            }
            None => self.dispatch(&method, &params),
        };
        let drop = {
            let mut s = self.lock();
            *s.calls.entry(method.clone()).or_insert(0) += 1;
            s.drop_next.remove(&method)
        };
        if drop {
            return;
        }
        let body = match outcome {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        let _ = write_http_response(&mut stream, 200, "OK", body.to_string().as_bytes());
    }

    fn dispatch(&self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        let str_param = |name: &str| -> Result<String, (i64, String)> {
            params
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or((-32602, format!("missing string parameter {name}")))
        };
        let hex16 = |name: &str| -> Result<[u8; 16], (i64, String)> {
            hex_array::<16>(&str_param(name)?).map_err(|e| (-32602, format!("{name}: {e}")))
        };
        let node_id = self.node_id();
        match method {
            "getminermeshinfo" => Ok(json!({"node_id": hex::encode(node_id), "simulated": true})),
            "getblockcount" => Ok(json!(self.issuer.tip())),
            "paidstream_requestinvoice" => {
                let service = str_param("service")?;
                let request_id = hex16("request_id")?;
                let (purchase_id, bytes) = self
                    .issuer
                    .request_invoice(node_id, &service, request_id)
                    .map_err(|e| (e.code(), e.message()))?;
                if self.lock().tamper == Tamper::MalformedInvoiceAnswer {
                    // The provider recorded the request; the answer arrives garbled.
                    return Ok(
                        json!({"purchase_id": hex::encode(purchase_id), "invoice": "not hex"}),
                    );
                }
                let bytes = self.tamper_invoice(bytes);
                Ok(json!({"purchase_id": hex::encode(purchase_id), "invoice": hex::encode(&bytes)}))
            }
            "paidstream_getgrant" => {
                let purchase_id = hex16("purchase_id")?;
                let status = self.issuer.get_grant(node_id, purchase_id);
                Ok(self.tamper_status(status))
            }
            "paidstream_setgrant" => self.set_grant(&str_param("grant")?),
            "mock_pay" | "mock_mine" | "mock_drop_next" | "mock_tamper"
                if !self.lock().controls =>
            {
                Err((-32601, format!("method {method} not found")))
            }
            "mock_pay" => {
                let purchase_id = hex16("purchase_id")?;
                let amount = params.get("amount_zat").and_then(Value::as_u64);
                let expiry = params
                    .get("expiry_height")
                    .and_then(Value::as_u64)
                    .map(|h| h as u32);
                let height = self
                    .issuer
                    .pay(purchase_id, amount, expiry)
                    .map_err(|e| (e.code(), e.message()))?;
                Ok(json!({"included_height": height, "simulated": true}))
            }
            "mock_mine" => {
                let blocks = params
                    .get("blocks")
                    .and_then(Value::as_u64)
                    .ok_or((-32602, "missing blocks".to_string()))?;
                Ok(json!({"tip": self.issuer.mine(blocks as u32)}))
            }
            "mock_drop_next" => {
                self.drop_next(&str_param("method")?);
                Ok(json!(true))
            }
            "mock_tamper" => {
                let mode = str_param("mode")?;
                let tamper =
                    Tamper::parse(&mode).ok_or((-32602, format!("unknown tamper mode {mode}")))?;
                self.set_tamper(tamper);
                Ok(json!(true))
            }
            _ => Err((-32601, format!("method {method} not found"))),
        }
    }

    /// Verifies the grant like a node would before using it, then records it (idempotent).
    fn set_grant(&self, grant_hex: &str) -> Result<Value, (i64, String)> {
        let invalid = |e: String| (CODE_INVALID_GRANT, format!("InvalidGrant: {e}"));
        let bytes = hex_bytes(grant_hex).map_err(invalid)?;
        let mut s = self.lock();
        let grant =
            codec::verify_grant(&bytes, &s.issuer_key).map_err(|e| invalid(e.to_string()))?;
        if grant.network != s.network {
            return Err(invalid("grant is for another network".into()));
        }
        if grant.provider_node_id != s.provider {
            return Err(invalid("grant names another provider".into()));
        }
        if grant.client_node_id != s.node_id {
            return Err(invalid(
                "grant is bound to another NodeId than this node's".into(),
            ));
        }
        if grant.end_height <= grant.start_height {
            return Err(invalid("grant has an empty validity range".into()));
        }
        match s.installed.get(&grant.purchase_id) {
            Some(existing) if existing != &bytes => {
                return Err(invalid(
                    "a different grant is already installed for this purchase".into(),
                ))
            }
            _ => {}
        }
        s.installed.insert(grant.purchase_id, bytes);
        Ok(json!({
            "installed": true,
            "start_height": grant.start_height,
            "end_height": grant.end_height,
            "simulated": true,
            "note": "mock install; no stream is consumed"
        }))
    }

    fn tamper_invoice(&self, bytes: Vec<u8>) -> Vec<u8> {
        let tamper = self.lock().tamper;
        let body = &bytes[..bytes.len() - SIGNATURE_LEN];
        let resign = |f: &dyn Fn(&mut Invoice)| {
            let mut inv = Invoice::decode_body(body).expect("mock invoice decodes");
            f(&mut inv);
            self.issuer.sign_raw(
                INVOICE_DOMAIN,
                &inv.encode_body().expect("mock invoice encodes"),
            )
        };
        match tamper {
            Tamper::InvoiceWrongKey => codec::sign(
                INVOICE_DOMAIN,
                body,
                &SigningKey::from_bytes(&OTHER_ISSUER_SEED),
            ),
            Tamper::InvoiceGrantDomain => self.issuer.sign_raw(GRANT_DOMAIN, body),
            Tamper::InvoiceWrongNetwork => resign(&|i| i.network = Network::Main),
            Tamper::InvoiceWrongProvider => resign(&|i| i.provider_node_id = [0x51; 32]),
            Tamper::InvoiceWrongClient => resign(&|i| i.client_node_id = OTHER_CLIENT_NODE_ID),
            Tamper::InvoiceWrongService => resign(&|i| i.service = "synthetic-20".into()),
            _ => bytes,
        }
    }

    fn tamper_status(&self, status: MockStatus) -> Value {
        let tamper = self.lock().tamper;
        if tamper == Tamper::IssuedWithoutGrant {
            return json!({"status": "Issued"});
        }
        let (MockStatus::Issued(bytes) | MockStatus::Expired(Some(bytes))) = &status else {
            return status.to_json();
        };
        let body = &bytes[..bytes.len() - SIGNATURE_LEN];
        let forged = match tamper {
            Tamper::GrantWrongKey => codec::sign(
                GRANT_DOMAIN,
                body,
                &SigningKey::from_bytes(&OTHER_ISSUER_SEED),
            ),
            Tamper::GrantInvoiceDomain => self.issuer.sign_raw(INVOICE_DOMAIN, body),
            Tamper::GrantOtherClient => {
                let mut g = Grant::decode_body(body).expect("mock grant decodes");
                g.client_node_id = OTHER_CLIENT_NODE_ID;
                self.issuer
                    .sign_raw(GRANT_DOMAIN, &g.encode_body().expect("mock grant encodes"))
            }
            _ => return status.to_json(),
        };
        json!({"status": status.name(), "grant": hex::encode(&forged)})
    }
}
