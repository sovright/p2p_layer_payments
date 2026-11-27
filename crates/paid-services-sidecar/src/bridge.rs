//! The sidecar's purchase lifecycle: buy, status, resume and show.
//!
//! The sidecar holds no wallet spending keys and builds no transactions. It takes access
//! decisions only from issuer-signed objects that verify against its pinned issuer key. A
//! signed grant is the trusted issuer's access authorization, not on-chain proof of payment;
//! the sidecar never derives a grant or a payment from a local txid, a user statement or the
//! node's chain RPC.
//!
//! The lifecycle is the same for every service. A [`ServiceAdapter`] adds only what is
//! specific to a service kind: which terms it sells, its `service_params`, and how a grant is
//! redeemed.

use std::path::Path;
use std::time::Duration;

use paid_services_relay::RelayAdapter;
use paid_services_types::checks::{self, Expectations};
use paid_services_types::codec::{self, Entitlement, Invoice, VerificationKey};
use paid_services_types::contract;
use paid_services_types::expiry::{self, WalletExpiryProfile};
use paid_services_types::rpc::{
    self, GetGrantParams, GetGrantResult, InstallGrantParams, InstallGrantResult, PurchaseStatus,
    RequestInvoiceParams, RequestInvoiceResult,
};
use paid_services_types::service::{Redemption, ServiceAdapter};
use paid_services_types::{hex_array, hex_bytes, uri};
use rand_core::{OsRng, RngCore};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use crate::rpc::{RpcClient, RpcError};
use crate::state::{Purchase, Stage, StateError, StateFile, StateStore};

/// The sidecar configuration (see [`paid_services_types::config::SidecarConfig`]).
pub use paid_services_types::config::SidecarConfig as Config;

/// Printed by every command: what this prototype is not.
pub const SIMULATION_NOTICE: &str = "SIMULATION ONLY: this prototype talks to a mock node and a mock issuer. \
No wallet, no Zakura node and no payment are involved. paidsvc_* are proposed RPC contracts, not existing \
Zakura RPCs, and a mock install does not show that a node consumes a stream. Do not open a printed payment \
request in a wallet: wallet expiry support is not solved.";

/// The service adapters this sidecar knows. The relay stream is the only one so far.
pub fn adapter_for(service_kind: &str) -> Option<&'static dyn ServiceAdapter> {
    static RELAY: RelayAdapter = RelayAdapter;
    let adapters: [&'static dyn ServiceAdapter; 1] = [&RELAY];
    adapters
        .into_iter()
        .find(|a| a.service_kind() == service_kind)
}

/// Reads a JSON config. Relative paths are taken relative to the config file.
pub fn load_config(path: &Path) -> Result<Config, BridgeError> {
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

/// Test hook: stop right after a given state write, as a crash would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    /// After the request_id is written, before the invoice request goes out.
    AfterRequested,
    /// After the verified invoice is written.
    AfterInvoiced,
    /// After the verified grant is written, before it is redeemed.
    AfterGranted,
    /// After the redeemed grant is written.
    AfterRedeemed,
}

/// Why a command did not complete.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// A bad command line.
    #[error("usage: {0}")]
    Usage(String),
    /// A bad configuration.
    #[error("config: {0}")]
    Config(String),
    /// The state file is locked or unreadable.
    #[error(transparent)]
    State(#[from] StateError),
    /// No answer from the node: the outcome is unknown, and `resume` retries the same request.
    #[error("{0}")]
    NodeUnreachable(String),
    /// The node answered with an error or something unexpected.
    #[error("{0}")]
    Node(String),
    /// The invoice request did not complete. The purchase and its request_id are kept.
    #[error("not completed: {0}")]
    Retryable(String),
    /// The provider refused the purchase, or an object failed a check.
    #[error("rejected: {0}")]
    Rejected(String),
    /// No purchase matches.
    #[error("not found: {0}")]
    NotFound(String),
    /// The test hook stopped the command.
    #[error("simulated crash after {0:?}")]
    SimulatedCrash(CrashPoint),
}

