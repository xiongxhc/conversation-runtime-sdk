//! Standard base64, the one place audio is encoded for a provider request body.
//!
//! Extracted from the streaming speech-to-text adapter when the batch diarizer needed the same
//! encoding: two copies of an alphabet table is exactly the kind of duplication that drifts.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut group = [0_u8; 3];
        group[..chunk.len()].copy_from_slice(chunk);
        let packed = u32::from(group[0]) << 16 | u32::from(group[1]) << 8 | u32::from(group[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                let symbol = (packed >> (18 - 6 * index)) & 0x3F;
                encoded.push(char::from(ALPHABET[symbol as usize]));
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::encode;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(encode(&[0xFF, 0xEF, 0xBF]), "/++/");
    }
}
