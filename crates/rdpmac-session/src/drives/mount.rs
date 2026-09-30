//! Where redirected drives are mounted, and mounting and unmounting them with the Mac's own tools.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::{debug, info, warn};

/// The NFS options of every drive (ADR-0003):
///
/// - `locallocks`: this Mac is the only client of the server, so locks held here are all the locks
///   there are. Without locks SQLite reports a disk I/O error and flock fails with ENOTSUP.
/// - `nfc`: names reach the client in composed form, as Windows writes them.
/// - `rsize`, `wsize` and `readahead` are small so that a copy does not hold up the picture and the
///   sound, which share the RDP connection with it.
/// - `intr,deadtimeout=30`: when rdpmacd stops answering, programs using the drive get an error and
///   the mount goes away, instead of both hanging.
/// - `inet`: the server listens on 127.0.0.1 only. [`SERVER`] also resolves to ::1, where another
///   process could listen on the same port and be tried when the mount reconnects.
const OPTIONS: &str =
    "locallocks,nfc,vers=3,tcp,inet,rsize=262144,wsize=262144,readahead=4,actimeo=5,intr,deadtimeout=30";

/// The server every drive is mounted from, which Finder shows as the drives' server in its sidebar.
/// Every name in the `.localhost` domain resolves to the loopback address (RFC 6761) without a
/// change to the system; plain "localhost" said nothing about what the volumes are.
const SERVER: &str = "RDP Volume.localhost";

/// Servers the drives of earlier versions were mounted from, for [`reap_stale_mounts`].
const OLD_SERVERS: [&str; 1] = ["localhost"];

/// The folder the drives are mounted in: `~/RDP Drives`.
pub fn default_root(home: &Path) -> PathBuf {
    home.join("RDP Drives")
}

/// The folder name of a drive, "C on DESKTOP-01", or just the drive's name when the client gave none.
pub(crate) fn folder_name(drive: &str, client: Option<&str>) -> String {
    let drive = drive.trim();
    let name = match client.map(str::trim).filter(|c| !c.is_empty()) {
        Some(client) => format!("{drive} on {client}"),
        None => drive.to_owned(),
    };
    sanitise(&name)
}

/// A name that is one path component: no slash, no colon (which Finder shows as a slash), no
/// control characters, no leading dot (which would hide the folder), at most 200 bytes.
fn sanitise(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == ':' || c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').trim();
    let mut out = String::new();
    for c in cleaned.chars() {
        if out.len() + c.len_utf8() > 200 {
            break;
        }
        out.push(c);
    }
    if out.trim().is_empty() {
        "Drive".to_owned()
    } else {
        out.trim_end().to_owned()
    }
}