impl BridgeError {
    /// The process exit code for this error.
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

fn node_error(method: &str, e: RpcError) -> BridgeError {
    match e {
        RpcError::Transport(t) => BridgeError::NodeUnreachable(format!(
            "{method} got no answer ({t}); the outcome is unknown"
        )),
        other => BridgeError::Node(format!("{method}: {other}")),
    }
}

/// Whether a payment request is shown, and why not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaymentRequest {
    /// The request is shown.
    Offered {
        /// The ZIP 321 URI.
        uri: String,
        /// Price in zatoshis.
        amount_zat: u64,
        /// The invoice deadline height.
        deadline_height: u32,
        /// The `nExpiryHeight` a wallet following the configured profile would set now.
        expected_expiry: u32,
        /// Shown again for an invoice that was already shown once.
        reshown: bool,
    },
    /// The request is withheld, with the reason.
    Withheld(String),
}

/// What a command reports about one purchase.
#[derive(Clone, Debug)]
pub struct PurchaseView {
    /// The request_id (hex).
    pub request_id: String,
    /// The provider's purchase id (hex), once known.
    pub purchase_id: Option<String>,
    /// The versioned service kind.
    pub service_kind: String,
    /// The offer bought.
    pub offer_id: String,
    /// Where the purchase stands.
    pub stage: Stage,
    /// The provider's last reported status.
    pub last_status: Option<String>,
    /// The payment request, when a command decided on one.
    pub payment: Option<PaymentRequest>,
    /// The verified grant's entitlement, once granted.
    pub entitlement: Option<Entitlement>,
    /// A note about the last step.
    pub note: Option<String>,
}

/// Random bytes from the operating system's generator.
pub fn random_bytes<const N: usize>() -> Result<[u8; N], rand_core::Error> {
    let mut out = [0u8; N];
    OsRng.try_fill_bytes(&mut out)?;
    Ok(out)
}

fn status_name(status: PurchaseStatus) -> String {
    format!("{status:?}")
}

/// An open sidecar: configuration, node client and locked state.
pub struct Bridge {
    cfg: Config,
    rpc: RpcClient,
    issuer_key: VerificationKey,
    issuer_key_bytes: [u8; 32],
    provider: [u8; 32],
    store: StateStore,
    state: StateFile,
    node_id: Option<[u8; 32]>,
    crash: Option<CrashPoint>,
    profile: WalletExpiryProfile,
}

