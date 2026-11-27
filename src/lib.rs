//! Prototype bridge between a user's own wallet and a Zakura node buying a paid stream.
//!
//! **Simulation only.** The bridge talks to a mock node and a mock issuer. It opens no wallet,
//! starts no payment, holds no keys and talks to no real Zakura node. The `paidstream_*`
//! methods it calls are proposed RPC contracts, not existing Zakura RPCs.

pub mod bridge;
pub mod checks;
pub mod codec;
pub mod contract;
pub mod expiry;
pub mod hexutil;
pub mod mock;
pub mod rpc;
pub mod state;
pub mod uri;
