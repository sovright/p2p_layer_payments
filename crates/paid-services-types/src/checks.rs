//! What the bridge checks before it shows a payment request or installs a grant.

use crate::codec::{Entitlement, Grant, Invoice, Network, Terms};
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

/// Runs on an invoice whose signature already verified against the pinned key. The service
/// adapter checks the service-specific parts separately.
pub fn check_invoice(
    inv: &Invoice,
    exp: &Expectations,
    service_kind: &str,
    offer_id: &str,
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
    if inv.service_kind != service_kind || inv.offer_id != offer_id {
        return Err(CheckError::Service {
            expected: format!("{service_kind} {offer_id}"),
            got: format!("{} {}", inv.service_kind, inv.offer_id),
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
    if inv.confirmations == 0 {
        return Err(CheckError::Terms("confirmation count is zero"));
    }
    match inv.terms {
        Terms::HeightWindow { blocks: 0 } => {
            return Err(CheckError::Terms("access duration is zero"))
        }
        Terms::Quota { units: 0 } => return Err(CheckError::Terms("quota is zero")),
        _ => {}
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
    if grant.service_kind != inv.service_kind {
        return Err(CheckError::GrantMismatch("service_kind"));
    }
    if grant.offer_id != inv.offer_id {
        return Err(CheckError::GrantMismatch("offer_id"));
    }
    if grant.purchase_id != inv.purchase_id {
        return Err(CheckError::GrantMismatch("purchase_id"));
    }
    if grant.service_params != inv.service_params {
        return Err(CheckError::GrantMismatch("service_params"));
    }
    match (inv.terms, grant.entitlement) {
        (
            Terms::HeightWindow { blocks },
            Entitlement::HeightWindow {
                start_height,
                end_height,
            },
        ) => {
            if end_height.checked_sub(start_height) != Some(blocks) {
                return Err(CheckError::GrantMismatch(
                    "the height window differs from the quoted access",
                ));
            }
        }
        (Terms::Quota { units }, Entitlement::Quota { units: granted }) => {
            if units != granted {
                return Err(CheckError::GrantMismatch(
                    "the quota differs from the quoted one",
                ));
            }
        }
        (Terms::OneShot, Entitlement::OneShot) => {}
        _ => {
            return Err(CheckError::GrantMismatch(
                "the entitlement does not match the terms",
            ))
        }
    }
    Ok(())
}