impl Bridge {
    /// Takes the state lock and loads the state. A second process on the same state file fails here.
    pub fn open(cfg: Config) -> Result<Self, BridgeError> {
        let key_bytes: [u8; 32] = hex_array(&cfg.issuer_public_key)
            .map_err(|e| BridgeError::Config(format!("issuer_public_key: {e}")))?;
        let issuer_key = codec::verification_key(key_bytes)
            .map_err(|_| BridgeError::Config("issuer_public_key is not an Ed25519 key".into()))?;
        let provider = hex_array(&cfg.provider_node_id)
            .map_err(|e| BridgeError::Config(format!("provider_node_id: {e}")))?;
        let rpc = RpcClient::from_cookie_file(cfg.node_rpc, &cfg.cookie_file)
            .map_err(BridgeError::Config)?;
        let profile = expiry::profile_by_name(&cfg.wallet_expiry_profile).ok_or_else(|| {
            BridgeError::Config(format!(
                "unknown wallet_expiry_profile {:?}; known: {}",
                cfg.wallet_expiry_profile,
                expiry::PROFILES
                    .iter()
                    .map(|p| p.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        let (store, state) = StateStore::open(&cfg.state_file)?;
        Ok(Bridge {
            cfg,
            rpc,
            issuer_key,
            issuer_key_bytes: key_bytes,
            provider,
            store,
            state,
            node_id: None,
            crash: None,
            profile,
        })
    }

    /// Test hook: stop after the given state write.
    pub fn set_crash_point(&mut self, point: Option<CrashPoint>) {
        self.crash = point;
    }

    /// Test hook: another total time limit for each node call.
    pub fn set_rpc_timeout(&mut self, timeout: Duration) {
        self.rpc = self.rpc.clone().with_timeout(timeout);
    }

    /// The loaded state.
    pub fn state(&self) -> &StateFile {
        &self.state
    }

    /// A view of every purchase in the state.
    pub fn views(&self) -> Vec<PurchaseView> {
        (0..self.state.purchases.len())
            .map(|i| self.view(i, None))
            .collect()
    }

    fn view(&self, idx: usize, payment: Option<PaymentRequest>) -> PurchaseView {
        let p = &self.state.purchases[idx];
        let entitlement = p
            .grant
            .as_deref()
            .and_then(|g| hex_bytes(g).ok())
            .and_then(|g| codec::verify_grant(&g, &self.issuer_key).ok())
            .map(|g| g.entitlement);
        PurchaseView {
            request_id: p.request_id.clone(),
            purchase_id: p.purchase_id.clone(),
            service_kind: p.service_kind.clone(),
            offer_id: p.offer_id.clone(),
            stage: p.stage,
            last_status: p.last_status.clone(),
            payment,
            entitlement,
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

    /// Calls a node method with typed parameters and a typed result.
    fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, RpcError> {
        let params = serde_json::to_value(params)
            .map_err(|e| RpcError::Protocol(format!("cannot encode {method} parameters: {e}")))?;
        let value = self.rpc.call(method, params)?;
        serde_json::from_value(value)
            .map_err(|e| RpcError::Protocol(format!("malformed {method} answer: {e}")))
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
        let id = hex_array(hex)
            .map_err(|e| BridgeError::Node(format!("getminermeshinfo.node_id: {e}")))?;
        self.node_id = Some(id);
        Ok(id)
    }

    fn expectations(&mut self) -> Result<Expectations, BridgeError> {
        Ok(Expectations {
            network: self.cfg.network,
            issuer_key: self.issuer_key_bytes,
            provider_node_id: self.provider,
            client_node_id: self.node_id()?,
        })
    }

    fn adapter(&self, idx: usize) -> Result<&'static dyn ServiceAdapter, BridgeError> {
        let kind = &self.state.purchases[idx].service_kind;
        adapter_for(kind)
            .ok_or_else(|| BridgeError::Rejected(format!("no adapter for service kind {kind}")))
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

    /// Re-verifies the stored invoice's signature. Before a payment request is shown,
    /// `recheck_invoice` also runs the receipt checks again.
    fn stored_invoice(&self, idx: usize) -> Result<Invoice, BridgeError> {
        let hex = self.state.purchases[idx]
            .invoice
            .as_deref()
            .ok_or_else(|| BridgeError::Rejected("purchase has no stored invoice".into()))?;
        let bytes = hex_bytes(hex).map_err(BridgeError::Rejected)?;
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

    fn keep_for_retry(&mut self, idx: usize, msg: String) -> Result<PurchaseView, BridgeError> {
        self.set_note(idx, Some(msg.clone()))?;
        Err(BridgeError::Retryable(msg))
    }

    /// Starts a purchase of an offer, or continues the open one for the same offer.
    pub fn buy(&mut self, service_kind: &str, offer_id: &str) -> Result<PurchaseView, BridgeError> {
        if !codec::is_service_kind(service_kind) {
            return Err(BridgeError::Usage(format!(
                "{service_kind:?} is not a versioned service kind such as {}",
                paid_services_relay::SERVICE_KIND
            )));
        }
        if !codec::is_offer_id(offer_id) {
            return Err(BridgeError::Usage(format!(
                "{offer_id:?} is not an offer id"
            )));
        }
        if adapter_for(service_kind).is_none() {
            return Err(BridgeError::Usage(format!(
                "this sidecar has no adapter for service kind {service_kind}"
            )));
        }
        if let Some(idx) = self.state.purchases.iter().position(|p| {
            p.service_kind == service_kind && p.offer_id == offer_id && p.stage.is_open()
        }) {
            return match self.state.purchases[idx].stage {
                Stage::Requested => self.request_invoice(idx),
                Stage::Invoiced => self.present(idx, true),
                _ => Ok(self.view_note(
                    idx,
                    "this offer already has a granted purchase; no new purchase was started".into(),
                )),
            };
        }
        let request_id = random_bytes::<16>()
            .map_err(|e| BridgeError::Config(format!("the OS random generator failed: {e}")))?;
        self.state.purchases.push(Purchase {
            request_id: hex::encode(request_id),
            service_kind: service_kind.to_string(),
            offer_id: offer_id.to_string(),
            stage: Stage::Requested,
            purchase_id: None,
            invoice: None,
            last_status: None,
            grant: None,
            note: None,
        });
        let idx = self.state.purchases.len() - 1;
        self.persist(Some(CrashPoint::AfterRequested))?;
        self.request_invoice(idx)
    }

    fn request_invoice(&mut self, idx: usize) -> Result<PurchaseView, BridgeError> {
        let exp = self.expectations()?;
        let adapter = self.adapter(idx)?;
        let params = {
            let p = &self.state.purchases[idx];
            RequestInvoiceParams {
                service_kind: p.service_kind.clone(),
                offer_id: p.offer_id.clone(),
                request_id: p.request_id.clone(),
            }
        };
        let answer: RequestInvoiceResult = match self.call(rpc::REQUEST_INVOICE, &params) {
            Ok(v) => v,
            Err(RpcError::Transport(e)) => {
                return Err(BridgeError::NodeUnreachable(format!(
                    "{} got no answer ({e}); the outcome is unknown. \
                     Run `resume` to retry with the same request_id",
                    rpc::REQUEST_INVOICE
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
                    contract::CODE_DISABLED => "the paid-services module is disabled on the node",
                    _ => "the node answered with an unrecognized error code",
                };
                return self.keep_for_retry(
                    idx,
                    format!(
                        "{what} (code {code}): {message}. The purchase and its request_id are kept; \
                         run `resume` or `buy` again to retry the same request"
                    ),
                );
            }
            // The provider may already hold an invoice for this request_id, so a garbled
            // answer is retried like a lost one rather than ending the purchase.
            Err(RpcError::Protocol(e)) => {
                return self.keep_for_retry(
                    idx,
                    format!(
                        "malformed invoice answer ({e}). The purchase and its request_id are kept; \
                         run `resume` or `buy` again to retry the same request"
                    ),
                )
            }
        };
        let parsed = hex_array::<16>(&answer.purchase_id)
            .and_then(|pid| hex_bytes(&answer.invoice).map(|inv| (pid, inv)));
        let (purchase_id, invoice_bytes) = match parsed {
            Ok(x) => x,
            Err(e) => {
                return self.keep_for_retry(
                    idx,
                    format!(
                        "malformed invoice answer ({e}). The purchase and its request_id are kept; \
                         run `resume` or `buy` again to retry the same request"
                    ),
                )
            }
        };
        let inv = match codec::verify_invoice(&invoice_bytes, &self.issuer_key) {
            Ok(inv) => inv,
            Err(e) => return self.reject(idx, format!("invoice rejected: {e}")),
        };
        if let Err(e) = checks::check_invoice(
            &inv,
            &exp,
            &params.service_kind,
            &params.offer_id,
            &purchase_id,
            self.cfg.purchase_limit_zat,
        ) {
            return self.reject(idx, format!("invoice rejected: {e}"));
        }
        if let Err(e) = adapter.check_invoice(&inv) {
            return self.reject(idx, format!("invoice rejected by the service adapter: {e}"));
        }
        {
            let p = &mut self.state.purchases[idx];
            p.purchase_id = Some(hex::encode(purchase_id));
            p.invoice = Some(hex::encode(&invoice_bytes));
            p.stage = Stage::Invoiced;
            p.note = None;
        }
        self.persist(Some(CrashPoint::AfterInvoiced))?;
        self.present(idx, false)
    }

    /// Runs the receipt checks again on a stored invoice before a payment request is shown:
    /// the current network, issuer, provider and client NodeId, the purchase's service kind,
    /// offer and purchase_id, the current purchase limit and the service adapter. A failure
    /// only withholds the request. The purchase is not changed, so a grant for it can still
    /// be fetched and redeemed.
    fn recheck_invoice(&mut self, idx: usize, inv: &Invoice) -> Result<(), String> {
        let (service_kind, offer_id, purchase_id) = {
            let p = &self.state.purchases[idx];
            let pid = p
                .purchase_id
                .as_deref()
                .ok_or_else(|| "the purchase has no purchase_id".to_string())?;
            (
                p.service_kind.clone(),
                p.offer_id.clone(),
                hex_array::<16>(pid)?,
            )
        };
        let exp = self.expectations().map_err(|e| e.to_string())?;
        let adapter = self.adapter(idx).map_err(|e| e.to_string())?;
        checks::check_invoice(
            inv,
            &exp,
            &service_kind,
            &offer_id,
            &purchase_id,
            self.cfg.purchase_limit_zat,
        )
        .map_err(|e| e.to_string())?;
        adapter.check_invoice(inv)
    }

    /// Offers the payment request only while the stored invoice still passes the receipt
    /// checks, the provider still reports the purchase as pending and the offer window is
    /// open. Otherwise it is withheld.
    fn present(&mut self, idx: usize, reshown: bool) -> Result<PurchaseView, BridgeError> {
        let inv = self.stored_invoice(idx)?;
        if let Err(reason) = self.recheck_invoice(idx, &inv) {
            return Ok(self.withheld(
                idx,
                format!(
                    "the stored invoice was not confirmed against the current configuration \
                     ({reason}); no payment request shown"
                ),
            ));
        }
        let pid = self.state.purchases[idx]
            .purchase_id
            .clone()
            .ok_or_else(|| BridgeError::Rejected("invoiced purchase has no purchase_id".into()))?;
        let answer: GetGrantResult = match self.call(
            rpc::GET_GRANT,
            &GetGrantParams { purchase_id: pid },
        ) {
            Ok(v) => v,
            Err(e) => {
                return Ok(self.withheld(
                    idx,
                    format!("could not confirm that the purchase is still pending ({e}); no payment request shown"),
                ))
            }
        };
        let status = status_name(answer.status);
        if self.state.purchases[idx].last_status.as_deref() != Some(status.as_str()) {
            self.state.purchases[idx].last_status = Some(status.clone());
            self.persist(None)?;
        }
        match answer.status {
            PurchaseStatus::Pending => {}
            PurchaseStatus::PaymentSeen => {
                return Ok(self.withheld(
                    idx,
                    "the provider has seen a payment that is still short of its confirmations; \
                     no payment request shown. Run `status` to follow it",
                ))
            }
            _ => {
                return Ok(self.withheld(
                    idx,
                    format!("purchase status is {status}; no payment request shown. Run `status` to follow it"),
                ))
            }
        }
        let tip = self
            .rpc
            .call("getblockcount", json!([]))
            .ok()
            .and_then(|v| v.as_u64())
            .and_then(|t| u32::try_from(t).ok());
        let payment = match self.offer_window(&inv, tip) {
            Err(reason) => PaymentRequest::Withheld(reason),
            Ok(expected_expiry) => {
                let request = uri::build_payment_uri(&inv);
                uri::verify_payment_uri(&request, &inv, self.cfg.network)
                    .map_err(BridgeError::Rejected)?;
                PaymentRequest::Offered {
                    uri: request,
                    amount_zat: inv.amount_zat,
                    deadline_height: inv.deadline_height,
                    expected_expiry,
                    reshown,
                }
            }
        };
        Ok(self.view(idx, Some(payment)))
    }

    /// The offer window: a request is offered only while a wallet following the configured
    /// profile would set an expiry that, with the margin, stays at or below the deadline.
    /// Returns that expected expiry. This narrows when the request is shown; it does not stop
    /// a user from paying later, so a late payment can still miss the deadline rule.
    fn offer_window(&self, inv: &Invoice, tip: Option<u32>) -> Result<u32, String> {
        let tip = tip.ok_or("could not read the node's chain height; no payment request shown")?;
        let expiry = self
            .profile
            .expiry_for_tip(self.cfg.network, tip)
            .ok_or_else(|| {
                format!(
                    "the expiry profile {} has no upgrade schedule for this network; no payment request shown",
                    self.profile.name
                )
            })?;
        let latest = expiry.saturating_add(self.cfg.offer_margin_blocks);
        if latest > inv.deadline_height {
            return Err(format!(
                "offer window closed: at tip {tip} a wallet following {} would set nExpiryHeight {expiry}, \
                 and with the margin of {} blocks that passes the deadline {}; no payment request shown",
                self.profile.name, self.cfg.offer_margin_blocks, inv.deadline_height
            ));
        }
        Ok(expiry)
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
            Stage::Granted => self.redeem(idx),
            Stage::Invoiced | Stage::Redeemed => self.poll_grant(idx),
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
        let answer: GetGrantResult = self
            .call(
                rpc::GET_GRANT,
                &GetGrantParams {
                    purchase_id: hex::encode(inv.purchase_id),
                },
            )
            .map_err(|e| node_error(rpc::GET_GRANT, e))?;
        self.state.purchases[idx].last_status = Some(status_name(answer.status));

        match (answer.status, answer.grant) {
            (PurchaseStatus::Issued | PurchaseStatus::Expired, Some(g)) => {
                if let Err(reason) = self.accept_grant(idx, &g, &inv, &exp) {
                    self.set_note(
                        idx,
                        Some(format!("grant rejected: {reason}; nothing was redeemed")),
                    )?;
                    return Ok(self.view(idx, None));
                }
                if answer.status == PurchaseStatus::Expired {
                    self.state.purchases[idx].stage = Stage::Expired;
                    self.set_note(idx, None)?;
                    return Ok(self.view(idx, None));
                }
                if self.state.purchases[idx].stage == Stage::Redeemed {
                    self.persist(None)?;
                    return Ok(self.view(idx, None));
                }
                self.state.purchases[idx].stage = Stage::Granted;
                self.state.purchases[idx].note = None;
                self.persist(Some(CrashPoint::AfterGranted))?;
                self.redeem(idx)
            }
            (PurchaseStatus::Issued, None) => {
                self.set_note(
                    idx,
                    Some("the answer said Issued but carried no grant; ignored. Access is only taken from a verified grant".into()),
                )?;
                Ok(self.view(idx, None))
            }
            (PurchaseStatus::Expired, None) => {
                self.state.purchases[idx].stage = Stage::Expired;
                self.set_note(idx, None)?;
                Ok(self.view(idx, None))
            }
            (PurchaseStatus::Unknown, _) => {
                self.set_note(idx, Some("unknown provider status; ignored".into()))?;
                Ok(self.view(idx, None))
            }
            _ => {
                self.persist(None)?;
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
        let adapter = self.adapter(idx).map_err(|e| e.to_string())?;
        let bytes = hex_bytes(grant_hex)?;
        let grant = codec::verify_grant(&bytes, &self.issuer_key).map_err(|e| e.to_string())?;
        checks::check_grant(&grant, inv, exp).map_err(|e| e.to_string())?;
        adapter.check_grant(&grant)?;
        let encoded = hex::encode(&bytes);
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

    /// Redeems the stored grant the way its service's adapter says.
    fn redeem(&mut self, idx: usize) -> Result<PurchaseView, BridgeError> {
        let grant_hex = self.state.purchases[idx]
            .grant
            .clone()
            .ok_or_else(|| BridgeError::Rejected("purchase has no stored grant".into()))?;
        let bytes = hex_bytes(&grant_hex).map_err(BridgeError::Rejected)?;
        let grant = codec::verify_grant(&bytes, &self.issuer_key)
            .map_err(|e| BridgeError::Rejected(format!("stored grant no longer verifies: {e}")))?;
        match self.adapter(idx)?.redemption() {
            Redemption::InstallOnNode => {}
        }
        let answer: Result<InstallGrantResult, RpcError> =
            self.call(rpc::INSTALL_GRANT, &InstallGrantParams { grant: grant_hex });
        match answer {
            Ok(r) => {
                if !r.installed || r.grant_id != hex::encode(grant.grant_id) {
                    self.set_note(
                        idx,
                        Some("the node's install answer does not match the grant; not marked redeemed".into()),
                    )?;
                    return Ok(self.view(idx, None));
                }
                let p = &mut self.state.purchases[idx];
                if p.stage != Stage::Expired {
                    p.stage = Stage::Redeemed;
                }
                p.note = None;
                self.persist(Some(CrashPoint::AfterRedeemed))?;
                Ok(self.view(idx, None))
            }
            Err(RpcError::Transport(e)) => {
                self.set_note(
                    idx,
                    Some(format!(
                        "{} got no answer ({e}); run `resume` to install the same grant again",
                        rpc::INSTALL_GRANT
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
                Stage::Granted => self.redeem(idx),
                Stage::Redeemed => self.redeem(idx).and_then(|_| self.refresh(idx)),
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
            Stage::Granted | Stage::Redeemed => {
                Ok(self.withheld(idx, "a grant was already issued for this purchase"))
            }
            Stage::Expired => Ok(self.withheld(idx, "the purchase expired")),
        }
    }
}
