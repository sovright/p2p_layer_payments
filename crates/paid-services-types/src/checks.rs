//! What the bridge checks before it shows a payment request or installs a grant.

use crate::codec::{Grant, Invoice, Network};
use crate::uri;

/// Values the bridge trusts: its own configuration and the node's own NodeId.
#[derive(Clone, Debug)]
pub struct Expectations {
    /// The configured network.
    pub network: Network,
    /// The pinned issuer key.
    pub issuer_key: [u8; 32],
    /// The pinned provider NodeId.
    pub provider_node_id: [u8; 32],
    /// The node's own NodeId.
    pub client_node_id: [u8; 32],
}

/// Why an invoice or grant failed a check.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CheckError {
    /// The object names another network.
    #[error("object is for another network")]
    Network,
    /// The object names another issuer key.
    #[error("object names another issuer key")]
    IssuerKey,
    /// The object names another provider.
    #[error("object names another provider NodeId")]
    Provider,
    /// The object is bound to another client.
    #[error("object is bound to another client NodeId than this node's")]
    Client,
    /// The object is for another service.
    #[error("object is for service {got:?}, expected {expected:?}")]
    Service {
        /// The service asked for.
        expected: String,
        /// The service in the object.
        got: String,
    },
    /// The purchase id differs from the answer's.
    #[error("purchase_id differs from the signed invoice")]
    PurchaseId,
    /// The invoice costs nothing.
    #[error("invoice amount is zero")]
    AmountZero,
    /// The invoice costs more than the purchase limit.
    #[error("invoice amount {amount_zat} zat is above the purchase limit {limit_zat} zat")]
    OverLimit {
        /// The invoice amount.
        amount_zat: u64,
        /// The configured limit.
        limit_zat: u64,
    },
    /// The terms are invalid.
    #[error("invoice terms are invalid: {0}")]
    Terms(&'static str),
    /// The destination is invalid.
    #[error("invoice destination is invalid: {0}")]
    Destination(String),
    /// The payment request built from the invoice does not read back.
    #[error("payment request check failed: {0}")]
    PaymentRequest(String),
    /// The grant does not match its invoice.
    #[error("grant does not match the invoice: {0}")]
    GrantMismatch(&'static str),
}

fn check_parties(
    network: Network,
    issuer_key: &[u8; 32],
    provider: &[u8; 32],
    client: &[u8; 32],
    exp: &Expectations,
) -> Result<(), CheckError> {
    if network != exp.network {
        return Err(CheckError::Network);
    }
    if issuer_key != &exp.issuer_key {
        return Err(CheckError::IssuerKey);
    }
    if provider != &exp.provider_node_id {
        return Err(CheckError::Provider);
    }
    if client != &exp.client_node_id {
        return Err(CheckError::Client);
    }
    Ok(())
}

/// Runs on an invoice whose signature already verified against the pinned key.
pub fn check_invoice(
    inv: &Invoice,
    exp: &Expectations,
    service: &str,
    purchase_id: &[u8; 16],
    limit_zat: u64,
) -> Result<(), CheckError> {
    check_parties(
        inv.network,
        &inv.issuer_key,
        &inv.provider_node_id,
        &inv.client_node_id,
        exp,
    )?;
    if inv.service != service {
        return Err(CheckError::Service {
            expected: service.to_string(),
            got: inv.service.clone(),
        });
    }
    if &inv.purchase_id != purchase_id {
        return Err(CheckError::PurchaseId);
    }
    if inv.amount_zat == 0 {
        return Err(CheckError::AmountZero);
    }
    if inv.amount_zat > limit_zat {
        return Err(CheckError::OverLimit {
            amount_zat: inv.amount_zat,
            limit_zat,
        });
    }
    if inv.deadline_height == 0 {
        return Err(CheckError::Terms("deadline height is zero"));
    }
    if inv.access_blocks == 0 {
        return Err(CheckError::Terms("access duration is zero"));
    }
    if inv.confirmations == 0 {
        return Err(CheckError::Terms("confirmation count is zero"));
    }
    uri::check_destination(&inv.destination, exp.network).map_err(CheckError::Destination)?;
    let request = uri::build_payment_uri(inv);
    uri::verify_payment_uri(&request, inv, exp.network).map_err(CheckError::PaymentRequest)?;
    Ok(())
}

/// Runs on a grant whose signature already verified against the pinned key.
pub fn check_grant(grant: &Grant, inv: &Invoice, exp: &Expectations) -> Result<(), CheckError> {
    check_parties(
        grant.network,
        &grant.issuer_key,
        &grant.provider_node_id,
        &grant.client_node_id,
        exp,
    )?;
    if grant.service != inv.service {
        return Err(CheckError::GrantMismatch("service"));
    }
    if grant.purchase_id != inv.purchase_id {
        return Err(CheckError::GrantMismatch("purchase_id"));
    }
    if grant.end_height <= grant.start_height {
        return Err(CheckError::GrantMismatch(
            "end height is not above start height",
        ));
    }
    if grant.end_height - grant.start_height != inv.access_blocks {
        return Err(CheckError::GrantMismatch(
            "duration differs from the quoted access",
        ));
    }
    if grant.session_allowance == 0 {
        return Err(CheckError::GrantMismatch("session allowance is zero"));
    }
    Ok(())
}
