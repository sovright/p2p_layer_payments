//! The bridge: buy, status, resume and show.
//!
//! The bridge holds no keys and builds no transactions. It takes access decisions only from
//! issuer-signed objects that verify against its pinned issuer key. A signed grant is the
//! trusted issuer's access authorization, not on-chain proof of payment; the bridge never
//! derives a grant or a payment from a local txid, a user statement or the node's chain RPC.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::checks::{self, Expectations};
use crate::codec::{self, Invoice, Network};
use crate::contract;
use crate::hexutil;
use crate::rpc::{RpcClient, RpcError};
use crate::state::{Install, Purchase, Stage, StateError, StateFile, StateStore};
use crate::uri;

pub const SIMULATION_NOTICE: &str = "SIMULATION ONLY: this prototype talks to a mock node and a mock issuer. \
No wallet, no Zakura node and no payment are involved. paidstream_* are proposed RPC contracts, not existing \
Zakura RPCs, and a mock install does not show that a node consumes a stream. Do not open a printed payment \
request in a wallet: wallet expiry support is not solved.";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The user's node RPC (in this prototype, the mock node). Must be loopback.
    pub node_rpc: SocketAddr,
    pub cookie_file: PathBuf,
    pub state_file: PathBuf,
    pub network: Network,
    /// Pinned together with the issuer key.
    pub provider_node_id: String,
    pub issuer_public_key: String,
    pub purchase_limit_zat: u64,
}

impl Config {
    /// Reads a JSON config. Relative paths are taken relative to the config file.
    pub fn load(path: &Path) -> Result<Self, BridgeError> {
        let err = |e: String| BridgeError::Config(format!("{}: {e}", path.display()));
        let text = std::fs::read_to_string(path).map_err(|e| err(e.to_string()))?;
        let mut cfg: Config = serde_json::from_str(&text).map_err(|e| err(e.to_string()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        if cfg.cookie_file.is_relative() {
            cfg.cookie_file = base.join(&cfg.cookie_file);
        }
        if cfg.state_file.is_relative() {
            cfg.state_file = base.join(&cfg.state_file);
        }
        Ok(cfg)
    }
}

/// Test hook: stop right after a given state write, as a crash would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    AfterRequested,
    AfterInvoiced,
    AfterGranted,
    AfterInstalled,
}

#[derive(Debug)]
pub enum BridgeError {
    Usage(String),
    Config(String),
    State(StateError),
    /// No answer from the node: the outcome is unknown, and `resume` retries the same request.
    NodeUnreachable(String),
    Node(String),
    /// The invoice request did not complete. The purchase and its request_id are kept.
    Retryable(String),
    Rejected(String),
    NotFound(String),
    SimulatedCrash(CrashPoint),
}

impl BridgeError {
    pub fn exit_code(&self) -> i32 {
        match self {
            BridgeError::Usage(_) | BridgeError::Config(_) => 2,
            BridgeError::State(StateError::Locked(_)) => 3,
            BridgeError::State(_) => 4,
            BridgeError::NodeUnreachable(_) | BridgeError::Node(_) | BridgeError::Retryable(_) => 5,
            BridgeError::Rejected(_) => 6,
            BridgeError::NotFound(_) => 7,
            BridgeError::SimulatedCrash(_) => 86,
        }
    }
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BridgeError::Usage(e) => write!(f, "usage: {e}"),
            BridgeError::Config(e) => write!(f, "config: {e}"),
            BridgeError::State(e) => write!(f, "{e}"),
            BridgeError::NodeUnreachable(e) => write!(f, "{e}"),
            BridgeError::Node(e) => write!(f, "{e}"),
            BridgeError::Retryable(e) => write!(f, "not completed: {e}"),
            BridgeError::Rejected(e) => write!(f, "rejected: {e}"),
            BridgeError::NotFound(e) => write!(f, "not found: {e}"),
            BridgeError::SimulatedCrash(p) => write!(f, "simulated crash after {p:?}"),
        }
    }
}

impl std::error::Error for BridgeError {}

impl From<StateError> for BridgeError {
    fn from(e: StateError) -> Self {
        BridgeError::State(e)
    }
}

