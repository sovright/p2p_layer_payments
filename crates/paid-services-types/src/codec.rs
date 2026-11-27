//! Issuer-signed invoices and grants for any paid service.
//!
//! The byte format here is a prototype format, not a Zakura wire format. A signed object is
//! `body || signature`, where the Ed25519 signature covers `domain || 0x00 || body` and the
//! invoice and grant use different domains, so neither can be replayed as the other.
//! Signatures are checked with `ed25519-zebra`, which applies the ZIP 215 validation rules.
//!
//! Every object names a versioned `service_kind` and the `offer_id` it was bought under. The
//! terms are typed, and the grant carries the matching entitlement. `service_params` are
//! opaque here; only the adapter of that service kind reads and validates them.

use ed25519_zebra::Signature;
pub use ed25519_zebra::{SigningKey, VerificationKey};
use serde::{Deserialize, Serialize};
use zcash_protocol::consensus::NetworkType;

/// Signature domain of invoices.
pub const INVOICE_DOMAIN: &[u8] = b"paid-services/prototype/v2/invoice";
/// Signature domain of grants.
pub const GRANT_DOMAIN: &[u8] = b"paid-services/prototype/v2/grant";

const FORMAT_VERSION: u8 = 2;
/// Length of the trailing Ed25519 signature.
pub const SIGNATURE_LEN: usize = 64;
/// Longest service kind, in bytes.
pub const MAX_SERVICE_KIND_LEN: usize = 64;
/// Longest offer id, in bytes.
pub const MAX_OFFER_ID_LEN: usize = 64;
/// Longest destination address, in bytes.
pub const MAX_DESTINATION_LEN: usize = 512;
/// Longest memo, in bytes.
pub const MAX_MEMO_LEN: usize = 512;
/// Longest `service_params`, in bytes. The service adapter may set a lower limit.
pub const MAX_SERVICE_PARAMS_LEN: usize = 1024;

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

/// What a purchase buys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terms {
    /// Access for this many blocks from issuance.
    HeightWindow {
        /// Access length in blocks.
        blocks: u32,
    },
    /// A number of units of the service.
    Quota {
        /// Units bought.
        units: u64,
    },
    /// One use.
    OneShot,
}

