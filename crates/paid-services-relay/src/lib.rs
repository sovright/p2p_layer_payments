//! The relay block stream as a paid service.
//!
//! This is the first, and so far only, service adapter. It sells a height window of access to
//! a provider's relay stream. Its `service_params` carry the number of concurrent sessions a
//! grant allows, and a grant is redeemed by installing it on the user's own node, which
//! presents it to the provider. Nothing relay-specific lives in the sidecar or the types crate.
//!
//! **Prototype.** The service kind, the parameter format and the redemption path are
//! proposals; no Zakura node implements them yet.

use paid_services_types::codec::{Entitlement, Grant, Invoice, Terms};
use paid_services_types::service::{Redemption, ServiceAdapter};

/// The relay stream's versioned service kind.
pub const SERVICE_KIND: &str = "zakura.relay.blockstream/1";

const PARAMS_VERSION: u8 = 1;
const PARAMS_LEN: usize = 5;

/// The relay stream's service parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayParams {
    /// Concurrent sessions a grant allows; at least 1.
    pub session_allowance: u32,
}

impl RelayParams {
    /// Encodes the parameters: a version byte and the allowance (big-endian).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(PARAMS_LEN);
        out.push(PARAMS_VERSION);
        out.extend_from_slice(&self.session_allowance.to_be_bytes());
        out
    }

    /// Decodes and validates the parameters.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let [version, a, b, c, d] = <[u8; PARAMS_LEN]>::try_from(bytes)
            .map_err(|_| format!("relay params are {} bytes, expected 5", bytes.len()))?;
        if version != PARAMS_VERSION {
            return Err(format!("unknown relay params version {version}"));
        }
        let session_allowance = u32::from_be_bytes([a, b, c, d]);
        if session_allowance == 0 {
            return Err("relay session allowance is zero".into());
        }
        Ok(RelayParams { session_allowance })
    }
}

/// The relay stream's sidecar adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct RelayAdapter;

impl ServiceAdapter for RelayAdapter {
    fn service_kind(&self) -> &'static str {
        SERVICE_KIND
    }

    fn check_invoice(&self, invoice: &Invoice) -> Result<(), String> {
        if !matches!(invoice.terms, Terms::HeightWindow { .. }) {
            return Err("the relay stream sells only height windows".into());
        }
        RelayParams::decode(&invoice.service_params).map(|_| ())
    }

    fn check_grant(&self, grant: &Grant) -> Result<(), String> {
        if !matches!(grant.entitlement, Entitlement::HeightWindow { .. }) {
            return Err("a relay grant must carry a height window".into());
        }
        RelayParams::decode(&grant.service_params).map(|_| ())
    }

    fn redemption(&self) -> Redemption {
        Redemption::InstallOnNode
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paid_services_types::codec::Network;

    fn invoice(terms: Terms, service_params: Vec<u8>) -> Invoice {
        Invoice {
            network: Network::Test,
            issuer_key: [0; 32],
            provider_node_id: [1; 32],
            client_node_id: [2; 32],
            service_kind: SERVICE_KIND.into(),
            offer_id: "relay-10".into(),
            purchase_id: [3; 16],
            amount_zat: 1,
            destination: String::new(),
            memo: Vec::new(),
            deadline_height: 1,
            confirmations: 1,
            terms,
            service_params,
        }
    }

    #[test]
    fn params_round_trip_and_reject_bad_input() {
        let p = RelayParams {
            session_allowance: 2,
        };
        assert_eq!(RelayParams::decode(&p.encode()), Ok(p));
        assert!(RelayParams::decode(&[1, 0, 0, 0, 0]).is_err());
        assert!(RelayParams::decode(&[2, 0, 0, 0, 1]).is_err());
        assert!(RelayParams::decode(&[1, 0, 0, 1]).is_err());
    }

    #[test]
    fn adapter_accepts_only_height_windows_with_valid_params() {
        let a = RelayAdapter;
        let params = RelayParams {
            session_allowance: 1,
        }
        .encode();
        assert!(a
            .check_invoice(&invoice(Terms::HeightWindow { blocks: 10 }, params.clone()))
            .is_ok());
        assert!(a
            .check_invoice(&invoice(Terms::Quota { units: 10 }, params))
            .is_err());
        assert!(a
            .check_invoice(&invoice(Terms::HeightWindow { blocks: 10 }, vec![9]))
            .is_err());
        assert_eq!(a.redemption(), Redemption::InstallOnNode);
    }
}
