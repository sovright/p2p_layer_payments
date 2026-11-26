//! Lower-case hex, the encoding every byte field uses in JSON and on the command line.

const DIGITS: &[u8; 16] = b"0123456789abcdef";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

pub fn decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("hex string has an odd length".into());
    }
    s.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = nibble(pair[0])?;
            let lo = nibble(pair[1])?;
            Ok((hi << 4) | lo)
        })
        .collect()
}

pub fn decode_array<const N: usize>(s: &str) -> Result<[u8; N], String> {
    let bytes = decode(s)?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| format!("expected {N} bytes, got {}", v.len()))
}

fn nibble(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("invalid hex character {:?}", c as char)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_errors() {
        let bytes = [0x00, 0x7f, 0xff, 0x10];
        assert_eq!(encode(&bytes), "007fff10");
        assert_eq!(decode("007fff10").unwrap(), bytes);
        assert_eq!(decode_array::<4>("007FFF10").unwrap(), bytes);
        assert!(decode("abc").is_err());
        assert!(decode("zz").is_err());
        assert!(decode_array::<3>("007fff10").is_err());
    }
}
