//! ZIP 321 payment requests.
//!
//! The bridge builds the request itself and then reads it back with the `zip321` reference
//! parser, checking address, network, amount and memo against the verified invoice. That
//! checks the bridge's own output. It is not evidence that any wallet accepts the request,
//! and nothing here opens the request in a wallet.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use zcash_address::{unified, ConversionError, TryFromAddress, ZcashAddress};
use zcash_protocol::{consensus::NetworkType, memo::MemoBytes, value::Zatoshis};
use zip321::TransactionRequest;

use crate::codec::{Invoice, Network};

/// Accepts shielded addresses only, which can all carry a memo.
struct Shielded;

impl TryFromAddress for Shielded {
    type Error = ();

    fn try_from_sapling(
        _net: NetworkType,
        _data: [u8; 43],
    ) -> Result<Self, ConversionError<Self::Error>> {
        Ok(Shielded)
    }

    fn try_from_unified(
        _net: NetworkType,
        _data: unified::Address,
    ) -> Result<Self, ConversionError<Self::Error>> {
        Ok(Shielded)
    }
}

/// The invoice destination must be a shielded address on the configured network.
pub fn check_destination(destination: &str, network: Network) -> Result<(), String> {
    let addr = ZcashAddress::try_from_encoded(destination)
        .map_err(|e| format!("destination is not a Zcash address: {e}"))?;
    addr.convert_if_network::<Shielded>(network.network_type())
        .map(|_| ())
        .map_err(|e| match e {
            ConversionError::IncorrectNetwork { expected, actual } => {
                format!("destination is for network {actual:?}, expected {expected:?}")
            }
            ConversionError::Unsupported(u) => {
                format!("destination is not a shielded address: {u}")
            }
            ConversionError::User(()) => "destination was refused".to_string(),
        })
}

/// Formats zatoshis as a ZIP 321 decimal ZEC amount.
pub fn format_zec(zat: u64) -> String {
    let whole = zat / 100_000_000;
    let frac = zat % 100_000_000;
    if frac == 0 {
        whole.to_string()
    } else {
        let digits = format!("{frac:08}");
        format!("{whole}.{}", digits.trim_end_matches('0'))
    }
}

/// `zcash:<address>?amount=<ZEC>[&memo=<base64url>]`: one payment, no `req-` parameters.
pub fn build_payment_uri(inv: &Invoice) -> String {
    let mut uri = format!(
        "zcash:{}?amount={}",
        inv.destination,
        format_zec(inv.amount_zat)
    );
    if !inv.memo.is_empty() {
        uri.push_str("&memo=");
        uri.push_str(&URL_SAFE_NO_PAD.encode(&inv.memo));
    }
    uri
}

/// Reads `uri` back with the reference parser and compares it with the invoice.
pub fn verify_payment_uri(uri: &str, inv: &Invoice, network: Network) -> Result<(), String> {
    let req = TransactionRequest::from_uri(uri)
        .map_err(|e| format!("payment request does not parse: {e}"))?;
    let payments = req.payments();
    if payments.len() != 1 {
        return Err(format!(
            "payment request has {} payments, expected 1",
            payments.len()
        ));
    }
    let payment = payments.values().next().expect("one payment");
    let address = payment.recipient_address().encode();
    if address != inv.destination {
        return Err("payment request address differs from the invoice".into());
    }
    check_destination(&address, network)?;
    let expected_amount =
        Zatoshis::from_u64(inv.amount_zat).map_err(|_| "invoice amount is out of range")?;
    if payment.amount() != Some(expected_amount) {
        return Err("payment request amount differs from the invoice".into());
    }
    match (payment.memo(), inv.memo.is_empty()) {
        (None, true) => {}
        (Some(memo), false) => {
            let expected =
                MemoBytes::from_bytes(&inv.memo).map_err(|_| "invoice memo is too long")?;
            if *memo != expected {
                return Err("payment request memo differs from the invoice".into());
            }
        }
        _ => return Err("payment request memo presence differs from the invoice".into()),
    }
    if payment.label().is_some() || payment.message().is_some() {
        return Err("payment request carries a label or message".into());
    }
    if !payment.other_params().is_empty() {
        return Err("payment request carries extra parameters".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zec_formatting() {
        assert_eq!(format_zec(0), "0");
        assert_eq!(format_zec(100_000_000), "1");
        assert_eq!(format_zec(25_000), "0.00025");
        assert_eq!(format_zec(123_456_789), "1.23456789");
        assert_eq!(format_zec(150_000_000), "1.5");
    }
}
