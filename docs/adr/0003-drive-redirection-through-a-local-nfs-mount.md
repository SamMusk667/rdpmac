# ADR-0003: Drive redirection through a local NFS mount

| | |
|---|---|
| Status | Accepted (2026-09-29) |
| Relates to | ADR-0002 (the licence that lets rdpmac take macrdp's code) |

## Context

RDP clients can share their drives with the server: the RDPDR static channel with its file system
extension (MS-RDPEFS). A Windows server shows each one as "C on DESKTOP-01". The drive never moves
as a whole. Every open, read, write and directory listing is a request that the server sends to the
client over the RDP connection, so a Mac server needs a file system that turns Finder's calls into
those requests.

rdpmac has no drive redirection. Nor does its IronRDP fork have the server side of RDPDR:
ironrdp-rdpdr implements only the client, and ironrdp-server has no RDPDR code at all (section 1 of
ADR-0001 counted drive redirection among its channels by mistake). clintcan/macrdp (MIT OR Apache-2.0) has both: the server
side of RDPDR in its vendored IronRDP, and for each drive an in-process NFSv3 server (the nfsserve
crate) that the Mac's own `mount_nfs` mounts. It has been verified reading and writing with FreeRDP
and mstsc.

macrdp tries `/Volumes/<label>` first and always falls back to `$TMPDIR`, because /Volumes belongs to
root. The other ways into /Volumes were checked on 2026-09-28 (macOS 27, 26A428):

- NetFS, which Finder's Connect to Server uses, mounts into /Volumes without root through
  NetAuthSysAgent and automountd. But it drops the port of an `nfs://` URL (it ran
  `mount_nfs -odeadtimeout=45 127.0.0.1:/ /Volumes/127.0.0.1`), so it cannot reach a server on a
  loopback port. Its `http://` (WebDAV) path failed with error -6600, and macOS's WebDAV client
  downloads a whole file before opening it.
- A privileged helper could create `/Volumes/<label>` for the user, at the cost of a root component,
  an administrator's approval at install and another surface to secure.
- FSKit mounts only under /Volumes and needs no root. But a file system that is not backed by a disk
  needs macOS 26, the user has to enable the extension in System Settings, and its I/O is slower than
  the kernel's (macFUSE's documentation).

The same day, throwaway servers on that Mac showed:

- A user can mount NFS onto a folder of their own in the home folder. macOS lists it as a browsable
  network volume named after the folder (spaces work), Spotlight does not index it, and
  `diskutil unmount`, which Finder's Eject uses, unmounts it.
- The home folder is 750 with the group staff, and every local account belongs to staff.
- nfsserve, as macrdp uses it, answers anyone who reaches its port: an unprivileged process read a
  file over raw RPC without mounting anything. Requiring a reserved source port does not help,
  because an unprivileged process can bind 0.0.0.0:999 and connect from port 999.
- After the server was killed, a mount with `intr,deadtimeout=10` blocked for about 18 seconds,
  failed with ENXIO and removed itself. Without deadtimeout, as in macrdp, the mount stays until
  something unmounts it.
- With `nolocks`, as in macrdp, SQLite reported a disk I/O error and flock failed with ENOTSUP. With
  `locallocks` both worked.
- NFSv3 has no extended attributes, so macOS stores them in `._` files. Creating one file sent both
  `new.txt` and `._new.txt` to the server; the second held the com.apple.provenance attribute that
  macOS adds by itself. Right after mounting, macOS looked up `.DS_Store`, `.Spotlight-V100`, `._.`
  and four other names that did not exist.
- With `nfc`, a name written in decomposed form (NFD) reached the server in composed form (NFC), the
  form Windows uses.

## Decision

1. Each drive that a client shares is mounted, for the length of the session, at
   `~/RDP Drives/<drive> on <client>`, such as `~/RDP Drives/C on DESKTOP-01`, and not in /Volumes.
   `~/RDP Drives` has mode 700. rdpmacd mounts as the user it runs as, and nothing runs as root.
