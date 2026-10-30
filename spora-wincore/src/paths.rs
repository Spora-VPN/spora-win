//! Where the service keeps what it keeps. On Windows everything lives under
//! `%ProgramData%\Spora`, which is private: SYSTEM and Administrators only
//! (no user reads anything there; the UI gets what it shows over the pipe).
//!
//! - `users\<SID>\` — one Windows user's own (see `users.rs`):
//!   - `service\` — spora-service's `service.json` (the user's device
//!     identity, upload switch, registration, relay access), the
//!     DPAPI-protected key its secrets are sealed with (`state.key`), and
//!     the relay settings (`relays.json`);
//!   - `diag\` — the diagnostic log and crash files; `records\` — the
//!     connection records; `reports\` — bug-report zips while they are
//!     being streamed to the UI;
//!   - `connlog\<routing hash>\` — each share's connection log.
//! - `spora-wincore.log` — the human-readable service log (every user's
//!   sessions; it never leaves the machine).
//!
//! None of it may be backed up and restored onto another machine: a restored
//! device token would clone a device (the DPAPI key does not open elsewhere,
//! so a restore there starts as a fresh device).
//!
//! Linux dev builds keep the same layout under `$SPORA_WINCORE_DATA`, else
//! `$XDG_STATE_HOME/spora-wincore`, else `~/.local/state/spora-wincore`.

use std::io;
use std::path::{Path, PathBuf};

use crate::users::UserId;

pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into()))
            .join("Spora")
    }
    #[cfg(not(windows))]
    {
        if let Some(d) = std::env::var_os("SPORA_WINCORE_DATA") {
            return d.into();
        }
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("spora-wincore")
    }
}

/// The human-readable service log of a `--service` run.
pub fn service_log() -> PathBuf {
    data_dir().join("spora-wincore.log")
}

/// One Windows user's directories.
#[derive(Clone, Debug)]
pub struct UserDirs {
    root: PathBuf,
}

impl UserDirs {
    pub fn of(user: &UserId) -> UserDirs {
        UserDirs {
            root: data_dir().join("users").join(user.as_str()),
        }
    }

    /// spora-service's state directory (plus the state key and the relay
    /// settings).
    pub fn service(&self) -> PathBuf {
        self.root.join("service")
    }

    pub fn diag(&self) -> PathBuf {
        self.root.join("diag")
    }

    pub fn records(&self) -> PathBuf {
        self.root.join("records")
    }

    pub fn reports(&self) -> PathBuf {
        self.root.join("reports")
    }

    /// A share's connection log: per routing hash, so two shares never
    /// interleave their logs (the core binds the log's database to it).
    pub fn connlog(&self, routing_hash: &[u8]) -> PathBuf {
        self.root
            .join("connlog")
            .join(spora_core::identity::hex(routing_hash))
    }

    /// Create them, private (the root is restricted again on its own, in
    /// case `%ProgramData%\Spora` could not be).
    pub fn ensure(&self) -> io::Result<()> {
        ensure_private_dir(&self.root)?;
        for d in [self.service(), self.diag(), self.records(), self.reports()] {
            std::fs::create_dir_all(&d)?;
        }
        Ok(())
    }
}

/// `dir`, created if need be and readable by SYSTEM and Administrators
/// only (Unix: 0700). Applied to an existing directory too: what is inside
/// gets the same.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    restrict(dir)
}

/// `D:P`: protected (nothing inherited from `%ProgramData%`, whose default
/// lets every user read); `OICI`: what is created inside inherits it.
#[cfg(windows)]
const PRIVATE_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

#[cfg(windows)]
fn restrict(dir: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
        SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
        PROTECTED_DACL_SECURITY_INFORMATION,
    };

    let sddl: Vec<u16> = PRIVATE_SDDL.encode_utf16().chain(Some(0)).collect();
    let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: plain Win32 calls on buffers that outlive them; the
    // descriptor is LocalAlloc'd by the conversion and freed below.
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let got = GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted);
        let result = if got == 0 {
            Err(io::Error::last_os_error())
        } else {
            let path: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
            // SetNamedSecurityInfo (not SetFileSecurity) so what is already
            // inside gets the new inherited entries too.
            let rc = SetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            );
            if rc == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(rc as i32))
            }
        };
        LocalFree(sd);
        result
    }
}

#[cfg(not(windows))]
fn restrict(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Write `data` to `path` atomically: a sibling temp file renamed over it,
/// so an interrupted write never leaves a truncated file behind.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)
        .and_then(|_| std::fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
}
