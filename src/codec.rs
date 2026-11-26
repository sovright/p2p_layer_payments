//! Issuer-signed invoices and grants.
//!
//! The byte format here is a prototype format, not a Zakura wire format. A signed object is
//! `body || signature`, where the Ed25519 signature covers `domain || 0x00 || body` and the
//! invoice and grant use different domains, so neither can be replayed as the other.
//! Verification uses `verify_strict`.

use std::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use zcash_protocol::consensus::NetworkType;

pub const INVOICE_DOMAIN: &[u8] = b"paid-stream-bridge/prototype/v1/invoice";
pub const GRANT_DOMAIN: &[u8] = b"paid-stream-bridge/prototype/v1/grant";

const FORMAT_VERSION: u8 = 1;
pub const SIGNATURE_LEN: usize = 64;
pub const MAX_SERVICE_LEN: usize = 64;
pub const MAX_DESTINATION_LEN: usize = 512;
pub const MAX_MEMO_LEN: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Main,
    Test,
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
    pub network: Network,
    /// The issuer's public key. It only names the signer; trust comes from the pinned key.
    pub issuer_key: [u8; 32],
    pub provider_node_id: [u8; 32],
    pub client_node_id: [u8; 32],
    pub service: String,
    pub purchase_id: [u8; 16],
    pub amount_zat: u64,
    pub destination: String,
    pub memo: Vec<u8>,
    /// Inclusive inclusion deadline on the provider's chain.
    pub deadline_height: u32,
    pub access_blocks: u32,
    pub confirmations: u32,
}

/// The issuer's access authorization for one purchase. It is not on-chain proof of payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub network: Network,
    pub issuer_key: [u8; 32],
    pub provider_node_id: [u8; 32],
    pub client_node_id: [u8; 32],
    pub service: String,
    pub purchase_id: [u8; 16],
    pub grant_id: [u8; 16],
    pub start_height: u32,
    /// Exclusive: access ends when the provider's tip reaches this height.
    pub end_height: u32,
    pub session_allowance: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecError {
    Truncated,
    TrailingBytes,
    BadVersion(u8),
    BadNetwork(u8),
    FieldTooLong(&'static str),
    BadUtf8(&'static str),
    BadSignature,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Truncated => write!(f, "object is truncated"),
            CodecError::TrailingBytes => write!(f, "object has trailing bytes"),
            CodecError::BadVersion(v) => write!(f, "unknown format version {v}"),
            CodecError::BadNetwork(n) => write!(f, "unknown network byte {n}"),
            CodecError::FieldTooLong(name) => write!(f, "field {name} is too long"),
            CodecError::BadUtf8(name) => write!(f, "field {name} is not UTF-8"),
            CodecError::BadSignature => {
                write!(f, "signature does not verify against the pinned issuer key")
            }
        }
    }
}

impl std::error::Error for CodecError {}

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
        if v.len() > max {
            return Err(CodecError::FieldTooLong(name));
        }
        self.0.extend_from_slice(&(v.len() as u16).to_be_bytes());
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
    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }
    fn var(&mut self, name: &'static str, max: usize) -> Result<Vec<u8>, CodecError> {
        let len = u16::from_be_bytes(self.take(2)?.try_into().expect("2 bytes")) as usize;
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
    key: &VerifyingKey,
) -> Result<&'a [u8], CodecError> {
    if signed.len() < SIGNATURE_LEN {
        return Err(CodecError::Truncated);
    }
    let (body, sig) = signed.split_at(signed.len() - SIGNATURE_LEN);
    let sig = Signature::from_bytes(sig.try_into().expect("64 bytes"));
    key.verify_strict(&signed_message(domain, body), &sig)
        .map_err(|_| CodecError::BadSignature)?;
    Ok(body)
}

pub fn sign_invoice(inv: &Invoice, key: &SigningKey) -> Result<Vec<u8>, CodecError> {
    Ok(sign(INVOICE_DOMAIN, &inv.encode_body()?, key))
}

pub fn verify_invoice(signed: &[u8], key: &VerifyingKey) -> Result<Invoice, CodecError> {
    Invoice::decode_body(open(INVOICE_DOMAIN, signed, key)?)
}

pub fn sign_grant(grant: &Grant, key: &SigningKey) -> Result<Vec<u8>, CodecError> {
    Ok(sign(GRANT_DOMAIN, &grant.encode_body()?, key))
}

pub fn verify_grant(signed: &[u8], key: &VerifyingKey) -> Result<Grant, CodecError> {
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
            issuer_key: k.verifying_key().to_bytes(),
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
            issuer_key: k.verifying_key().to_bytes(),
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
        let inv = invoice(&k);
        let signed = sign_invoice(&inv, &k).unwrap();
        assert_eq!(verify_invoice(&signed, &k.verifying_key()).unwrap(), inv);
        let g = grant(&k);
        let signed = sign_grant(&g, &k).unwrap();
        assert_eq!(verify_grant(&signed, &k.verifying_key()).unwrap(), g);
    }

    #[test]
    fn wrong_key_and_tampering_fail() {
        let k = key(7);
        let signed = sign_invoice(&invoice(&k), &k).unwrap();
        assert_eq!(
            verify_invoice(&signed, &key(8).verifying_key()),
            Err(CodecError::BadSignature)
        );
        let mut flipped = signed.clone();
        flipped[10] ^= 1;
        assert_eq!(
            verify_invoice(&flipped, &k.verifying_key()),
            Err(CodecError::BadSignature)
        );
        assert_eq!(
            verify_invoice(&signed[..signed.len() - 1], &k.verifying_key()),
            Err(CodecError::BadSignature)
        );
        assert_eq!(
            verify_invoice(&signed[..10], &k.verifying_key()),
            Err(CodecError::Truncated)
        );
    }

    #[test]
    fn domains_do_not_cross() {
        let k = key(7);
        // A grant body signed in the invoice domain is not a grant, and the reverse.
        let g = grant(&k);
        let in_invoice_domain = sign(INVOICE_DOMAIN, &g.encode_body().unwrap(), &k);
        assert_eq!(
            verify_grant(&in_invoice_domain, &k.verifying_key()),
            Err(CodecError::BadSignature)
        );
        let inv = invoice(&k);
        let in_grant_domain = sign(GRANT_DOMAIN, &inv.encode_body().unwrap(), &k);
        assert_eq!(
            verify_invoice(&in_grant_domain, &k.verifying_key()),
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
            verify_invoice(&signed, &k.verifying_key()),
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