/// What a grant entitles its holder to; it matches the invoice's terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entitlement {
    /// Access from `start_height` until the provider's tip reaches `end_height` (exclusive).
    HeightWindow {
        /// First height of access.
        start_height: u32,
        /// Exclusive end height.
        end_height: u32,
    },
    /// A number of units of the service.
    Quota {
        /// Units granted.
        units: u64,
    },
    /// One use.
    OneShot,
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
    /// The versioned service kind, for example `zakura.relay.blockstream/1`.
    pub service_kind: String,
    /// The offer the purchase was made under.
    pub offer_id: String,
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
    /// Confirmations the issuer waits for before granting.
    pub confirmations: u32,
    /// What the purchase buys.
    pub terms: Terms,
    /// Parameters only the service's adapter reads.
    pub service_params: Vec<u8>,
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
    /// The versioned service kind.
    pub service_kind: String,
    /// The offer the purchase was made under.
    pub offer_id: String,
    /// The purchase this grant belongs to.
    pub purchase_id: [u8; 16],
    /// The grant's identifier.
    pub grant_id: [u8; 16],
    /// What the grant entitles its holder to.
    pub entitlement: Entitlement,
    /// The invoice's service parameters, for whoever redeems the grant.
    pub service_params: Vec<u8>,
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
    /// Unknown terms or entitlement tag.
    #[error("unknown terms tag {0}")]
    BadTerms(u8),
    /// A variable-length field is too long.
    #[error("field {0} is too long")]
    FieldTooLong(&'static str),
    /// A text field is not UTF-8.
    #[error("field {0} is not UTF-8")]
    BadUtf8(&'static str),
    /// A service kind or offer id has the wrong shape.
    #[error("field {0} is malformed")]
    Malformed(&'static str),
    /// The signature does not verify.
    #[error("signature does not verify against the pinned issuer key")]
    BadSignature,
    /// The bytes are not an Ed25519 public key.
    #[error("not a valid Ed25519 public key")]
    BadKey,
}

/// Whether `kind` is a versioned service kind: `<name>/<version>`, where the name is
/// lower-case letters, digits, dots and dashes, and the version is decimal digits.
pub fn is_service_kind(kind: &str) -> bool {
    let Some((name, version)) = kind.split_once('/') else {
        return false;
    };
    kind.len() <= MAX_SERVICE_KIND_LEN
        && !name.is_empty()
        && !version.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && version.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `offer_id` is 1 to 64 letters, digits, dots, dashes or underscores.
pub fn is_offer_id(offer_id: &str) -> bool {
    !offer_id.is_empty()
        && offer_id.len() <= MAX_OFFER_ID_LEN
        && offer_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
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
    fn names(&mut self, service_kind: &str, offer_id: &str) -> Result<(), CodecError> {
        if !is_service_kind(service_kind) {
            return Err(CodecError::Malformed("service_kind"));
        }
        if !is_offer_id(offer_id) {
            return Err(CodecError::Malformed("offer_id"));
        }
        self.var(
            "service_kind",
            service_kind.as_bytes(),
            MAX_SERVICE_KIND_LEN,
        )?;
        self.var("offer_id", offer_id.as_bytes(), MAX_OFFER_ID_LEN)
    }
    fn terms(&mut self, terms: &Terms) {
        match terms {
            Terms::HeightWindow { blocks } => {
                self.u8(1);
                self.u32(*blocks);
            }
            Terms::Quota { units } => {
                self.u8(2);
                self.u64(*units);
            }
            Terms::OneShot => self.u8(3),
        }
    }
    fn entitlement(&mut self, entitlement: &Entitlement) {
        match entitlement {
            Entitlement::HeightWindow {
                start_height,
                end_height,
            } => {
                self.u8(1);
                self.u32(*start_height);
                self.u32(*end_height);
            }
            Entitlement::Quota { units } => {
                self.u8(2);
                self.u64(*units);
            }
            Entitlement::OneShot => self.u8(3),
        }
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
    fn names(&mut self) -> Result<(String, String), CodecError> {
        let kind = self.string("service_kind", MAX_SERVICE_KIND_LEN)?;
        if !is_service_kind(&kind) {
            return Err(CodecError::Malformed("service_kind"));
        }
        let offer = self.string("offer_id", MAX_OFFER_ID_LEN)?;
        if !is_offer_id(&offer) {
            return Err(CodecError::Malformed("offer_id"));
        }
        Ok((kind, offer))
    }
    fn terms(&mut self) -> Result<Terms, CodecError> {
        match self.u8()? {
            1 => Ok(Terms::HeightWindow {
                blocks: self.u32()?,
            }),
            2 => Ok(Terms::Quota { units: self.u64()? }),
            3 => Ok(Terms::OneShot),
            other => Err(CodecError::BadTerms(other)),
        }
    }
    fn entitlement(&mut self) -> Result<Entitlement, CodecError> {
        match self.u8()? {
            1 => Ok(Entitlement::HeightWindow {
                start_height: self.u32()?,
                end_height: self.u32()?,
            }),
            2 => Ok(Entitlement::Quota { units: self.u64()? }),
            3 => Ok(Entitlement::OneShot),
            other => Err(CodecError::BadTerms(other)),
        }
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
        w.names(&self.service_kind, &self.offer_id)?;
        w.raw(&self.purchase_id);
        w.u64(self.amount_zat);
        w.var(
            "destination",
            self.destination.as_bytes(),
            MAX_DESTINATION_LEN,
        )?;
        w.var("memo", &self.memo, MAX_MEMO_LEN)?;
        w.u32(self.deadline_height);
        w.u32(self.confirmations);
        w.terms(&self.terms);
        w.var(
            "service_params",
            &self.service_params,
            MAX_SERVICE_PARAMS_LEN,
        )?;
        Ok(w.0)
    }

    /// Decodes an unsigned body, refusing trailing bytes.
    pub fn decode_body(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(bytes)?;
        let network = Network::from_byte(r.u8()?)?;
        let issuer_key = r.array()?;
        let provider_node_id = r.array()?;
        let client_node_id = r.array()?;
        let (service_kind, offer_id) = r.names()?;
        let inv = Invoice {
            network,
            issuer_key,
            provider_node_id,
            client_node_id,
            service_kind,
            offer_id,
            purchase_id: r.array()?,
            amount_zat: r.u64()?,
            destination: r.string("destination", MAX_DESTINATION_LEN)?,
            memo: r.var("memo", MAX_MEMO_LEN)?,
            deadline_height: r.u32()?,
            confirmations: r.u32()?,
            terms: r.terms()?,
            service_params: r.var("service_params", MAX_SERVICE_PARAMS_LEN)?,
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
        w.names(&self.service_kind, &self.offer_id)?;
        w.raw(&self.purchase_id);
        w.raw(&self.grant_id);
        w.entitlement(&self.entitlement);
        w.var(
            "service_params",
            &self.service_params,
            MAX_SERVICE_PARAMS_LEN,
        )?;
        Ok(w.0)
    }

    /// Decodes an unsigned body, refusing trailing bytes.
    pub fn decode_body(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(bytes)?;
        let network = Network::from_byte(r.u8()?)?;
        let issuer_key = r.array()?;
        let provider_node_id = r.array()?;
        let client_node_id = r.array()?;
        let (service_kind, offer_id) = r.names()?;
        let grant = Grant {
            network,
            issuer_key,
            provider_node_id,
            client_node_id,
            service_kind,
            offer_id,
            purchase_id: r.array()?,
            grant_id: r.array()?,
            entitlement: r.entitlement()?,
            service_params: r.var("service_params", MAX_SERVICE_PARAMS_LEN)?,
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

    fn invoice(k: &SigningKey, terms: Terms) -> Invoice {
        Invoice {
            network: Network::Test,
            issuer_key: public_key_bytes(k),
            provider_node_id: [1; 32],
            client_node_id: [2; 32],
            service_kind: "example.service/1".into(),
            offer_id: "offer-1".into(),
            purchase_id: [3; 16],
            amount_zat: 1234,
            destination: "dest".into(),
            memo: b"memo".to_vec(),
            deadline_height: 100,
            confirmations: 3,
            terms,
            service_params: vec![1, 2, 3],
        }
    }

    fn grant(k: &SigningKey, entitlement: Entitlement) -> Grant {
        Grant {
            network: Network::Test,
            issuer_key: public_key_bytes(k),
            provider_node_id: [1; 32],
            client_node_id: [2; 32],
            service_kind: "example.service/1".into(),
            offer_id: "offer-1".into(),
            purchase_id: [3; 16],
            grant_id: [4; 16],
            entitlement,
            service_params: vec![1, 2, 3],
        }
    }

    #[test]
    fn every_terms_and_entitlement_kind_round_trips() {
        let k = key(7);
        let vk = k.verification_key();
        for terms in [
            Terms::HeightWindow { blocks: 10 },
            Terms::Quota { units: 5 },
            Terms::OneShot,
        ] {
            let inv = invoice(&k, terms);
            let signed = sign_invoice(&inv, &k).unwrap();
            assert_eq!(verify_invoice(&signed, &vk).unwrap(), inv);
        }
        for entitlement in [
            Entitlement::HeightWindow {
                start_height: 50,
                end_height: 60,
            },
            Entitlement::Quota { units: 5 },
            Entitlement::OneShot,
        ] {
            let g = grant(&k, entitlement);
            let signed = sign_grant(&g, &k).unwrap();
            assert_eq!(verify_grant(&signed, &vk).unwrap(), g);
        }
    }

    #[test]
    fn wrong_key_and_tampering_fail() {
        let k = key(7);
        let vk = k.verification_key();
        let signed = sign_invoice(&invoice(&k, Terms::OneShot), &k).unwrap();
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
        let g = grant(&k, Entitlement::OneShot);
        let in_invoice_domain = sign(INVOICE_DOMAIN, &g.encode_body().unwrap(), &k);
        assert_eq!(
            verify_grant(&in_invoice_domain, &vk),
            Err(CodecError::BadSignature)
        );
        let inv = invoice(&k, Terms::OneShot);
        let in_grant_domain = sign(GRANT_DOMAIN, &inv.encode_body().unwrap(), &k);
        assert_eq!(
            verify_invoice(&in_grant_domain, &vk),
            Err(CodecError::BadSignature)
        );
    }

    #[test]
    fn strict_decoding_and_names() {
        let k = key(7);
        let mut body = invoice(&k, Terms::OneShot).encode_body().unwrap();
        body.push(0);
        let signed = sign(INVOICE_DOMAIN, &body, &k);
        assert_eq!(
            verify_invoice(&signed, &k.verification_key()),
            Err(CodecError::TrailingBytes)
        );
        let mut bad = invoice(&k, Terms::OneShot);
        bad.service_kind = "Relay".into();
        assert_eq!(
            bad.encode_body(),
            Err(CodecError::Malformed("service_kind"))
        );
        let mut bad = invoice(&k, Terms::OneShot);
        bad.offer_id = String::new();
        assert_eq!(bad.encode_body(), Err(CodecError::Malformed("offer_id")));
        let mut big = invoice(&k, Terms::OneShot);
        big.service_params = vec![0; MAX_SERVICE_PARAMS_LEN + 1];
        assert_eq!(
            big.encode_body(),
            Err(CodecError::FieldTooLong("service_params"))
        );
        let mut body = invoice(&k, Terms::OneShot).encode_body().unwrap();
        body[0] = 1;
        assert_eq!(Invoice::decode_body(&body), Err(CodecError::BadVersion(1)));
        assert!(is_service_kind("zakura.relay.blockstream/1"));
        assert!(!is_service_kind("zakura.relay.blockstream"));
        assert!(!is_service_kind("zakura.relay/v1"));
        assert!(is_offer_id("relay-10"));
        assert!(!is_offer_id("relay 10"));
    }
}
