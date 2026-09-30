//! Drive redirection (ADR-0003): the drives a client shares are mounted, for the length of the
//! session, at `~/RDP Drives/<drive> on <client>`.
//!
//! [`DriveFactory`] gives IronRDP's RDPDR channel a [`DriveBackend`] for each connection. When the
//! client announces a drive, the backend waits [`MOUNT_DELAY`], starts an NFSv3 server for it on a
//! loopback port ([`fs::DriveFs`], which turns NFS calls into RDPDR requests through
//! [`rdpdr::RdpdrHandle`]) and mounts it as the user rdpmacd runs as. It unmounts the drive when the
//! client stops sharing it or the connection ends, and stops serving it when the user ejects it in
//! Finder. [`reap_stale_mounts`] removes what an rdpmacd that did not stop cleanly left mounted.

mod fs;
mod mount;
mod rdpdr;

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use ironrdp_core::impl_as_any;
use ironrdp_pdu::PduResult;
use ironrdp_rdpdr::pdu::efs::{
    ClientDriveLockControlResponse, ClientDriveNotifyChangeDirectoryResponse, ClientDriveQueryDirectoryResponse,
    ClientDriveQueryInformationResponse, ClientDriveQuerySecurityResponse, ClientDriveQueryVolumeInformationResponse,
    ClientDriveSetInformationResponse, ClientDriveSetSecurityResponse, DeviceAnnounceHeader, DeviceCloseResponse,
    DeviceControlResponse, DeviceCreateResponse, DeviceFlushBuffersResponse, DeviceReadResponse, DeviceType,
    DeviceWriteResponse,
};
use ironrdp_server::{RdpdrServerBackend, RdpdrServerFactory, ServerEvent, ServerEventSender};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tracing::{debug, info, warn};

use self::fs::{ClientDrive, DriveFs};
pub use self::mount::{default_root, reap_stale_mounts};
use self::rdpdr::{Answer, RdpdrHandle, Requests};

/// How long after the client announces a drive it is mounted. Clients announce their drives while
/// the session starts, and a volume appearing then only competes with setting up the display.
const MOUNT_DELAY: Duration = Duration::from_secs(3);

/// How often a mounted drive is checked for having been ejected.
const EJECT_POLL: Duration = Duration::from_secs(3);

/// Builds a [`DriveBackend`] for each connection, mounting drives in `root`.
pub struct DriveFactory {
    root: PathBuf,
    /// The server's events, which carry the backends' requests to the connection.
    sender: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl DriveFactory {
    pub fn new(root: PathBuf) -> Self {
        Self { root, sender: None }
    }
}

impl ServerEventSender for DriveFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.sender = Some(sender);
    }
}

impl RdpdrServerFactory for DriveFactory {
    fn build_backend(&self) -> Box<dyn RdpdrServerBackend> {
        if self.sender.is_none() {
            warn!("the RDPDR channel started before the server gave it its events; drives cannot be read");
        }
        Box::new(DriveBackend::new(self.root.clone(), self.sender.clone()))
    }
}

/// The drives of one connection. Dropping it, when the connection ends, unmounts them.
pub struct DriveBackend {
    root: PathBuf,
    requests: Requests,
    handle: RdpdrHandle,
    client_name: Option<String>,
    drives: HashMap<u32, Drive>,
}

impl_as_any!(DriveBackend);

