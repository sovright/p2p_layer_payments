//! Issuer-signed invoices and grants.
//!
//! The byte format here is a prototype format, not a Zakura wire format. A signed object is
//! `body || signature`, where the Ed25519 signature covers `domain || 0x00 || body` and the
//! invoice and grant use different domains, so neither can be replayed as the other.
//! Signatures are checked with `ed25519-zebra`, which applies the ZIP 215 validation rules.

use ed25519_zebra::Signature;
pub use ed25519_zebra::{SigningKey, VerificationKey};
use serde::{Deserialize, Serialize};
use zcash_protocol::consensus::NetworkType;

/// Signature domain of invoices.
pub const INVOICE_DOMAIN: &[u8] = b"paid-stream-bridge/prototype/v1/invoice";
/// Signature domain of grants.
pub const GRANT_DOMAIN: &[u8] = b"paid-stream-bridge/prototype/v1/grant";

const FORMAT_VERSION: u8 = 1;
/// Length of the trailing Ed25519 signature.
pub const SIGNATURE_LEN: usize = 64;
/// Longest service name, in bytes.
pub const MAX_SERVICE_LEN: usize = 64;
/// Longest destination address, in bytes.
pub const MAX_DESTINATION_LEN: usize = 512;
/// Longest memo, in bytes.
pub const MAX_MEMO_LEN: usize = 512;

/// The Zcash network an object belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// Mainnet.
    Main,
    /// Testnet.
    Test,
    /// A regtest chain.
    Regtest,
}

impl Network {
    fn to_byte(self) -> u8 {
        match self {
            Network::Main => 0,
            Network::Test => 1,
            Network::Regtest => 2,
        }
    }

    fn from_byte(b: u8) -> Result<Self, CodecError> {
        match b {
            0 => Ok(Network::Main),
            1 => Ok(Network::Test),
            2 => Ok(Network::Regtest),
            other => Err(CodecError::BadNetwork(other)),
        }
    }

    /// The matching network type of the Zcash address crates.
    pub fn network_type(self) -> NetworkType {
        match self {
            Network::Main => NetworkType::Main,
            Network::Test => NetworkType::Test,
            Network::Regtest => NetworkType::Regtest,
        }
    }
}

/// What the issuer offers: one fixed-price, non-renewing purchase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invoice {
    /// Network of the purchase.
    pub network: Network,
    /// The issuer's public key. It only names the signer; trust comes from the pinned key.
    pub issuer_key: [u8; 32],
    /// The provider's NodeId.
    pub provider_node_id: [u8; 32],
    /// The buying node's NodeId.
    pub client_node_id: [u8; 32],
    /// The service being bought.
    pub service: String,
    /// The provider's purchase identifier.
    pub purchase_id: [u8; 16],
    /// Price in zatoshis.
    pub amount_zat: u64,
    /// Shielded address that receives the payment.
    pub destination: String,
    /// Memo to put on the payment.
    pub memo: Vec<u8>,
    /// Inclusive inclusion deadline on the provider's chain.
    pub deadline_height: u32,
    /// Access length in blocks.
    pub access_blocks: u32,
    /// Confirmations the issuer waits for before granting.
    pub confirmations: u32,
}

/// The issuer's access authorization for one purchase. It is not on-chain proof of payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// Network of the purchase.
    pub network: Network,
    /// The issuer's public key.
    pub issuer_key: [u8; 32],
    /// The provider's NodeId.
    pub provider_node_id: [u8; 32],
    /// The buying node's NodeId.
    pub client_node_id: [u8; 32],
    /// The service bought.
    pub service: String,
    /// The purchase this grant belongs to.
    pub purchase_id: [u8; 16],
    /// The grant's identifier.
    pub grant_id: [u8; 16],
    /// First height of access.
    pub start_height: u32,
    /// Exclusive: access ends when the provider's tip reaches this height.
    pub end_height: u32,
    /// Concurrent sessions the grant allows.
    pub session_allowance: u32,
}

