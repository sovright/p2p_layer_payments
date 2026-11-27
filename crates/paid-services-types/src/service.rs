//! How a service kind plugs into the sidecar: a small adapter trait.
//!
//! The purchase lifecycle, the signed objects and the payment request are the same for every
//! service. An adapter adds only what is specific to its service kind: which terms it sells,
//! what its `service_params` mean, and how a grant is redeemed. Adapters do no I/O.

use crate::codec::{Grant, Invoice};

/// How a verified grant is redeemed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Redemption {
    /// The sidecar installs the grant on the user's own node (the proposed
    /// `paidsvc_installgrant` method), which presents it to the provider.
    InstallOnNode,
}

/// The service-specific part of a purchase.
pub trait ServiceAdapter {
    /// The versioned service kind this adapter handles.
    fn service_kind(&self) -> &'static str;

    /// Checks the service-specific parts of a verified invoice: the terms it allows and its
    /// `service_params`.
    fn check_invoice(&self, invoice: &Invoice) -> Result<(), String>;

    /// Checks the service-specific parts of a verified grant.
    fn check_grant(&self, grant: &Grant) -> Result<(), String>;

    /// How a verified grant is redeemed.
    fn redemption(&self) -> Redemption;
}