impl fmt::Debug for DriveBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriveBackend")
            .field("root", &self.root)
            .field("client_name", &self.client_name)
            .field("drives", &self.drives.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// A drive the client shares: the task that mounts it and then watches for an eject, and what has
/// to be undone once it is mounted or being mounted.
struct Drive {
    task: JoinHandle<()>,
    serving: Arc<Mutex<Option<Serving>>>,
}

struct Serving {
    mountpoint: PathBuf,
    server: AbortHandle,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl DriveBackend {
    fn new(root: PathBuf, sender: Option<mpsc::UnboundedSender<ServerEvent>>) -> Self {
        let requests = Requests::new(sender);
        Self {
            root,
            handle: RdpdrHandle::new(requests.clone()),
            requests,
            client_name: None,
            drives: HashMap::new(),
        }
    }

    /// Starts the task that mounts a drive the client shares, unless it is mounted already: clients
    /// may announce a drive again.
    fn mount(&mut self, device: &DeviceAnnounceHeader) {
        let device_id = device.device_id();
        if self.drives.contains_key(&device_id) {
            return;
        }
        let name = rdpdr::drive_name(device);
        info!(device_id, %name, "the client shares a drive");
        let folder = mount::folder_name(&name, self.client_name.as_deref());
        let serving = Arc::new(Mutex::new(None));
        let remote = ClientDrive {
            handle: self.handle.clone(),
            device_id,
        };
        let task = tokio::spawn(serve(self.root.clone(), folder, remote, Arc::clone(&serving)));
        self.drives.insert(device_id, Drive { task, serving });
    }
}

impl RdpdrServerBackend for DriveBackend {
    fn on_device_announce(&mut self, devices: &[DeviceAnnounceHeader]) -> Vec<(u32, bool)> {
        let mut decisions = Vec::with_capacity(devices.len());
        for device in devices {
            let accepted = device.device_type() == DeviceType::Filesystem;
            if accepted {
                self.mount(device);
            } else {
                debug!(
                    device_id = device.device_id(),
                    device_type = ?device.device_type(),
                    "declining a device that is not a drive"
                );
            }
            decisions.push((device.device_id(), accepted));
        }
        decisions
    }

    fn on_device_remove(&mut self, device_ids: &[u32]) {
        info!(?device_ids, "the client stops sharing devices");
        for device_id in device_ids {
            // The client's handles went with the drive.
            self.handle.forget_device(*device_id);
            if let Some(drive) = self.drives.remove(device_id) {
                stop(drive);
            }
        }
    }

    fn on_client_name(&mut self, computer_name: &str) {
        self.client_name = Some(computer_name.to_owned());
    }

    fn on_request_sent(&mut self, completion_id: u32) {
        self.requests.sent(completion_id);
    }

    fn on_create_complete(&mut self, response: &DeviceCreateResponse) -> PduResult<()> {
        let answer = Answer::Create {
            file_id: response.file_id,
        };
        self.requests.complete(&response.device_io_reply, answer);
        Ok(())
    }

    fn on_close_complete(&mut self, response: &DeviceCloseResponse) -> PduResult<()> {
        self.requests.complete(&response.device_io_response, Answer::Close);
        Ok(())
    }

    fn on_read_complete(&mut self, response: &DeviceReadResponse) -> PduResult<()> {
        let answer = Answer::Read {
            data: response.read_data.clone(),
        };
        self.requests.complete(&response.device_io_reply, answer);
        Ok(())
    }

    fn on_write_complete(&mut self, response: &DeviceWriteResponse) -> PduResult<()> {
        let answer = Answer::Write {
            length: response.length,
        };
        self.requests.complete(&response.device_io_reply, answer);
        Ok(())
    }

    fn on_query_information_complete(&mut self, response: &ClientDriveQueryInformationResponse) -> PduResult<()> {
        let answer = Answer::QueryInformation {
            buffer: response.buffer.clone(),
        };
        self.requests.complete(&response.device_io_response, answer);
        Ok(())
    }

    fn on_set_information_complete(&mut self, response: &ClientDriveSetInformationResponse) -> PduResult<()> {
        self.requests.complete(response.device_io_reply(), Answer::SetInformation);
        Ok(())
    }

    fn on_query_directory_complete(&mut self, response: &ClientDriveQueryDirectoryResponse) -> PduResult<()> {
        let answer = Answer::QueryDirectory {
            buffer: response.buffer.clone(),
        };
        self.requests.complete(&response.device_io_reply, answer);
        Ok(())
    }

    // Requests the drives never send.

    fn on_flush_buffers_complete(&mut self, _response: &DeviceFlushBuffersResponse) -> PduResult<()> {
        Ok(())
    }

    fn on_device_control_complete(&mut self, _response: &DeviceControlResponse) -> PduResult<()> {
        Ok(())
    }

    fn on_notify_change_directory_complete(
        &mut self,
        _response: &ClientDriveNotifyChangeDirectoryResponse,
    ) -> PduResult<()> {
        Ok(())
    }

    fn on_query_volume_information_complete(
        &mut self,
        _response: &ClientDriveQueryVolumeInformationResponse,
    ) -> PduResult<()> {
        Ok(())
    }

    fn on_lock_control_complete(&mut self, _response: &ClientDriveLockControlResponse) -> PduResult<()> {
        Ok(())
    }

    fn on_query_security_complete(&mut self, _response: &ClientDriveQuerySecurityResponse) -> PduResult<()> {
        Ok(())
    }

    fn on_set_security_complete(&mut self, _response: &ClientDriveSetSecurityResponse) -> PduResult<()> {
        Ok(())
    }
}

impl Drop for DriveBackend {
    fn drop(&mut self) {
        // The connection is over: whatever waits for the client never gets an answer, and the NFS
        // calls that unmounting brings fail at once instead of waiting for the timeout.
        self.requests.close();
        for (_, drive) in self.drives.drain() {
            stop(drive);
        }
    }
}

/// Stops mounting or watching a drive, unmounts it and stops its server.
fn stop(drive: Drive) {
    drive.task.abort();
    if let Some(serving) = lock(&drive.serving).take() {
        // Unmounting blocks until the server has answered, and the server runs on this thread: unmount
        // on another one, and stop the server once that is done.
        std::thread::spawn(move || {
            mount::unmount(&serving.mountpoint);
            serving.server.abort();
            info!(mountpoint = %serving.mountpoint.display(), "drive unmounted");
        });
    }
}

/// Mounts a drive after [`MOUNT_DELAY`], then watches for the user ejecting it.
async fn serve(root: PathBuf, folder: String, remote: ClientDrive, serving: Arc<Mutex<Option<Serving>>>) {
    tokio::time::sleep(MOUNT_DELAY).await;
    let mountpoint = match mount::prepare(&root, &folder) {
        Ok(mountpoint) => mountpoint,
        Err(error) => {
            warn!(%error, folder, "could not make a folder for a drive");
            return;
        }
    };
    // SAFETY: getuid and getgid cannot fail and touch no memory.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let fsid = u64::from(remote.device_id);

    // A mount fails if another process sent the server a MOUNT first, which the server accepts only
    // once. A new server, on a new port with a new secret, gets past that.
    let mut mounted = false;
    for attempt in 1..=2 {
        let drive = DriveFs::new(remote.clone(), uid, gid, fsid);
        let listener = match NFSTcpListener::bind("127.0.0.1:0", drive).await {
            Ok(listener) => listener,
            Err(error) => {
                warn!(%error, "could not start the NFS server of a drive");
                break;
            }
        };
        let port = listener.get_listen_port();
        let server = tokio::spawn(async move {
            if let Err(error) = listener.handle_forever().await {
                warn!(%error, "NFS server of a drive stopped");
            }
        });
        *lock(&serving) = Some(Serving {
            mountpoint: mountpoint.clone(),
            server: server.abort_handle(),
        });
        let target = mountpoint.clone();
        match tokio::task::spawn_blocking(move || mount::mount(port, &target, false)).await {
            Ok(Ok(())) => {
                mounted = true;
                break;
            }
            Ok(Err(error)) => warn!(%error, attempt, mountpoint = %mountpoint.display(), "could not mount a drive"),
            Err(error) => warn!(%error, "the mount of a drive did not finish"),
        }
        server.abort();
        *lock(&serving) = None;
    }
    if !mounted {
        let _ = std::fs::remove_dir(&mountpoint);
        return;
    }
    info!(mountpoint = %mountpoint.display(), "drive mounted");

    loop {
        tokio::time::sleep(EJECT_POLL).await;
        if !mount::is_mounted(&mountpoint) {
            info!(mountpoint = %mountpoint.display(), "drive ejected");
            if let Some(serving) = lock(&serving).take() {
                serving.server.abort();
            }
            let _ = std::fs::remove_dir(&mountpoint);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fs::tests::FakeDrive;
    use super::*;

    /// Mounts a drive held in memory through the Mac's NFS client, as a session does, reads and
    /// writes it, and checks that extended attributes stay on the Mac and that a second mount of the
    /// server is refused. It mounts a file system, so it is ignored by default:
    /// `cargo test -p rdpmac-session -- --ignored drives`.
    #[tokio::test]
    #[ignore = "mounts an NFS file system on this Mac"]
    async fn a_drive_mounts_reads_and_unmounts() {
        let drive = FakeDrive::with(&[
            ("\\hello.txt", Some(b"hello")),
            ("\\docs", None),
            ("\\docs\\a.txt", Some(b"a")),
        ]);
        let root = std::env::temp_dir().join(format!("rdpmac-drive-mount-test-{}", std::process::id()));
        let mountpoint = mount::prepare(&root, "Fake on TEST").unwrap();
        // SAFETY: getuid and getgid cannot fail and touch no memory.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let listener = NFSTcpListener::bind("127.0.0.1:0", DriveFs::new(Arc::clone(&drive), uid, gid, 1))
            .await
            .unwrap();
        let port = listener.get_listen_port();
        let server = tokio::spawn(async move { listener.handle_forever().await });
        let target = mountpoint.clone();
        tokio::task::spawn_blocking(move || mount::mount(port, &target, false))
            .await
            .unwrap()
            .unwrap();
        assert!(mount::is_mounted(&mountpoint));
        // Finder shows the drives' server by this name.
        let source = mount::mounts().into_iter().find(|m| m.on == mountpoint).unwrap().from;
        assert_eq!(source, "RDP Volume.localhost:/");

        // File operations on the mount block until the server answers, and it runs on this thread.
        let mp = mountpoint.clone();
        let (text, names, attribute) = tokio::task::spawn_blocking(move || {
            let text = std::fs::read_to_string(mp.join("hello.txt")).unwrap();
            let mut names: Vec<String> = std::fs::read_dir(&mp)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            std::fs::write(mp.join("new.txt"), b"written on the Mac").unwrap();
            // An extended attribute, which macOS keeps in an AppleDouble file on NFS.
            let status = std::process::Command::new("/usr/bin/xattr")
                .args(["-w", "com.example.test", "kept"])
                .arg(mp.join("new.txt"))
                .status()
                .unwrap();
            assert!(status.success());
            let attribute = std::process::Command::new("/usr/bin/xattr")
                .args(["-p", "com.example.test"])
                .arg(mp.join("new.txt"))
                .output()
                .unwrap()
                .stdout;
            std::fs::rename(mp.join("new.txt"), mp.join("docs/moved.txt")).unwrap();
            (text, names, attribute)
        })
        .await
        .unwrap();
        assert_eq!(text, "hello");
        assert_eq!(names, ["docs", "hello.txt"]);
        assert_eq!(String::from_utf8_lossy(&attribute).trim(), "kept");
        assert_eq!(
            drive.contents("\\docs\\moved.txt").as_deref(),
            Some(&b"written on the Mac"[..])
        );
        assert!(
            !drive.paths().iter().any(|p| p.contains("._")),
            "no AppleDouble file reached the drive: {:?}",
            drive.paths()
        );

        // Only the first MOUNT is accepted, so nobody else can mount the drive.
        let second = mount::prepare(&root, "Fake on TEST").unwrap();
        assert_eq!(
            second,
            std::fs::canonicalize(&root).unwrap().join("Fake on TEST 2"),
            "the mounted folder is not reused"
        );
        let target = second.clone();
        assert!(tokio::task::spawn_blocking(move || mount::mount(port, &target, true))
            .await
            .unwrap()
            .is_err());
        let _ = std::fs::remove_dir(&second);

        let target = mountpoint.clone();
        tokio::task::spawn_blocking(move || mount::unmount(&target)).await.unwrap();
        assert!(!mount::is_mounted(&mountpoint));
        assert!(!mountpoint.exists(), "the folder goes with the mount");
        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }
}
