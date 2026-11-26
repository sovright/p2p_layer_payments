//! What the bridge checks before it shows a payment request or installs a grant.

use std::fmt;

use crate::codec::{Grant, Invoice, Network};
use crate::uri;

/// Values the bridge trusts: its own configuration and the node's own NodeId.
#[derive(Clone, Debug)]
pub struct Expectations {
    pub network: Network,
    pub issuer_key: [u8; 32],
    pub provider_node_id: [u8; 32],
    pub client_node_id: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckError {
    Network,
    IssuerKey,
    Provider,
    Client,
    Service { expected: String, got: String },
    PurchaseId,
    AmountZero,
    OverLimit { amount_zat: u64, limit_zat: u64 },
    Terms(&'static str),
    Destination(String),
    PaymentRequest(String),
    GrantMismatch(&'static str),
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckError::Network => write!(f, "object is for another network"),
            CheckError::IssuerKey => write!(f, "object names another issuer key"),
            CheckError::Provider => write!(f, "object names another provider NodeId"),
            CheckError::Client => {
                write!(
                    f,
                    "object is bound to another client NodeId than this node's"
                )
            }
            CheckError::Service { expected, got } => {
                write!(f, "object is for service {got:?}, expected {expected:?}")
            }
            CheckError::PurchaseId => write!(f, "purchase_id differs from the signed invoice"),
            CheckError::AmountZero => write!(f, "invoice amount is zero"),
            CheckError::OverLimit {
                amount_zat,
                limit_zat,
            } => write!(
                f,
                "invoice amount {amount_zat} zat is above the purchase limit {limit_zat} zat"
            ),
            CheckError::Terms(what) => write!(f, "invoice terms are invalid: {what}"),
            CheckError::Destination(e) => write!(f, "invoice destination is invalid: {e}"),
            CheckError::PaymentRequest(e) => write!(f, "payment request check failed: {e}"),
            CheckError::GrantMismatch(what) => {
                write!(f, "grant does not match the invoice: {what}")
            }
        }
    }
}

impl std::error::Error for CheckError {}

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
