//! Shared little helpers that several providers need. No policy here.

use sha2::{Digest, Sha256};

/// HMAC-SHA256 over `sha2`. The workspace has no `hmac` crate and this
/// two-pad construction is pinned by the golden-vector tests below.
pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.iter().map(|b| b ^ 0x36).collect::<Vec<u8>>());
    inner.update(data);
    let mut outer = Sha256::new();
    outer.update(block.iter().map(|b| b ^ 0x5c).collect::<Vec<u8>>());
    outer.update(inner.finalize());
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4231 test case 2.
    #[test]
    fn the_rfc_golden_vector_holds() {
        let out = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            out.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
