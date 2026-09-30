# Drive redirection

The drives a client shares, such as mstsc's "Local Resources > Drives" or FreeRDP's `/drive`, are
mounted on the Mac for the length of the session, at `~/RDP Drives/<drive> on <client>`, for example
`~/RDP Drives/C on DESKTOP-01`. They appear in Finder as network volumes, to read and write. ADR-0003
records why they are mounted there and not in /Volumes, and the order of the work.

State on 2026-09-29: reading and writing work with FreeRDP (steps 2 and 3 of ADR-0003). mstsc and
Windows App are still to be tested (step 4).

## How it works

- **Protocol.** The RDPDR channel is IronRDP's own server side (`ironrdp_rdpdr::server::RdpdrServer`),
  with six small changes on branch `rdpmac/nla` of the fork, to be offered upstream. The server sends
  User Logged On after the capability exchange, without which clients announce no drives. It tells
  the backend each request's completion ID, each device's type and the result of a set-information
  request, and it names four more NTSTATUS codes. Drive traffic goes after sound and video in each
  batch of outgoing messages. rdpmac's backend (`crates/rdpmac-session/src/drives/rdpdr.rs`) accepts
  only drives and declines other devices. It opens, lists, queries, reads, writes, truncates, sets
  the times of, renames and deletes the client's files, and each request gives up after 30 seconds.
- **File system.** For each drive rdpmacd runs an NFSv3 server (the nfsserve crate) on a loopback
  port and mounts it with `mount_nfs`, as the user it runs as
  (`crates/rdpmac-session/src/drives`). NFS calls become RDPDR requests:

  | NFS call | Round trips to the client |
  |---|---|
  | Listing a folder | 2, plus 1 per entry: the client returns one entry per request |
  | Looking up a name | 4 (open, two queries, close), whatever the size of the folder |
  | Attributes | the same 4, at most once a second per file |
  | Reading, writing | 1 per 256 KiB, plus an open the first time: open files are kept open, up to 16 |
  | Creating, truncating, setting times, deleting, renaming | 3 (open, the change, close), plus a close of any handle kept open on the file |

  A folder is listed once per enumeration, and every page macOS asks for comes from that listing.
  Attributes carry the client's real times and sizes, which is how macOS notices a file changed on
  the client. A renamed file or folder keeps its NFS file ID, so a program that writes a temporary
  file and renames it over the original goes on using the handle it holds.
- **Mount options.** `locallocks,nfc,vers=3,tcp,inet,rsize=262144,wsize=262144,readahead=4,actimeo=5,intr,deadtimeout=30`.
  `locallocks`: without locks, SQLite reports a disk I/O error and flock fails. `nfc`: names reach
  the client in composed form, as Windows writes them. `inet`: the server listens on 127.0.0.1
  only, and the mount would otherwise also try ::1, where another process could listen. The small
  transfer size keeps copies from holding up the picture and the sound. `deadtimeout`: if rdpmacd
  stops answering, the mount goes away by itself instead of hanging the programs that use it.
- **Server name.** Drives are mounted from `RDP Volume.localhost:/`, which Finder shows as their
  server in its sidebar. Every name in the `.localhost` domain resolves to the loopback address
  without a change to the system; a name without the suffix would need an entry in /etc/hosts.
- **Who can reach a drive.** `~/RDP Drives` has mode 700; the home folder alone would let every
  account of the Mac in, since they all belong to its group. Files are reported as the user's, 700 and
  600; modes and owners set on the Mac are accepted and dropped, since Windows drives have neither.
  The NFS server accepts one MOUNT only, puts a random secret into every file handle and refuses
  handles without it: another process that finds the port can neither mount the drive nor read it.
- **macOS's own files.** `._*` (the AppleDouble files macOS keeps extended attributes in on NFS,
  such as the provenance and quarantine it adds to downloads), `.DS_Store`, `.Spotlight-V100`,
  `.fseventsd`, `.Trashes`, `.VolumeIcon.icns`, `.localized`, `Icon\r` and the
  `.metadata_never_index*` names never reach the client. What macOS writes to them is kept in memory
  for the session, up to 64 MiB per drive: extended attributes and Finder's view settings work, and
  last until the client disconnects. Folders with these names are refused. `.TemporaryItems` does
  reach the client: programs save documents on a volume by writing into it and renaming the result
  into place.