/// Why a signed object was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    /// The object ends early.
    #[error("object is truncated")]
    Truncated,
    /// Bytes follow the last field.
    #[error("object has trailing bytes")]
    TrailingBytes,
    /// Unknown format version.
    #[error("unknown format version {0}")]
    BadVersion(u8),
    /// Unknown network byte.
    #[error("unknown network byte {0}")]
    BadNetwork(u8),
    /// A variable-length field is too long.
    #[error("field {0} is too long")]
    FieldTooLong(&'static str),
    /// A text field is not UTF-8.
    #[error("field {0} is not UTF-8")]
    BadUtf8(&'static str),
    /// The signature does not verify.
    #[error("signature does not verify against the pinned issuer key")]
    BadSignature,
    /// The bytes are not an Ed25519 public key.
    #[error("not a valid Ed25519 public key")]
    BadKey,
}

struct Writer(Vec<u8>);

impl Writer {
    fn new() -> Self {
        Writer(vec![FORMAT_VERSION])
    }
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn raw(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v);
    }
    fn var(&mut self, name: &'static str, v: &[u8], max: usize) -> Result<(), CodecError> {
        let len = u16::try_from(v.len()).map_err(|_| CodecError::FieldTooLong(name))?;
        if v.len() > max {
            return Err(CodecError::FieldTooLong(name));
        }
        self.0.extend_from_slice(&len.to_be_bytes());
        self.0.extend_from_slice(v);
        Ok(())
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Result<Self, CodecError> {
        let mut r = Reader { buf, pos: 0 };
        let version = r.u8()?;
        if version != FORMAT_VERSION {
            return Err(CodecError::BadVersion(version));
        }
        Ok(r)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        let end = self.pos.checked_add(n).ok_or(CodecError::Truncated)?;
        let out = self.buf.get(self.pos..end).ok_or(CodecError::Truncated)?;
        self.pos = end;
        Ok(out)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        <[u8; N]>::try_from(self.take(N)?).map_err(|_| CodecError::Truncated)
    }
    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.array::<1>()?[0])
    }
    fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    fn var(&mut self, name: &'static str, max: usize) -> Result<Vec<u8>, CodecError> {
        let len = usize::from(u16::from_be_bytes(self.array()?));
        if len > max {
            return Err(CodecError::FieldTooLong(name));
        }
        Ok(self.take(len)?.to_vec())
    }
    fn string(&mut self, name: &'static str, max: usize) -> Result<String, CodecError> {
        String::from_utf8(self.var(name, max)?).map_err(|_| CodecError::BadUtf8(name))
    }
    fn finish(self) -> Result<(), CodecError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(CodecError::TrailingBytes)
        }
    }
}

impl Invoice {
    /// Encodes the unsigned body.
    pub fn encode_body(&self) -> Result<Vec<u8>, CodecError> {
        let mut w = Writer::new();
        w.u8(self.network.to_byte());
        w.raw(&self.issuer_key);
        w.raw(&self.provider_node_id);
        w.raw(&self.client_node_id);
        w.var("service", self.service.as_bytes(), MAX_SERVICE_LEN)?;
        w.raw(&self.purchase_id);
        w.u64(self.amount_zat);
        w.var(
            "destination",
            self.destination.as_bytes(),
            MAX_DESTINATION_LEN,
        )?;
        w.var("memo", &self.memo, MAX_MEMO_LEN)?;
        w.u32(self.deadline_height);
        w.u32(self.access_blocks);
        w.u32(self.confirmations);
        Ok(w.0)
    }

