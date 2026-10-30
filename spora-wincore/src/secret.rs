//! Secrets at rest. spora-service seals the secrets in `service.json` (the
//! device's private key, the device token, the relay token) with a 32-byte
//! state key the platform keeps in its secure store; on Windows that store
//! is DPAPI under the service's account (LocalSystem): `state.key` holds the
//! key as a DPAPI blob only a SYSTEM process on this machine can open. The
//! own relays' tokens in `relays.json` are DPAPI blobs the same way.
//!
//! A `state.key` that does not open here (copied from another machine, the
//! SYSTEM profile reset) is replaced by a new key, and `service.json`
//! sealed with the old one is then state loss: a fresh device, never a
//! clone. Linux dev builds keep everything in the clear (file permissions
//! only), as the CLI does.

use std::path::Path;

use spora_service::StateKey;

#[cfg(windows)]
const KEY_FILE: &str = "state.key";

/// The state key for spora-service's state in `dir`: the kept one, or a new
/// one, kept before it is used (so a state is never sealed with a key that
/// is not on disk). An error when `state.key` exists but cannot be read,
/// rather than a new key that would throw the device away.
#[cfg(windows)]
pub fn state_key(dir: &Path) -> Result<StateKey, String> {
    let path = dir.join(KEY_FILE);
    match std::fs::read(&path) {
        Ok(blob) => match dpapi::unprotect(&blob) {
            Ok(key) if key.len() == spora_service::seal::STATE_KEY_LEN => {
                return Ok(StateKey::Provided(key));
            }
            Ok(_) => log::warn!(
                "state key in {} has the wrong length; making a new one (a fresh device)",
                path.display()
            ),
            Err(e) => log::warn!(
                "state key in {} does not open on this machine ({e}); making a new one (a fresh device)",
                path.display()
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    }
    let key = dpapi::random(spora_service::seal::STATE_KEY_LEN)?;
    let blob = dpapi::protect(&key)?;
    crate::paths::write_atomic(&path, &blob)
        .map_err(|e| format!("keeping the state key in {}: {e}", path.display()))?;
    log::info!("made a new state key ({})", path.display());
    Ok(StateKey::Provided(key))
}

#[cfg(not(windows))]
pub fn state_key(_dir: &Path) -> Result<StateKey, String> {
    Ok(StateKey::Plaintext)
}

const SEALED_PREFIX: &str = "dpapi:";

/// A small secret (a relay token) as it is written to disk.
pub fn seal(plain: &str) -> String {
    #[cfg(windows)]
    {
        use base64::Engine as _;
        match dpapi::protect(plain.as_bytes()) {
            Ok(blob) => {
                return format!(
                    "{SEALED_PREFIX}{}",
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(blob)
                );
            }
            // Kept in the clear (the directory is private) rather than lost.
            Err(e) => log::warn!("cannot seal a relay token ({e}); keeping it unsealed"),
        }
    }
    plain.to_string()
}

/// [`seal`] undone; `None` when a sealed value does not open here.
pub fn unseal(stored: &str) -> Option<String> {
    let Some(b64) = stored.strip_prefix(SEALED_PREFIX) else {
        return Some(stored.to_string());
    };
    #[cfg(windows)]
    {
        use base64::Engine as _;
        let blob = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(b64)
            .ok()?;
        let plain = dpapi::unprotect(&blob).ok()?;
        String::from_utf8(plain).ok()
    }
    #[cfg(not(windows))]
    {
        let _ = b64;
        None
    }
}

#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom, CRYPT_INTEGER_BLOB,
        CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    /// Binds every blob to this purpose: DPAPI blobs other software made
    /// under SYSTEM do not open as ours, nor ours as theirs.
    const ENTROPY: &[u8] = b"Spora wincore secret v1";

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    /// Takes the output blob's bytes and frees it.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY (caller): `out` came from a successful DPAPI call, which
        // LocalAlloc'd `cbData` bytes at `pbData`.
        let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
        unsafe { LocalFree(out.pbData as _) };
        v
    }

    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(plain);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: the input blobs point at live slices; DPAPI only reads them.
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(format!(
                "CryptProtectData: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(unsafe { take(out) })
    }

    pub fn unprotect(sealed: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(sealed);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: as in `protect`.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(format!(
                "CryptUnprotectData: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(unsafe { take(out) })
    }

    pub fn random(len: usize) -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; len];
        // SAFETY: writes exactly `len` bytes into `buf`.
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                buf.as_mut_ptr(),
                len as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status != 0 {
            return Err(format!("BCryptGenRandom: NTSTATUS {status:#x}"));
        }
        Ok(buf)
    }
}
