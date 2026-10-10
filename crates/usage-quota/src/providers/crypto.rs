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

/// Chromium's Windows `safeStorage` key is a DPAPI blob prefixed by ASCII
/// `DPAPI`. `CryptUnprotectData` allocates the output with LocalAlloc; copy it
/// before freeing it so no Windows-owned pointer escapes this function.
#[cfg(windows)]
pub(crate) fn dpapi_unprotect(data: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    let cb_data = u32::try_from(data.len()).ok()?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: cb_data,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut output,
        )
    };
    if ok == 0 || output.pbData.is_null() {
        return None;
    }

    let plaintext =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Some(plaintext)
}

/// Current Chromium/Electron Windows envelopes are `v10 || nonce(12) ||
/// AES-256-GCM(ciphertext || tag)`, with empty additional authenticated data.
#[cfg(windows)]
pub(crate) fn decrypt_windows_v10(key: &[u8], blob: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::{
        aead::{Aead, KeyInit},
        Aes256Gcm, Nonce,
    };

    let payload = blob.strip_prefix(b"v10")?;
    if payload.len() <= 12 {
        return None;
    }
    let (nonce, ciphertext) = payload.split_at(12);
    if ciphertext.len() < 16 {
        return None;
    }
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    cipher.decrypt(Nonce::from_slice(nonce), ciphertext).ok()
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