    /// Decodes an unsigned body, refusing trailing bytes.
    pub fn decode_body(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(bytes)?;
        let inv = Invoice {
            network: Network::from_byte(r.u8()?)?,
            issuer_key: r.array()?,
            provider_node_id: r.array()?,
            client_node_id: r.array()?,
            service: r.string("service", MAX_SERVICE_LEN)?,
            purchase_id: r.array()?,
            amount_zat: r.u64()?,
            destination: r.string("destination", MAX_DESTINATION_LEN)?,
            memo: r.var("memo", MAX_MEMO_LEN)?,
            deadline_height: r.u32()?,
            access_blocks: r.u32()?,
            confirmations: r.u32()?,
        };
        r.finish()?;
        Ok(inv)
    }
}

impl Grant {
    /// Encodes the unsigned body.
    pub fn encode_body(&self) -> Result<Vec<u8>, CodecError> {
        let mut w = Writer::new();
        w.u8(self.network.to_byte());
        w.raw(&self.issuer_key);
        w.raw(&self.provider_node_id);
        w.raw(&self.client_node_id);
        w.var("service", self.service.as_bytes(), MAX_SERVICE_LEN)?;
        w.raw(&self.purchase_id);
        w.raw(&self.grant_id);
        w.u32(self.start_height);
        w.u32(self.end_height);
        w.u32(self.session_allowance);
        Ok(w.0)
    }

    /// Decodes an unsigned body, refusing trailing bytes.
    pub fn decode_body(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(bytes)?;
        let grant = Grant {
            network: Network::from_byte(r.u8()?)?,
            issuer_key: r.array()?,
            provider_node_id: r.array()?,
            client_node_id: r.array()?,
            service: r.string("service", MAX_SERVICE_LEN)?,
            purchase_id: r.array()?,
            grant_id: r.array()?,
            start_height: r.u32()?,
            end_height: r.u32()?,
            session_allowance: r.u32()?,
        };
        r.finish()?;
        Ok(grant)
    }
}

fn signed_message(domain: &[u8], body: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(domain.len() + 1 + body.len());
    msg.extend_from_slice(domain);
    msg.push(0);
    msg.extend_from_slice(body);
    msg
}

/// The 32-byte public key of a signing key.
pub fn public_key_bytes(key: &SigningKey) -> [u8; 32] {
    <[u8; 32]>::from(key.verification_key())
}

/// Parses a 32-byte Ed25519 public key.
pub fn verification_key(bytes: [u8; 32]) -> Result<VerificationKey, CodecError> {
    VerificationKey::try_from(bytes).map_err(|_| CodecError::BadKey)
}

/// Signs `body` in `domain` and returns `body || signature`.
pub fn sign(domain: &[u8], body: &[u8], key: &SigningKey) -> Vec<u8> {
    let sig = key.sign(&signed_message(domain, body));
    let mut out = body.to_vec();
    out.extend_from_slice(&sig.to_bytes());
    out
}

/// Checks the signature of `body || signature` in `domain` and returns the body.
pub fn open<'a>(
    domain: &[u8],
    signed: &'a [u8],
    key: &VerificationKey,
) -> Result<&'a [u8], CodecError> {
    let split = signed
        .len()
        .checked_sub(SIGNATURE_LEN)
        .ok_or(CodecError::Truncated)?;
    let (body, sig) = signed.split_at(split);
    let sig = <[u8; SIGNATURE_LEN]>::try_from(sig).map_err(|_| CodecError::Truncated)?;
    key.verify(&Signature::from_bytes(&sig), &signed_message(domain, body))
        .map_err(|_| CodecError::BadSignature)?;
    Ok(body)
}

/// Signs an invoice in the invoice domain.
pub fn sign_invoice(inv: &Invoice, key: &SigningKey) -> Result<Vec<u8>, CodecError> {
    Ok(sign(INVOICE_DOMAIN, &inv.encode_body()?, key))
}

/// Verifies a signed invoice in the invoice domain and decodes it.
pub fn verify_invoice(signed: &[u8], key: &VerificationKey) -> Result<Invoice, CodecError> {
    Invoice::decode_body(open(INVOICE_DOMAIN, signed, key)?)
}