/// Makes `root` with mode 700, and in it an empty folder for a drive: `name`, or `name 2` and so on
/// when a drive of this session or another is mounted there.
pub(crate) fn prepare(root: &Path, name: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    // The home folder is 750 with the group staff, which every account of the Mac belongs to, so
    // this folder is what keeps other accounts away from the drives.
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
    // The mount table holds paths with their symbolic links resolved (/private/var for /var), so
    // mount points are made from the resolved path, for comparisons with it to hold.
    let root = std::fs::canonicalize(root)?;
    let mounted = mounts();
    for n in 1..100 {
        let candidate = if n == 1 {
            root.join(name)
        } else {
            root.join(format!("{name} {n}"))
        };
        if mounted.iter().any(|m| m.on == candidate) {
            continue;
        }
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            // An empty folder left by an earlier session is reused, so that a drive keeps its path.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if std::fs::read_dir(&candidate).is_ok_and(|mut entries| entries.next().is_none()) {
                    return Ok(candidate);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free folder name"))
}

/// Mounts the NFS server listening on `port` at `mountpoint`. mount_nfs blocks until the server has
/// answered, and the server runs on the async runtime, so this has to run off it.
pub(crate) fn mount(port: u16, mountpoint: &Path, read_only: bool) -> io::Result<()> {
    let mut options = format!("{OPTIONS},port={port},mountport={port}");
    if read_only {
        options.push_str(",rdonly");
    }
    let output = Command::new("/sbin/mount_nfs")
        .arg("-o")
        .arg(&options)
        .arg(format!("{SERVER}:/"))
        .arg(mountpoint)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "mount_nfs failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

/// Unmounts `mountpoint`, forcibly if a program still has a file open there, and removes the folder.
/// Blocks; the server has to go on answering until it returns.
pub(crate) fn unmount(mountpoint: &Path) {
    let unmounted = Command::new("/sbin/umount")
        .arg(mountpoint)
        .status()
        .is_ok_and(|s| s.success());
    if !unmounted {
        debug!(mountpoint = %mountpoint.display(), "drive busy; unmounting it forcibly");
        let forced = Command::new("/sbin/umount")
            .arg("-f")
            .arg(mountpoint)
            .status()
            .is_ok_and(|s| s.success());
        if !forced && is_mounted(mountpoint) {
            warn!(mountpoint = %mountpoint.display(), "could not unmount a drive");
            return;
        }
    }
    let _ = std::fs::remove_dir(mountpoint);
}

/// A file system mounted on this Mac.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Mount {
    pub(crate) from: String,
    pub(crate) on: PathBuf,
    pub(crate) fs_type: String,
}

/// The file systems mounted on this Mac, from the kernel's table. MNT_NOWAIT answers from what the
/// kernel knows, without asking a file system that may not answer, such as an NFS mount whose server
/// has gone.
pub(crate) fn mounts() -> Vec<Mount> {
    // SAFETY: a null buffer asks only for the number of mounts.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    // Room for a few mounts more, in case some appear meanwhile.
    let mut table: Vec<libc::statfs> = Vec::with_capacity(count + 8);
    let Ok(bytes) = libc::c_int::try_from(table.capacity() * std::mem::size_of::<libc::statfs>()) else {
        return Vec::new();
    };
    // SAFETY: `table` has room for `bytes` bytes of statfs records, and getfsstat writes at most that
    // many and returns how many records it wrote.
    let written = unsafe { libc::getfsstat(table.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    let Ok(written) = usize::try_from(written) else {
        return Vec::new();
    };
    // SAFETY: getfsstat initialised the first `written` records, and `written` fits the capacity.
    unsafe { table.set_len(written.min(table.capacity())) };
    table
        .iter()
        .map(|s| Mount {
            from: c_chars(&s.f_mntfromname),
            on: PathBuf::from(c_chars(&s.f_mntonname)),
            fs_type: c_chars(&s.f_fstypename),
        })
        .collect()
}

fn c_chars(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| u8::from_ne_bytes(c.to_ne_bytes()))
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Whether a mount's source is one that rdpmacd mounts drives from, now or in an earlier version.
fn is_drive_source(from: &str) -> bool {
    std::iter::once(SERVER)
        .chain(OLD_SERVERS)
        .any(|server| from.strip_prefix(server).is_some_and(|rest| rest.starts_with(':')))
}

pub(crate) fn is_mounted(path: &Path) -> bool {
    mounts().iter().any(|m| m.on == path)
}

/// Unmounts the drives that an rdpmacd which stopped without cleaning up left mounted in `root`, and
/// removes the empty folders there. Their server went with that process, so only a forced unmount
/// returns. Returns how many drives it unmounted.
pub fn reap_stale_mounts(root: &Path) -> usize {
    let Ok(root) = std::fs::canonicalize(root) else {
        // No folder, so nothing mounted in it.
        return 0;
    };
    let root = root.as_path();
    let mut reaped = 0;
    for stale in mounts() {
        if stale.fs_type == "nfs" && is_drive_source(&stale.from) && stale.on.parent() == Some(root) {
            let forced = Command::new("/sbin/umount")
                .arg("-f")
                .arg(&stale.on)
                .status()
                .is_ok_and(|s| s.success());
            if forced {
                reaped += 1;
            } else {
                warn!(mountpoint = %stale.on.display(), "could not unmount a drive left by an earlier rdpmacd");
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_mounted(&path) {
                // Only removes empty folders.
                let _ = std::fs::remove_dir(&path);
            }
        }
    }
    if reaped > 0 {
        info!(reaped, root = %root.display(), "unmounted drives left by an earlier rdpmacd");
    }
    reaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_are_one_path_component() {
        assert_eq!(folder_name("C", Some("DESKTOP-01")), "C on DESKTOP-01");
        assert_eq!(folder_name("Documents_2026", None), "Documents_2026");
        assert_eq!(folder_name(" D ", Some("  ")), "D");
        assert_eq!(folder_name("a/b:c", Some("pc")), "a_b_c on pc");
        assert_eq!(folder_name("..hidden", None), "hidden");
        assert_eq!(folder_name("", None), "Drive");
        assert!(folder_name(&"x".repeat(300), None).len() <= 200);
    }

    #[test]
    fn prepare_makes_a_private_root_and_a_free_folder() {
        let root = std::env::temp_dir().join(format!("rdpmac-drives-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let first = prepare(&root, "C on PC").unwrap();
        // $TMPDIR is under /var, a link to /private/var.
        let resolved = std::fs::canonicalize(&root).unwrap();
        assert_eq!(first, resolved.join("C on PC"));
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);

        // An empty folder is reused; one with something in it is not.
        assert_eq!(prepare(&root, "C on PC").unwrap(), first);
        std::fs::write(first.join("file"), b"x").unwrap();
        assert_eq!(prepare(&root, "C on PC").unwrap(), resolved.join("C on PC 2"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn drives_of_this_and_earlier_versions_are_recognised() {
        assert!(is_drive_source("RDP Volume.localhost:/"));
        assert!(is_drive_source("localhost:/"));
        assert!(!is_drive_source("localhost.example.com:/export"));
        assert!(!is_drive_source("nas:/export"));
        assert!(!is_drive_source("//user@localhost/share"));
    }

    #[test]
    fn the_server_resolves_to_the_loopback_address() {
        use std::net::ToSocketAddrs;
        let addresses: Vec<_> = (SERVER, 0).to_socket_addrs().unwrap().map(|a| a.ip()).collect();
        assert!(addresses.iter().any(|ip| ip.is_loopback() && ip.is_ipv4()), "{addresses:?}");
        assert!(addresses.iter().all(|ip| ip.is_loopback()), "{addresses:?}");
    }

    #[test]
    fn the_mount_table_has_the_root_file_system() {
        assert!(mounts().iter().any(|m| m.on == Path::new("/")));
    }
}