fn node_error(method: &str, e: RpcError) -> BridgeError {
    match e {
        RpcError::Transport(t) => BridgeError::NodeUnreachable(format!(
            "{method} got no answer ({t}); the outcome is unknown"
        )),
        other => BridgeError::Node(format!("{method}: {other}")),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaymentRequest {
    Offered {
        uri: String,
        amount_zat: u64,
        deadline_height: u32,
        /// Shown again for an invoice that was already shown once.
        reshown: bool,
    },
    Withheld(String),
}

#[derive(Clone, Debug)]
pub struct PurchaseView {
    pub request_id: String,
    pub purchase_id: Option<String>,
    pub service: String,
    pub stage: Stage,
    pub last_status: Option<String>,
    pub payment: Option<PaymentRequest>,
    pub install: Option<Install>,
    pub note: Option<String>,
}

pub fn random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut out = [0u8; N];
    File::open("/dev/urandom")?.read_exact(&mut out)?;
    Ok(out)
}

pub struct Bridge {
    cfg: Config,
    rpc: RpcClient,
    issuer_key: VerifyingKey,
    provider: [u8; 32],
    store: StateStore,
    state: StateFile,
    node_id: Option<[u8; 32]>,
    crash: Option<CrashPoint>,
}

impl Bridge {
    /// Takes the state lock and loads the state. A second process on the same state file fails here.
    pub fn open(cfg: Config) -> Result<Self, BridgeError> {
        let key_bytes: [u8; 32] = hexutil::decode_array(&cfg.issuer_public_key)
            .map_err(|e| BridgeError::Config(format!("issuer_public_key: {e}")))?;
        let issuer_key = VerifyingKey::from_bytes(&key_bytes)
            .map_err(|_| BridgeError::Config("issuer_public_key is not an Ed25519 key".into()))?;
        let provider = hexutil::decode_array(&cfg.provider_node_id)
            .map_err(|e| BridgeError::Config(format!("provider_node_id: {e}")))?;
        let rpc = RpcClient::from_cookie_file(cfg.node_rpc, &cfg.cookie_file)
            .map_err(BridgeError::Config)?;
        let (store, state) = StateStore::open(&cfg.state_file)?;
        Ok(Bridge {
            cfg,
            rpc,
            issuer_key,
            provider,
            store,
            state,
            node_id: None,
            crash: None,
        })
    }

    pub fn set_crash_point(&mut self, point: Option<CrashPoint>) {
        self.crash = point;
    }

    pub fn state(&self) -> &StateFile {
        &self.state
    }

    pub fn views(&self) -> Vec<PurchaseView> {
        (0..self.state.purchases.len())
            .map(|i| self.view(i, None))
            .collect()
    }

    fn view(&self, idx: usize, payment: Option<PaymentRequest>) -> PurchaseView {
        let p = &self.state.purchases[idx];
        PurchaseView {
            request_id: p.request_id.clone(),
            purchase_id: p.purchase_id.clone(),
            service: p.service.clone(),
            stage: p.stage,
            last_status: p.last_status.clone(),
            payment,
            install: p.install,
            note: p.note.clone(),
        }
    }

    fn view_note(&self, idx: usize, note: String) -> PurchaseView {
        let mut v = self.view(idx, None);
        v.note = Some(note);
        v
    }

    fn withheld(&self, idx: usize, reason: impl Into<String>) -> PurchaseView {
        self.view(idx, Some(PaymentRequest::Withheld(reason.into())))
    }

    fn persist(&mut self, point: Option<CrashPoint>) -> Result<(), BridgeError> {
        self.store.save(&self.state)?;
        match point {
            Some(p) if self.crash == Some(p) => Err(BridgeError::SimulatedCrash(p)),
            _ => Ok(()),
        }
    }

    fn set_note(&mut self, idx: usize, note: Option<String>) -> Result<(), BridgeError> {
        self.state.purchases[idx].note = note;
        self.persist(None)
    }

    fn node_id(&mut self) -> Result<[u8; 32], BridgeError> {
        if let Some(id) = self.node_id {
            return Ok(id);
        }
        let v = self
            .rpc
            .call("getminermeshinfo", json!([]))
            .map_err(|e| node_error("getminermeshinfo", e))?;
        let hex = v.get("node_id").and_then(Value::as_str).ok_or_else(|| {
            BridgeError::Node(
                "the node reports no v2 node ID (getminermeshinfo.node_id is null)".into(),
            )
        })?;
        let id = hexutil::decode_array(hex)
            .map_err(|e| BridgeError::Node(format!("getminermeshinfo.node_id: {e}")))?;
        self.node_id = Some(id);
        Ok(id)
    }

    fn expectations(&mut self) -> Result<Expectations, BridgeError> {
        Ok(Expectations {
            network: self.cfg.network,
            issuer_key: self.issuer_key.to_bytes(),
            provider_node_id: self.provider,
            client_node_id: self.node_id()?,
        })
    }

    fn find(&self, which: &str) -> Result<usize, BridgeError> {
        let w = which.to_ascii_lowercase();
        if w.len() < 8 {
            return Err(BridgeError::Usage(
                "give at least 8 hex characters of a purchase_id or request_id".into(),
            ));
        }
        let matches: Vec<usize> = self
            .state
            .purchases
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.request_id.starts_with(&w)
                    || p.purchase_id
                        .as_deref()
                        .is_some_and(|id| id.starts_with(&w))
            })
            .map(|(i, _)| i)
            .collect();
        match matches.as_slice() {
            [] => Err(BridgeError::NotFound(format!(
                "no purchase matches {which}"
            ))),
            [one] => Ok(*one),
            _ => Err(BridgeError::Usage(format!(
                "{which} matches more than one purchase"
            ))),
        }
    }

    /// Re-verifies the stored invoice, so an edited state file can never yield a payment request.
    fn stored_invoice(&self, idx: usize) -> Result<Invoice, BridgeError> {
        let hex = self.state.purchases[idx]
            .invoice
            .as_deref()
            .ok_or_else(|| BridgeError::Rejected("purchase has no stored invoice".into()))?;
        let bytes = hexutil::decode(hex).map_err(BridgeError::Rejected)?;
        codec::verify_invoice(&bytes, &self.issuer_key)
            .map_err(|e| BridgeError::Rejected(format!("stored invoice no longer verifies: {e}")))
    }

    fn reject(&mut self, idx: usize, reason: String) -> Result<PurchaseView, BridgeError> {
        let p = &mut self.state.purchases[idx];
        p.stage = Stage::Rejected;
        p.note = Some(reason.clone());
        self.persist(None)?;
        Err(BridgeError::Rejected(reason))
    }

    /// Starts a purchase, or continues the open one for the same service.
    pub fn buy(&mut self, service: &str) -> Result<PurchaseView, BridgeError> {
        if service.is_empty() || service.len() > codec::MAX_SERVICE_LEN {
            return Err(BridgeError::Usage(format!(
                "service name must be 1 to {} bytes",
                codec::MAX_SERVICE_LEN
            )));
        }
        if let Some(idx) = self
            .state
            .purchases
            .iter()
            .position(|p| p.service == service && p.stage.is_open())
        {
            return match self.state.purchases[idx].stage {
                Stage::Requested => self.request_invoice(idx),
                Stage::Invoiced => self.present(idx, true),
                _ => Ok(self.view_note(
                    idx,
                    "this service already has a granted purchase; no new purchase was started"
                        .into(),
                )),
            };
        }
        let request_id = random_bytes::<16>()
            .map_err(|e| BridgeError::Config(format!("cannot read /dev/urandom: {e}")))?;
        self.state.purchases.push(Purchase {
            request_id: hexutil::encode(&request_id),
            service: service.to_string(),
            stage: Stage::Requested,
            purchase_id: None,
            invoice: None,
            last_status: None,
            grant: None,
            install: None,
            note: None,
        });
        let idx = self.state.purchases.len() - 1;
        self.persist(Some(CrashPoint::AfterRequested))?;
        self.request_invoice(idx)
    }

    fn request_invoice(&mut self, idx: usize) -> Result<PurchaseView, BridgeError> {
        let exp = self.expectations()?;
        let (service, request_id) = {
            let p = &self.state.purchases[idx];
            (p.service.clone(), p.request_id.clone())
        };
        let answer = match self.rpc.call(
            "paidstream_requestinvoice",
            json!({"service": service, "request_id": request_id}),
        ) {
            Ok(v) => v,
            Err(RpcError::Transport(e)) => {
                return Err(BridgeError::NodeUnreachable(format!(
                    "paidstream_requestinvoice got no answer ({e}); the outcome is unknown. \
                     Run `resume` to retry with the same request_id"
                )))
            }
            Err(RpcError::Rpc { code, message }) if contract::is_final_invoice_error(code) => {
                return self.reject(
                    idx,
                    format!("the provider refused the invoice request (code {code}): {message}"),
                )
            }
            Err(RpcError::Rpc { code, message }) => {
                let what = match code {
                    contract::CODE_PROVIDER_UNREACHABLE => "the node could not reach the provider",
                    contract::CODE_DISABLED => "the paid-stream service is disabled on the node",
                    _ => "the node answered with an unrecognized error code",
                };
                let msg = format!(
                    "{what} (code {code}): {message}. The purchase and its request_id are kept; \
                     run `resume` or `buy` again to retry the same request"
                );
                self.set_note(idx, Some(msg.clone()))?;
                return Err(BridgeError::Retryable(msg));
            }
            Err(e @ RpcError::Protocol(_)) => {
                return Err(node_error("paidstream_requestinvoice", e))
            }
        };
        let field = |name: &str| -> Result<String, String> {
            answer
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("answer has no {name}"))
        };
        let parsed = field("purchase_id")
            .and_then(|pid| hexutil::decode_array::<16>(&pid))
            .and_then(|pid| {
                field("invoice")
                    .and_then(|inv| hexutil::decode(&inv))
                    .map(|inv| (pid, inv))
            });
        let (purchase_id, invoice_bytes) = match parsed {
            Ok(x) => x,
            Err(e) => return self.reject(idx, format!("malformed invoice answer: {e}")),
        };
        let inv = match codec::verify_invoice(&invoice_bytes, &self.issuer_key) {
            Ok(inv) => inv,
            Err(e) => return self.reject(idx, format!("invoice rejected: {e}")),
        };
        if let Err(e) = checks::check_invoice(
            &inv,
            &exp,
            &service,
            &purchase_id,
            self.cfg.purchase_limit_zat,
        ) {
            return self.reject(idx, format!("invoice rejected: {e}"));
        }
        {
            let p = &mut self.state.purchases[idx];
            p.purchase_id = Some(hexutil::encode(&purchase_id));
            p.invoice = Some(hexutil::encode(&invoice_bytes));
            p.stage = Stage::Invoiced;
            p.note = None;
        }
        self.persist(Some(CrashPoint::AfterInvoiced))?;
        self.present(idx, false)
    }

    /// Offers the payment request only while the provider still reports the purchase as
    /// pending and the local node's height is below the deadline. Otherwise it is withheld.
    fn present(&mut self, idx: usize, reshown: bool) -> Result<PurchaseView, BridgeError> {
        let inv = self.stored_invoice(idx)?;
        let pid = self.state.purchases[idx]
            .purchase_id
            .clone()
            .ok_or_else(|| BridgeError::Rejected("invoiced purchase has no purchase_id".into()))?;
        let status = match self
            .rpc
            .call("paidstream_getgrant", json!({"purchase_id": pid}))
        {
            Ok(v) => v.get("status").and_then(Value::as_str).map(str::to_string),
            Err(e) => {
                return Ok(self.withheld(
                    idx,
                    format!("could not confirm that the purchase is still pending ({e}); no payment request shown"),
                ))
            }
        };
        let Some(status) = status else {
            return Ok(self.withheld(
                idx,
                "the provider's answer has no status; no payment request shown",
            ));
        };
        if self.state.purchases[idx].last_status.as_deref() != Some(status.as_str()) {
            self.state.purchases[idx].last_status = Some(status.clone());
            self.persist(None)?;
        }
        if status != "Pending" {
            return Ok(self.withheld(
                idx,
                format!("purchase status is {status}; no payment request shown. Run `status` to follow it"),
            ));
        }
        let tip = self
            .rpc
            .call("getblockcount", json!([]))
            .ok()
            .and_then(|v| v.as_u64());
        let payment = match tip {
            None => PaymentRequest::Withheld(
                "could not read the node's chain height; no payment request shown".into(),
            ),
            Some(t) if t >= u64::from(inv.deadline_height) => PaymentRequest::Withheld(format!(
                "the node's chain height {t} has reached the deadline height {}; no payment request shown",
                inv.deadline_height
            )),
            Some(_) => {
                let request = uri::build_payment_uri(&inv);
                uri::verify_payment_uri(&request, &inv, self.cfg.network)
                    .map_err(BridgeError::Rejected)?;
                PaymentRequest::Offered {
                    uri: request,
                    amount_zat: inv.amount_zat,
                    deadline_height: inv.deadline_height,
                    reshown,
                }
            }
        };
        Ok(self.view(idx, Some(payment)))
    }

    /// Asks the provider (through the node) for the status of open purchases.
    pub fn status(&mut self, which: Option<&str>) -> Result<Vec<PurchaseView>, BridgeError> {
        let targets: Vec<usize> = match which {
            Some(w) => vec![self.find(w)?],
            None => self.open_indices(),
        };
        let mut out = Vec::new();
        for idx in targets {
            out.push(self.refresh(idx)?);
        }
        Ok(out)
    }

    fn open_indices(&self) -> Vec<usize> {
        (0..self.state.purchases.len())
            .filter(|&i| self.state.purchases[i].stage.is_open())
            .collect()
    }

    fn refresh(&mut self, idx: usize) -> Result<PurchaseView, BridgeError> {
        let result = match self.state.purchases[idx].stage {
            Stage::Requested => Ok(self.view_note(
                idx,
                "no verified invoice yet; run `resume` to retry the invoice request with the same request_id".into(),
            )),
            Stage::Rejected | Stage::Expired => Ok(self.view(idx, None)),
            Stage::Granted => self.install(idx),
            Stage::Invoiced | Stage::Installed => self.poll_grant(idx),
        };
        match result {
            Err(BridgeError::NodeUnreachable(e)) | Err(BridgeError::Node(e)) => {
                Ok(self.view_note(idx, format!("{e}; nothing was changed")))
            }
            other => other,
        }
    }

    fn poll_grant(&mut self, idx: usize) -> Result<PurchaseView, BridgeError> {
        let inv = self.stored_invoice(idx)?;
        let exp = self.expectations()?;
        let pid = hexutil::encode(&inv.purchase_id);
        let answer = self
            .rpc
            .call("paidstream_getgrant", json!({"purchase_id": pid}))
            .map_err(|e| node_error("paidstream_getgrant", e))?;
        let status = answer
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| BridgeError::Node("paidstream_getgrant answer has no status".into()))?
            .to_string();
        let grant_hex = answer
            .get("grant")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.state.purchases[idx].last_status = Some(status.clone());

        match (status.as_str(), grant_hex) {
            ("Issued", Some(g)) | ("Expired", Some(g)) => {
                if let Err(reason) = self.accept_grant(idx, &g, &inv, &exp) {
                    self.set_note(
                        idx,
                        Some(format!("grant rejected: {reason}; nothing was installed")),
                    )?;
                    return Ok(self.view(idx, None));
                }
                let stage = self.state.purchases[idx].stage;
                if status == "Expired" {
                    self.state.purchases[idx].stage = Stage::Expired;
                    self.set_note(idx, None)?;
                    return Ok(self.view(idx, None));
                }
                if stage == Stage::Installed {
                    self.persist(None)?;
                    return Ok(self.view(idx, None));
                }
                self.state.purchases[idx].stage = Stage::Granted;
                self.state.purchases[idx].note = None;
                self.persist(Some(CrashPoint::AfterGranted))?;
                self.install(idx)
            }
            ("Issued", None) => {
                self.set_note(
                    idx,
                    Some("the answer said Issued but carried no grant; ignored. Access is only taken from a verified grant".into()),
                )?;
                Ok(self.view(idx, None))
            }
            ("Expired", None) => {
                self.state.purchases[idx].stage = Stage::Expired;
                self.set_note(idx, None)?;
                Ok(self.view(idx, None))
            }
            ("Pending" | "Underpaid" | "NeedsReconciliation" | "NotFound", _) => {
                self.persist(None)?;
                Ok(self.view(idx, None))
            }
            (other, _) => {
                let note = format!("unknown provider status {other:?}; ignored");
                self.set_note(idx, Some(note))?;
                Ok(self.view(idx, None))
            }
        }
    }

    fn accept_grant(
        &mut self,
        idx: usize,
        grant_hex: &str,
        inv: &Invoice,
        exp: &Expectations,
    ) -> Result<(), String> {
        let bytes = hexutil::decode(grant_hex)?;
        let grant = codec::verify_grant(&bytes, &self.issuer_key).map_err(|e| e.to_string())?;
        checks::check_grant(&grant, inv, exp).map_err(|e| e.to_string())?;
        let encoded = hexutil::encode(&bytes);
        let p = &mut self.state.purchases[idx];
        if let Some(existing) = &p.grant {
            if existing != &encoded {
                return Err(
                    "the provider returned a different grant for the same purchase; the first one is kept"
                        .into(),
                );
            }
        }
        p.grant = Some(encoded);
        Ok(())
    }

    fn install(&mut self, idx: usize) -> Result<PurchaseView, BridgeError> {
        let grant_hex = self.state.purchases[idx]
            .grant
            .clone()
            .ok_or_else(|| BridgeError::Rejected("purchase has no stored grant".into()))?;
        let bytes = hexutil::decode(&grant_hex).map_err(BridgeError::Rejected)?;
        let grant = codec::verify_grant(&bytes, &self.issuer_key)
            .map_err(|e| BridgeError::Rejected(format!("stored grant no longer verifies: {e}")))?;
        match self
            .rpc
            .call("paidstream_setgrant", json!({"grant": grant_hex}))
        {
            Ok(v) => {
                let installed = v.get("installed").and_then(Value::as_bool) == Some(true);
                let start = v.get("start_height").and_then(Value::as_u64);
                let end = v.get("end_height").and_then(Value::as_u64);
                if !installed
                    || start != Some(u64::from(grant.start_height))
                    || end != Some(u64::from(grant.end_height))
                {
                    self.set_note(
                        idx,
                        Some("the node's setgrant answer does not match the grant; not marked installed".into()),
                    )?;
                    return Ok(self.view(idx, None));
                }
                let p = &mut self.state.purchases[idx];
                p.install = Some(Install {
                    start_height: grant.start_height,
                    end_height: grant.end_height,
                });
                if p.stage != Stage::Expired {
                    p.stage = Stage::Installed;
                }
                p.note = None;
                self.persist(Some(CrashPoint::AfterInstalled))?;
                Ok(self.view(idx, None))
            }
            Err(RpcError::Transport(e)) => {
                self.set_note(
                    idx,
                    Some(format!(
                        "paidstream_setgrant got no answer ({e}); run `resume` to install the same grant again"
                    )),
                )?;
                Ok(self.view(idx, None))
            }
            Err(e) => {
                self.set_note(idx, Some(format!("the node refused the grant: {e}")))?;
                Ok(self.view(idx, None))
            }
        }
    }

    /// Finishes whatever a lost answer or a restart left open, always with the stored ids.
    pub fn resume(&mut self) -> Result<Vec<PurchaseView>, BridgeError> {
        let mut out = Vec::new();
        for idx in self.open_indices() {
            let result = match self.state.purchases[idx].stage {
                Stage::Requested => self.request_invoice(idx),
                Stage::Invoiced => match self.refresh(idx) {
                    Ok(_) if self.state.purchases[idx].stage == Stage::Invoiced => {
                        self.present(idx, true)
                    }
                    other => other,
                },
                Stage::Granted => self.install(idx),
                Stage::Installed => self.install(idx).and_then(|_| self.refresh(idx)),
                Stage::Rejected | Stage::Expired => Ok(self.view(idx, None)),
            };
            match result {
                Ok(v) => out.push(v),
                Err(e @ (BridgeError::SimulatedCrash(_) | BridgeError::State(_))) => return Err(e),
                Err(e) => out.push(self.view_note(idx, e.to_string())),
            }
        }
        Ok(out)
    }

    /// Shows the payment request of an invoiced purchase again, under the same rules as `buy`.
    pub fn show(&mut self, which: &str) -> Result<PurchaseView, BridgeError> {
        let idx = self.find(which)?;
        match self.state.purchases[idx].stage {
            Stage::Invoiced => self.present(idx, true),
            Stage::Requested => Ok(self.withheld(idx, "no verified invoice yet; run `resume`")),
            Stage::Rejected => Ok(self.withheld(
                idx,
                "the invoice failed a check; it is never shown as a payment request",
            )),
            Stage::Granted | Stage::Installed => {
                Ok(self.withheld(idx, "a grant was already issued for this purchase"))
            }
            Stage::Expired => Ok(self.withheld(idx, "the purchase expired")),
        }
    }
}
