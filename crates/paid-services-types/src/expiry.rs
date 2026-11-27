//! What `nExpiryHeight` a user's own wallet sets, as an explicit profile.
//!
//! A wallet paying from a ZIP 321 request chooses its own expiry height; the request cannot
//! set it. The bridge uses a profile of that behaviour to decide when to stop offering a
//! request. This is a heuristic: a user can still pay later than the request was shown, and a
//! wallet may compute the expiry differently. It does not guarantee the deadline rule.
//!
//! The one profile here was read from source, not tested against a running wallet:
//! - zecd 0.10.0 (`3a8a2ef8`): target height + 40, or + 120 from NU7 (test at
//!   `src/wallet/actor.rs:9573-9594`).
//! - Vizor v0.0.66 (`c4d01dc1`): live tip + 1 + 40, or + 120 from NU7, recomputed when the
//!   send runs and capped at the end of the consensus branch
//!   (`rust/src/wallet/sync/send.rs:156-165`, `:208-239`).
//! - Zodl, through librustzcash `9a3ffd33`: target height + 40, or + 120 from NU7
//!   (`zcash_primitives/src/transaction/builder.rs`; iOS SDK `rust/src/lib.rs:3523-3533`).
//!
//! Not modelled: the cap at the end of a consensus branch before a future upgrade.

use crate::codec::Network;

/// NU7 activation heights, from Zakura's consensus parameters.
pub fn nu7_activation_height(network: Network) -> Option<u32> {
    match network {
        Network::Main => Some(3_543_405),
        Network::Test => Some(4_465_026),
        // Regtest activation heights are set per chain; the profile does not guess them.
        Network::Regtest => None,
    }
}

/// How a wallet picks `nExpiryHeight` for a transaction it builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalletExpiryProfile {
    /// The profile's configuration name.
    pub name: &'static str,
    /// Blocks added to the target height before NU7.
    pub delta_before_nu7: u32,
    /// Blocks added to the target height from NU7 on.
    pub delta_from_nu7: u32,
}

/// The librustzcash default that zecd, Vizor and Zodl follow (see the module docs).
pub const LIBRUSTZCASH_DEFAULT: WalletExpiryProfile = WalletExpiryProfile {
    name: "librustzcash-default",
    delta_before_nu7: 40,
    delta_from_nu7: 120,
};

/// Every known profile.
pub const PROFILES: &[WalletExpiryProfile] = &[LIBRUSTZCASH_DEFAULT];

/// Finds a profile by its configuration name.
pub fn profile_by_name(name: &str) -> Option<WalletExpiryProfile> {
    PROFILES.iter().copied().find(|p| p.name == name)
}

impl WalletExpiryProfile {
    /// The `nExpiryHeight` a wallet with this profile sets when it builds a transaction while
    /// the chain tip is `tip` (target height `tip + 1`). `None` if the upgrade schedule of the
    /// network is unknown or the height overflows.
    pub fn expiry_for_tip(&self, network: Network, tip: u32) -> Option<u32> {
        let target = tip.checked_add(1)?;
        let delta = if target >= nu7_activation_height(network)? {
            self.delta_from_nu7
        } else {
            self.delta_before_nu7
        };
        target.checked_add(delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_follow_the_nu7_boundary() {
        let p = LIBRUSTZCASH_DEFAULT;
        // Target height is tip + 1.
        assert_eq!(
            p.expiry_for_tip(Network::Test, 4_465_024),
            Some(4_465_025 + 40)
        );
        assert_eq!(
            p.expiry_for_tip(Network::Test, 4_465_025),
            Some(4_465_026 + 120)
        );
        assert_eq!(
            p.expiry_for_tip(Network::Main, 3_543_404),
            Some(3_543_405 + 120)
        );
        assert_eq!(p.expiry_for_tip(Network::Regtest, 10), None);
        assert_eq!(p.expiry_for_tip(Network::Test, u32::MAX), None);
        assert_eq!(profile_by_name("librustzcash-default"), Some(p));
        assert_eq!(profile_by_name("unknown"), None);
    }
}
