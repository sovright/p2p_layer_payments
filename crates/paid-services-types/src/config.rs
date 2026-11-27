//! The sidecar's configuration file format. Reading the file is the sidecar's job.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::codec::Network;

/// The sidecar's JSON configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarConfig {
    /// The user's node RPC (in this prototype, the mock node). Must be loopback.
    pub node_rpc: SocketAddr,
    /// The node's RPC cookie file.
    pub cookie_file: PathBuf,
    /// The sidecar's durable state file.
    pub state_file: PathBuf,
    /// The network every signed object must name.
    pub network: Network,
    /// The provider's NodeId (hex), pinned together with the issuer key.
    pub provider_node_id: String,
    /// The issuer's Ed25519 public key (hex).
    pub issuer_public_key: String,
    /// The most a single purchase may cost, in zatoshis.
    pub purchase_limit_zat: u64,
    /// The wallet expiry profile used for the offer window (see [`crate::expiry`]).
    pub wallet_expiry_profile: String,
    /// Blocks kept free between the expected wallet expiry and the deadline. A configured
    /// value, not a recommendation.
    pub offer_margin_blocks: u32,
}