2. The mount is an in-process NFSv3 server for each drive, bound to 127.0.0.1 on a port the system
   chooses. It is built on nfsserve and on macrdp's code, which ADR-0002 lets rdpmac take with its
   notices. It is mounted with
   `locallocks,nfc,vers=3,tcp,rsize=262144,wsize=262144,readahead=4,actimeo=5,intr,deadtimeout=30`;
   the deadtimeout is to be tuned on a slow link.
3. The NFS server accepts one MOUNT only. It puts a random 16-byte secret into every file handle and
   refuses handles without it, and it reports files as owned by the user, with modes 700 and 600.
   These are overrides of nfsserve's `path_to_id`, `id_to_fh` and `fh_to_id`, not changes to the
   crate. If the mount fails, a server with a new port and secret takes its place.
4. macOS's own files (`._*`, `.DS_Store`, `.Spotlight-V100`, `.fseventsd`, `.Trashes`,
   `.metadata_never_index*`) never reach the client. Lookups for them are answered on the Mac, and
   what macOS writes to them is kept on the Mac for the session. `.TemporaryItems` does reach the
   client, because programs save documents on a volume by writing into it and renaming the result
   into place (corrected during step 3, 2026-09-29).
5. The server side of RDPDR goes into the IronRDP fork as commits of its own, to be offered upstream:
   - in ironrdp-rdpdr, the PDUs from server to client;
   - in ironrdp-server, a processor, a handle whose requests each have a timeout, a builder method
     and a server event;
   - RDPDR sent after audio and video in each batch, so that file transfers do not drop sound.
6. Drives are named from the client's DeviceData, falling back to PreferredDosName, which FreeRDP
   cuts to eight characters. Devices other than drives are declined.
7. A drive is mounted a few seconds after the session is up, and unmounted:
   - when the client removes it;
   - when the session ends, forcibly if a file is still open;
   - when rdpmacd starts, for mounts that a crash left behind.

   If the user ejects a drive in Finder, its server stops.
8. The setting `drives` (on by default) and `--no-drives` turn the channel off. The client decides
   which drives to share, as it does with a Windows server.
9. The work goes in four steps, each tested before the next:
   1. Protocol: the IronRDP changes, with a unit test for every PDU.
   2. Read-only drives: the hardened NFS server, the mounts and their life cycle, and the answers for
      macOS's names. Tested with FreeRDP on one Mac, then with mstsc.
   3. Writing: create, write, truncate, rename and delete, with macrdp's cache of open handles, and
      macOS's files kept on the Mac.
   4. Finishing: the setting in the app, docs/drives.md and the README, mstsc and Windows App,
      timeouts tuned on a slow link, and privacy prompts checked.

Alternatives were /Volumes through a privileged helper, FSKit, and NetFS with NFS or WebDAV, for the
reasons above, and SMB through NetFS, which would need an SMB server written for rdpmac.

## Consequences

- Drives appear in Finder and at a fixed path, but not in /Volumes. Scripts and apps that look only
  in /Volumes do not find them.
- Other accounts on the Mac cannot reach a drive, through the mount or through the port. Any process
  of the user can, as with any mounted volume.
- Extended attributes, Finder tags and quarantine included, last only for the session, and file
  locks are held on the Mac only. The client's disk gets no `._` files.
- Every file operation is one or more round trips over the RDP connection, which it shares with the
  picture and the sound, so large copies are slower than on a local disk.
- If rdpmacd crashes, programs using a drive can stall for up to about 40 seconds (the deadtimeout
  plus the time macOS takes to notice) before the mount goes away. rdpmacd removes what is left when
  it starts again.
- Apps may ask once for permission to access files on a network volume, as they do for other network
  volumes. Step 4 confirms this.
- The IronRDP fork carries about 460 more lines of patches, in commits of their own so that they can
  go upstream.
