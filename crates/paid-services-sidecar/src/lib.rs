//! A simulation-only sidecar that buys paid services for a Zakura node.
//!
//! The sidecar holds no wallet spending keys and builds no transactions. It requests an
//! issuer-signed invoice through the user's node, shows a ZIP 321 payment request for the
//! user's own wallet, follows the purchase and installs the issuer-signed grant on the node.
//!
//! **Simulation only.** It talks to a mock node and a mock issuer. The `paidstream_*`
//! methods it calls are proposed RPC contracts, not existing Zakura RPCs.

pub mod bridge;
pub mod rpc;
pub mod state;