/// Signs a grant in the grant domain.
pub fn sign_grant(grant: &Grant, key: &SigningKey) -> Result<Vec<u8>, CodecError> {
    Ok(sign(GRANT_DOMAIN, &grant.encode_body()?, key))
}

/// Verifies a signed grant in the grant domain and decodes it.
pub fn verify_grant(signed: &[u8], key: &VerificationKey) -> Result<Grant, CodecError> {
    Grant::decode_body(open(GRANT_DOMAIN, signed, key)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn invoice(k: &SigningKey) -> Invoice {
        Invoice {
            network: Network::Test,
            issuer_key: public_key_bytes(k),
            provider_node_id: [1; 32],
            client_node_id: [2; 32],
            service: "svc".into(),
            purchase_id: [3; 16],
            amount_zat: 1234,
            destination: "dest".into(),
            memo: b"memo".to_vec(),
            deadline_height: 100,
            access_blocks: 10,
            confirmations: 3,
        }
    }

    fn grant(k: &SigningKey) -> Grant {
        Grant {
            network: Network::Test,
            issuer_key: public_key_bytes(k),
            provider_node_id: [1; 32],
            client_node_id: [2; 32],
            service: "svc".into(),
            purchase_id: [3; 16],
            grant_id: [4; 16],
            start_height: 50,
            end_height: 60,
            session_allowance: 1,
        }
    }

    #[test]
    fn invoice_and_grant_round_trip() {
        let k = key(7);
        let vk = k.verification_key();
        let inv = invoice(&k);
        let signed = sign_invoice(&inv, &k).unwrap();
        assert_eq!(verify_invoice(&signed, &vk).unwrap(), inv);
        let g = grant(&k);
        let signed = sign_grant(&g, &k).unwrap();
        assert_eq!(verify_grant(&signed, &vk).unwrap(), g);
    }

    #[test]
    fn wrong_key_and_tampering_fail() {
        let k = key(7);
        let vk = k.verification_key();
        let signed = sign_invoice(&invoice(&k), &k).unwrap();
        assert_eq!(
            verify_invoice(&signed, &key(8).verification_key()),
            Err(CodecError::BadSignature)
        );
        let mut flipped = signed.clone();
        flipped[10] ^= 1;
        assert_eq!(verify_invoice(&flipped, &vk), Err(CodecError::BadSignature));
        assert_eq!(
            verify_invoice(&signed[..signed.len() - 1], &vk),
            Err(CodecError::BadSignature)
        );
        assert_eq!(
            verify_invoice(&signed[..10], &vk),
            Err(CodecError::Truncated)
        );
    }

    #[test]
    fn domains_do_not_cross() {
        let k = key(7);
        let vk = k.verification_key();
        // A grant body signed in the invoice domain is not a grant, and the reverse.
        let in_invoice_domain = sign(INVOICE_DOMAIN, &grant(&k).encode_body().unwrap(), &k);
        assert_eq!(
            verify_grant(&in_invoice_domain, &vk),
            Err(CodecError::BadSignature)
        );
        let in_grant_domain = sign(GRANT_DOMAIN, &invoice(&k).encode_body().unwrap(), &k);
        assert_eq!(
            verify_invoice(&in_grant_domain, &vk),
            Err(CodecError::BadSignature)
        );
    }

    #[test]
    fn strict_decoding() {
        let k = key(7);
        let mut body = invoice(&k).encode_body().unwrap();
        body.push(0);
        let signed = sign(INVOICE_DOMAIN, &body, &k);
        assert_eq!(
            verify_invoice(&signed, &k.verification_key()),
            Err(CodecError::TrailingBytes)
        );
        let mut long = invoice(&k);
        long.service = "x".repeat(MAX_SERVICE_LEN + 1);
        assert_eq!(long.encode_body(), Err(CodecError::FieldTooLong("service")));
        let mut body = invoice(&k).encode_body().unwrap();
        body[0] = 9;
        assert_eq!(Invoice::decode_body(&body), Err(CodecError::BadVersion(9)));
    }
}
