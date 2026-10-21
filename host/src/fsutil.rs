//! Small filesystem helper shared by the on-disk secret stores (slice
//! 3.2b): `device::DeviceStore` and `access::AccessStore` both write a
//! single small secret file atomically with owner-only permissions on
//! Unix. Factored out of `device.rs`, which had this logic first (slice
//! 3.1c), so `access.rs` doesn't duplicate it.

use std::io;
use std::path::Path;

/// Writes `bytes` to `path` atomically: a temporary file next to `path`,
/// then `rename` over it, so a crash or power loss mid-write never leaves a
/// half-written file behind. Mode `0600` on Unix -- set when the file is
/// *created*, not after it already holds the secret: writing first and
/// `set_permissions` after would leave a window in which the temporary file
/// is world-readable (umask), and it holds exactly the bytes this mode is
/// meant to protect. On Windows there's no POSIX mode bit to set, but none
/// is needed either -- see `device.rs`'s module doc comment for why. The
/// parent directory is created if it doesn't exist yet.
///
/// A second mode exists ([`write_atomic_mode`]) for the machine data
/// directory of macOS full mode (D51), shared by root and an administrator:
/// there `0660` is needed, otherwise a file written by one of them could not
/// be read by the other.
pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_mode(path, bytes, 0o600)
}

/// Same algorithm as [`write_private_atomic`] with an explicit Unix `mode`
/// (ignored on other platforms). The mode is applied to the temporary file
/// with `fchmod` right after creation, before any bytes are written, so a
/// `umask` cannot strip bits (e.g. group-write of `0660`).
pub(crate) fn write_atomic_mode(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = path.with_file_name(tmp_name);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    {
        use std::io::Write as _;
        let mut file = options.open(&tmp_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        file.write_all(bytes)?;
        file.sync_all()?;
    }

    std::fs::rename(&tmp_path, path)
}
