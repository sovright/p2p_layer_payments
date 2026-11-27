//! The proposed node RPC methods and their request and response bodies.
//!
//! These are proposals for this prototype, not Zakura RPCs. Each method is documented in the
//! style of zakura-rpc's `RpcServer` trait (`crates/zakura-rpc/src/methods.rs`), so that adding
//! it to Zakura is mostly a trait entry and a handler. All three would be Admin methods,
//! served only on the loopback, cookie-authenticated listener, and off unless enabled in the
//! node's config. The node never takes the client NodeId as a parameter: it is the node's own
//! TLS identity on its P2P session with the provider.

use serde::{Deserialize, Serialize};

/// Requests an invoice for an offer from the configured provider.
///
/// method: post
/// tags: paidsvc
///
/// # Notes
///
/// The node sends the request over its authenticated P2P session with the provider and
/// returns the issuer-signed invoice unchanged. The node keeps no purchase state; the caller
/// keeps the `request_id`. The same `request_id` gets the same answer bytes, and the same
/// `request_id` with other terms is a final rejection (code 3). Any other error, or no
/// answer, leaves the outcome unknown, and the caller retries with the same `request_id`.
pub const REQUEST_INVOICE: &str = "paidsvc_requestinvoice";

/// Asks the provider for a purchase's status and, once issued, its grant.
///
/// method: post
/// tags: paidsvc
///
/// # Notes
///
/// Only the client the purchase is bound to can read it; an unknown purchase and one bound to
/// another client both get `NotFound`. Once a grant is issued, every answer carries the same
/// signed bytes.
pub const GET_GRANT: &str = "paidsvc_getgrant";

/// Installs a verified grant on the node, which presents it to the provider. This is how
/// services redeemed on the user's node, such as the relay stream, redeem a grant.
///
/// method: post
/// tags: paidsvc
///
/// # Notes
///
/// The node checks the grant against its configured issuer key, network and provider and its
/// own NodeId. Installing the same bytes again changes nothing; a different grant for the
/// same purchase is refused (code 4).
pub const INSTALL_GRANT: &str = "paidsvc_installgrant";

/// Parameters of [`REQUEST_INVOICE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestInvoiceParams {
    /// The versioned service kind.
    pub service_kind: String,
    /// The offer to buy.
    pub offer_id: String,
    /// The caller's request id (hex, 16 bytes), kept across retries.
    pub request_id: String,
}

/// Result of [`REQUEST_INVOICE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestInvoiceResult {
    /// The provider's purchase id (hex, 16 bytes).
    pub purchase_id: String,
    /// The issuer-signed invoice (hex).
    pub invoice: String,
}

/// Parameters of [`GET_GRANT`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetGrantParams {
    /// The purchase id (hex, 16 bytes).
    pub purchase_id: String,
}

/// A purchase's status, as the provider reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PurchaseStatus {
    /// Unknown, or bound to another client.
    NotFound,
    /// No payment seen.
    Pending,
    /// A payment still short of its confirmations.
    PaymentSeen,
    /// A payment below the invoice amount.
    Underpaid,
    /// A payment that breaks the deadline rule.
    NeedsReconciliation,
    /// A grant was issued.
    Issued,
    /// The purchase or its grant expired.
    Expired,
    /// A status this version does not know.
    #[serde(other)]
    Unknown,
}

/// Result of [`GET_GRANT`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetGrantResult {
    /// The purchase's status.
    pub status: PurchaseStatus,
    /// The issuer-signed grant (hex), once issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
}

/// Parameters of [`INSTALL_GRANT`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallGrantParams {
    /// The issuer-signed grant (hex).
    pub grant: String,
}

/// Result of [`INSTALL_GRANT`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallGrantResult {
    /// Whether the node holds the grant now.
    pub installed: bool,
    /// The installed grant's id (hex, 16 bytes).
    pub grant_id: String,
}