- **Life cycle.** A drive is mounted 3 seconds after the client announces it, so that it does not
  compete with setting up the display. It is unmounted when the client stops sharing it, and when the
  connection ends: first normally, then forcibly if a program still has a file open. If the user
  ejects it in Finder, its server stops within 3 seconds. When rdpmacd starts, it unmounts the drives
  an rdpmacd that stopped without cleaning up left in `~/RDP Drives`.
- **Setting.** `drives = false` in config.toml, or `--no-drives`, turns the channel off. The client
  decides which drives to share.

## Measured

2026-09-29, macOS 27 (26A428), Apple M4, debug build of rdpmacd with `--test-pattern`, FreeRDP's
`sfreerdp` 3.32.0-dev on the same Mac sharing a folder with `/drive:Share,<folder>`:

| Check | Result |
|---|---|
| Time from connecting to the mount | 3.3 s (3 s of it the deliberate delay) |
| `~/RDP Drives` mode; files' owner and modes | 700; the user, 600 and 700 |
| Modification time and size of a file | equal to the client's |
| A 5 MiB file read from the drive | SHA-256 equal, 0.05 s |
| A 5 MiB file copied onto the drive | SHA-256 equal, 0.28 s |
| A folder of 300 files | listed in 0.06 s |
| Names with accents and Chinese characters | shown as on the client |
| New file, append, truncate, new folder, move, delete | done on the client |
| `touch -t 202001011200` | the client's file dated 2020-01-01 12:00 |
| Writing a temporary file and renaming it over the original | the original replaced |
| SQLite: create a table, insert, select | works, the database on the client |
| An extended attribute; a `.DS_Store` | read back on the Mac; neither on the client |
| `._*` or `.DS_Store` files on the client | none |
| Finder | a browsable NFS volume named "Share on E2E-CLIENT" |
| Client disconnects | unmounted at once, folder removed, nothing left mounted |

The same checks gave the same results after the switch to upstream IronRDP's RDPDR server later that
day: mounted after 3.3 s, a 5 MiB copy in 0.31 s, 300 files listed in 0.07 s.

## Clients

- **FreeRDP 3.32.0-dev** (commit fd769f89f of 2026-09-20 and later): `drive_file_read` passes the
  file's structure to `GetFileSize` instead of its handle, so every read of a redirected drive fails,
  with any server. The tests above used a build with that line corrected
  (`GetFileSize(file->file_handle, ...)`).
- **FreeRDP** answers a read that starts past the end of a file with STATUS_UNSUCCESSFUL, where
  Windows answers STATUS_END_OF_FILE. macOS reads whole 256 KiB blocks, in parts, even of a short
  file, so the parts past the end are answered on the Mac and never sent.
- **FreeRDP** cannot set a folder's times: it keeps no handle to a folder. The failure is logged and
  otherwise ignored, since reporting it would tell macOS the folder is gone.
- **FreeRDP** sends a drive's full name in DeviceData as 8-bit characters, where the specification
  asks for UTF-16, which IronRDP's client sends; both are read. FreeRDP cuts PreferredDosName to
  eight characters, so DeviceData wins when present.
- **FreeRDP** answers a query about a file ID it does not know without the Length field. IronRDP's
  server cannot decode that answer and ends the connection, so rdpmac queries only handles it has
  just opened.
- **mstsc** refuses to open a file whose share mode has bits other than read, write and delete, with
  STATUS_INVALID_PARAMETER, which macOS shows as error -50. Folders still open, so the drive lists
  but no file opens, reads or copies. IronRDP's server set all 32 bits (`SharedAccess::all()` of a
  type that keeps unknown bits) until the fork fixed it on 2026-09-29, the day mstsc found it.
- **mstsc and Windows App**: otherwise not yet tested with rdpmac. macrdp found that mstsc needs the
  Client ID Confirm together with the capability request, and SYNCHRONIZE in the access rights of a
  file it reads; rdpmac does both. Writing to the root of `C:` or to `$Recycle.Bin` from an ordinary
  mstsc session is refused by Windows itself.

## Testing

```sh
cargo test -p rdpmac-session drives                # the file system against a drive in memory, the requests against IronRDP's channel
cargo test -p rdpmac-session -- --ignored drives    # the same drive mounted through macOS's NFS client
cargo test -p ironrdp-testsuite-core --test integration_tests_core rdpdr   # in ../IronRDP: the channel and the PDUs
```

End to end on one Mac: run rdpmacd with `--test-pattern`, then connect FreeRDP with
`/drive:Share,<folder>`; the drive appears in `~/RDP Drives` about three seconds later.
