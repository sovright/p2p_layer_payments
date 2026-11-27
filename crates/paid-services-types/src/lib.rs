//! Sync-only types for paid services over Zakura P2P.
//!
//! This crate holds the issuer-signed invoice and grant format, the checks a buyer runs on
//! them, the contract error codes, the ZIP 321 payment request code, the wallet expiry
//! profile and the sidecar configuration. It does no I/O, so it can sit low in Zakura's
//! dependency graph.
//!
//! **Prototype.** The byte format, the contract and every RPC name here are proposals for a
//! simulation, not Zakura wire formats or Zakura RPCs.

pub mod checks;
pub mod codec;
pub mod config;
pub mod contract;
pub mod expiry;
pub mod rpc;
pub mod service;
pub mod uri;

/// Decodes a hex string.
pub fn hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    hex::decode(s).map_err(|e| e.to_string())
}

/// Decodes a hex string into a fixed-size array.
pub fn hex_array<const N: usize>(s: &str) -> Result<[u8; N], String> {
    let bytes = hex::decode(s).map_err(|e| e.to_string())?;
    let len = bytes.len();
    <[u8; N]>::try_from(bytes).map_err(|_| format!("expected {N} bytes, got {len}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_arrays() {
        assert_eq!(hex_array::<2>("00ff"), Ok([0x00, 0xff]));
        assert_eq!(hex_array::<2>("00FF"), Ok([0x00, 0xff]));
        assert!(hex_array::<3>("00ff").is_err());
        assert!(hex_array::<1>("zz").is_err());
        assert!(hex_array::<1>("abc").is_err());
    }
}
