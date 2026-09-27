//! Files through the clipboard (MS-RDPECLIP 1.3.2.2.3): a file list (FileGroupDescriptorW) and
//! the files' contents, read piece by piece.
//!
//! [`Outgoing`] offers files copied on the Mac: folders are walked, and the client's File
//! Contents requests are answered from disk. [`Incoming`] fetches files copied on the client into
//! a folder of its own, one request at a time, and hands back the top-level entries to put on the
//! pasteboard once everything has arrived.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor,
};
use tracing::{debug, info, warn};

/// More entries than this in one copy are left out; Explorer copes badly with huge lists anyway.
pub const MAX_ENTRIES: usize = 10_000;
/// The longest name on the wire: relative path, backslash and name (MS-RDPECLIP 2.2.5.2.3.1).
const MAX_WIRE_NAME: usize = 259;
/// How much of a file is asked for at once.
const CHUNK: u32 = 1024 * 1024;
/// A transfer the client stops answering for this long is given up.
pub const STALL: Duration = Duration::from_secs(30);
/// Between 1601-01-01, where Windows file times start, and 1970-01-01, in 100 ns units.
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

fn filetime(time: SystemTime) -> Option<u64> {
    let since = time.duration_since(UNIX_EPOCH).ok()?;
    Some(FILETIME_UNIX_EPOCH + since.as_secs() * 10_000_000 + u64::from(since.subsec_nanos()) / 100)
}

fn system_time(filetime: u64) -> Option<SystemTime> {
    let since = filetime.checked_sub(FILETIME_UNIX_EPOCH)?;
    UNIX_EPOCH.checked_add(Duration::new(since / 10_000_000, (since % 10_000_000) as u32 * 100))
}

/// A Mac name as Windows can create it: characters Windows forbids become `_`, and trailing dots
/// and spaces go. `None` for a name that should not be copied at all.
fn windows_name(name: &str) -> Option<String> {
    if name.starts_with("._") || name == ".DS_Store" {
        return None;
    }
    let mapped: String = name
        .chars()
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c < ' ' { '_' } else { c })
        .collect();
    let trimmed = mapped.trim_end_matches(['.', ' ']);
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Files copied on the Mac, offered to the client.
#[derive(Default)]
pub struct Outgoing {
    /// The paths behind the offered descriptors, in their order.
    paths: Arc<Vec<PathBuf>>,
    /// Lists the client locked, by clipDataId; it may keep reading them after a new copy.
    locked: HashMap<u32, Arc<Vec<PathBuf>>>,
    /// The file read last, kept open for the next piece.
    open: Option<(PathBuf, File)>,
}

impl Outgoing {
    /// Walks `roots` and returns the descriptors to offer, remembering the paths behind them.
    /// Symbolic links are left out, and so is everything past [`MAX_ENTRIES`].
    pub fn offer(&mut self, roots: &[PathBuf]) -> Vec<FileDescriptor> {
        let mut descriptors = Vec::new();
        let mut paths = Vec::new();
        for root in roots {
            let Some(name) = root.file_name().and_then(|n| n.to_str()).and_then(windows_name) else {
                continue;
            };
            walk(root, name, None, &mut descriptors, &mut paths);
        }
        if descriptors.len() >= MAX_ENTRIES {
            warn!(limit = MAX_ENTRIES, "more files copied than offered to the client");
        }
        self.paths = Arc::new(paths);
        self.open = None;
        descriptors
    }

    pub fn lock(&mut self, id: u32) {
        self.locked.insert(id, self.paths.clone());
    }

    pub fn unlock(&mut self, id: u32) {
        self.locked.remove(&id);
    }

    /// Answers a File Contents Request from the client: a file's size or a piece of it.
    pub fn answer(&mut self, request: &FileContentsRequest) -> FileContentsResponse<'static> {
        let stream = request.stream_id;
        let paths = match request.data_id {
            Some(id) => self.locked.get(&id).cloned().unwrap_or_else(|| self.paths.clone()),
            None => self.paths.clone(),
        };
        let Some(path) = usize::try_from(request.index).ok().and_then(|i| paths.get(i)) else {
            debug!(index = request.index, "file contents asked for a file not offered");
            return FileContentsResponse::new_error(stream);
        };
        let result = if request.flags.contains(FileContentsFlags::SIZE) {
            fs::metadata(path).map(|m| FileContentsResponse::new_size_response(stream, if m.is_dir() { 0 } else { m.len() }))
        } else {
            self.read(path, request.position, request.requested_size)
                .map(|data| FileContentsResponse::new_data_response(stream, data))
        };
        result.unwrap_or_else(|e| {
            warn!(%e, path = %path.display(), "a copied file could not be read for the client");
            FileContentsResponse::new_error(stream)
        })
    }

    fn read(&mut self, path: &Path, position: u64, size: u32) -> io::Result<Vec<u8>> {
        if self.open.as_ref().is_none_or(|(open, _)| open != path) {
            self.open = Some((path.to_owned(), File::open(path)?));
        }
        let Some((_, file)) = self.open.as_mut() else {
            return Err(io::Error::other("no open file"));
        };
        file.seek(SeekFrom::Start(position))?;
        let mut data = Vec::with_capacity(size as usize);
        file.take(u64::from(size)).read_to_end(&mut data)?;
        Ok(data)
    }
}

/// Adds `path`, called `name` on the client inside `parent`, and a folder's contents after it.
fn walk(path: &Path, name: String, parent: Option<&str>, descriptors: &mut Vec<FileDescriptor>, paths: &mut Vec<PathBuf>) {
    if descriptors.len() >= MAX_ENTRIES {
        return;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() {
        return;
    }
    let wire = parent.map_or(name.chars().count(), |p| p.chars().count() + 1 + name.chars().count());
    if wire > MAX_WIRE_NAME {
        warn!(path = %path.display(), "a copied file's path is too long for the client; left out");
        return;
    }
    let mut descriptor = FileDescriptor::new(name.clone());
    if let Some(parent) = parent {
        descriptor = descriptor.with_relative_path(parent);
    }
    if let Some(time) = metadata.modified().ok().and_then(filetime) {
        descriptor = descriptor.with_last_write_time(time);
    }
    if metadata.is_dir() {
        descriptors.push(descriptor.with_attributes(ClipboardFileAttributes::DIRECTORY).with_file_size(0));
        paths.push(path.to_owned());
        let inside = match parent {
            Some(parent) => format!("{parent}\\{name}"),
            None => name,
        };
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        let mut children: Vec<PathBuf> = entries.filter_map(|e| Some(e.ok()?.path())).collect();
        children.sort();
        for child in children {
            if let Some(child_name) = child.file_name().and_then(|n| n.to_str()).and_then(windows_name) {
                walk(&child, child_name, Some(&inside), descriptors, paths);
            }
        }
    } else if metadata.is_file() {
        descriptors.push(descriptor.with_attributes(ClipboardFileAttributes::ARCHIVE).with_file_size(metadata.len()));
        paths.push(path.to_owned());
    }
}

/// Where a descriptor from the client goes under `root`; `None` for a path that would leave it.
fn target(root: &Path, file: &FileDescriptor) -> Option<PathBuf> {
    let mut path = root.to_owned();
    let parts = file.relative_path.iter().flat_map(|p| p.split('\\')).chain([file.name.as_str()]);
    for part in parts.filter(|p| !p.is_empty()) {
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(c)), None) => path.push(c),
            _ => return None,
        }
    }
    (path != root).then_some(path)
}

/// What to do next in a transfer from the client.
#[derive(Debug)]
pub enum Step {
    Request(FileContentsRequest),
    /// An answer to an earlier request; the current one is still out.
    Wait,
    /// Everything arrived; these are the top-level entries.
    Done(Vec<PathBuf>),
    Failed,
}

/// Files copied on the client, being fetched into a folder of our own.
pub struct Incoming {
    root: PathBuf,
    files: Vec<FileDescriptor>,
    targets: Vec<Option<PathBuf>>,
    index: usize,
    /// The file being fetched, how far, and its size once known.
    out: Option<File>,
    position: u64,
    size: Option<u64>,
    stream: u32,
    bytes: u64,
    started: Instant,
    pub last_activity: Instant,
}

impl Incoming {
    /// Prepares to fetch `files` into `root`, a new folder, with stream ids from `stream`.
    pub fn new(root: PathBuf, files: Vec<FileDescriptor>, stream: u32) -> io::Result<Self> {
        fs::create_dir_all(&root)?;
        let targets = files.iter().map(|f| target(&root, f)).collect();
        let now = Instant::now();
        Ok(Self {
            root,
            files,
            targets,
            index: 0,
            out: None,
            position: 0,
            size: None,
            stream,
            bytes: 0,
            started: now,
            last_activity: now,
        })
    }

    pub fn stream(&self) -> u32 {
        self.stream
    }

    fn request(&mut self, flags: FileContentsFlags, size: u32) -> Step {
        self.stream = self.stream.wrapping_add(1);
        Step::Request(FileContentsRequest {
            stream_id: self.stream,
            index: self.index as i32,
            flags,
            position: if flags.contains(FileContentsFlags::SIZE) { 0 } else { self.position },
            requested_size: size,
            data_id: None,
        })
    }

    /// Starts on the current entry or moves past finished ones: the next request, or the end.
    pub fn advance(&mut self) -> Step {
        loop {
            let Some(file) = self.files.get(self.index) else {
                return self.finish();
            };
            let Some(path) = self.targets[self.index].clone() else {
                warn!(name = %file.name, "a file from the client with an unusable path; left out");
                self.index += 1;
                continue;
            };
            let directory = file.attributes.is_some_and(|a| a.contains(ClipboardFileAttributes::DIRECTORY));
            if directory {
                if let Err(e) = fs::create_dir_all(&path) {
                    warn!(%e, path = %path.display(), "could not create a folder copied on the client");
                    return Step::Failed;
                }
                self.index += 1;
                continue;
            }
            if self.out.is_none() {
                let created = path.parent().map_or(Ok(()), fs::create_dir_all).and_then(|()| File::create(&path));
                match created {
                    Ok(out) => self.out = Some(out),
                    Err(e) => {
                        warn!(%e, path = %path.display(), "could not create a file copied on the client");
                        return Step::Failed;
                    }
                }
                self.position = 0;
                self.size = file.file_size;
            }
            let Some(size) = self.size else {
                return self.request(FileContentsFlags::SIZE, 8);
            };
            if self.position >= size {
                self.close_file();
                continue;
            }
            let piece = (size - self.position).min(u64::from(CHUNK)) as u32;
            return self.request(FileContentsFlags::RANGE, piece);
        }
    }

    /// Takes the client's answer to the last request; `None` when it answered with an error.
    pub fn received(&mut self, stream: u32, data: Option<&[u8]>) -> Step {
        if stream != self.stream {
            debug!(stream, expected = self.stream, "file contents for another request; ignored");
            return Step::Wait;
        }
        self.last_activity = Instant::now();
        let Some(data) = data else {
            let name = self.files.get(self.index).map(|f| f.name.clone()).unwrap_or_default();
            warn!(name, "the client could not give a copied file");
            return Step::Failed;
        };
        if self.size.is_none() {
            let Some(size) = data.get(..8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes) else {
                return Step::Failed;
            };
            self.size = Some(size);
            return self.advance();
        }
        if data.is_empty() {
            warn!("the client sent no more of a copied file than it said it had");
            return Step::Failed;
        }
        let written = self.out.as_mut().map(|out| out.write_all(data));
        if let Some(Err(e)) = written {
            warn!(%e, "could not write a file copied on the client");
            return Step::Failed;
        }
        self.position += data.len() as u64;
        self.bytes += data.len() as u64;
        self.advance()
    }

    fn close_file(&mut self) {
        if let Some(out) = self.out.take() {
            if let Some(time) = self.files[self.index].last_write_time.and_then(system_time) {
                let _ = out.set_modified(time);
            }
        }
        self.index += 1;
    }

    fn finish(&mut self) -> Step {
        let tops: Vec<PathBuf> = self
            .files
            .iter()
            .zip(&self.targets)
            .filter(|(f, t)| f.relative_path.as_deref().is_none_or(str::is_empty) && t.is_some())
            .filter_map(|(_, t)| t.clone())
            .collect();
        info!(
            entries = self.files.len(),
            bytes = self.bytes,
            seconds = self.started.elapsed().as_secs_f32(),
            dir = %self.root.display(),
            "files copied on the client arrived"
        );
        Step::Done(tops)
    }
}

/// A new folder under `base` for one transfer; earlier transfers' folders are removed, since
/// Finder has copied what it pasted from them.
pub fn transfer_folder(base: &Path) -> io::Result<PathBuf> {
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    let folder = base.join(millis.to_string());
    fs::create_dir_all(&folder)?;
    Ok(folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rdpmac-clip-files-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Plays the client: answers every request from `outgoing`, as mstsc does from Explorer.
    fn transfer(outgoing: &mut Outgoing, incoming: &mut Incoming) -> Vec<PathBuf> {
        let mut step = incoming.advance();
        for _ in 0..10_000 {
            match step {
                Step::Request(request) => {
                    let response = outgoing.answer(&request);
                    let data = (!response.is_error()).then(|| response.data().to_vec());
                    step = incoming.received(request.stream_id, data.as_deref());
                }
                Step::Done(tops) => return tops,
                Step::Wait | Step::Failed => panic!("transfer stopped: {step:?}"),
            }
        }
        panic!("transfer did not end");
    }

    #[test]
    fn a_folder_tree_and_a_file_go_across_intact() {
        let source = scratch("source");
        let tree = source.join("Project");
        fs::create_dir_all(tree.join("src/empty")).unwrap();
        fs::write(tree.join("README"), b"hello").unwrap();
        let big: Vec<u8> = (0..(CHUNK as usize * 2 + 123)).map(|i| (i % 251) as u8).collect();
        fs::write(tree.join("src/data.bin"), &big).unwrap();
        fs::write(tree.join(".DS_Store"), b"x").unwrap();
        std::os::unix::fs::symlink("/etc", tree.join("link")).unwrap();
        fs::write(source.join("a:b.txt"), b"colon").unwrap();

        let mut outgoing = Outgoing::default();
        let offered = outgoing.offer(&[tree.clone(), source.join("a:b.txt")]);
        let names: Vec<String> = offered
            .iter()
            .map(|f| match &f.relative_path {
                Some(p) => format!("{p}\\{}", f.name),
                None => f.name.clone(),
            })
            .collect();
        assert_eq!(names, ["Project", "Project\\README", "Project\\src", "Project\\src\\data.bin", "Project\\src\\empty", "a_b.txt"]);
        assert!(offered[0].attributes.unwrap().contains(ClipboardFileAttributes::DIRECTORY));
        assert_eq!(offered[3].file_size, Some(big.len() as u64));

        let target = scratch("target");
        let mut incoming = Incoming::new(target.clone(), offered, 7).unwrap();
        let tops = transfer(&mut outgoing, &mut incoming);
        assert_eq!(tops, [target.join("Project"), target.join("a_b.txt")]);
        assert_eq!(fs::read(target.join("Project/README")).unwrap(), b"hello");
        assert_eq!(fs::read(target.join("Project/src/data.bin")).unwrap(), big);
        assert!(target.join("Project/src/empty").is_dir());
        assert_eq!(fs::read(target.join("a_b.txt")).unwrap(), b"colon");
        let (a, b) = (fs::metadata(tree.join("README")).unwrap(), fs::metadata(target.join("Project/README")).unwrap());
        let diff = a.modified().unwrap().duration_since(b.modified().unwrap()).unwrap_or_else(|e| e.duration());
        assert!(diff < Duration::from_millis(1), "modification time kept");
        let _ = (fs::remove_dir_all(&source), fs::remove_dir_all(&target));
    }

    #[test]
    fn sizes_are_asked_for_when_not_given_and_errors_stop_the_transfer() {
        let source = scratch("sizes");
        fs::write(source.join("f"), b"12345").unwrap();
        let mut outgoing = Outgoing::default();
        let mut offered = outgoing.offer(&[source.join("f")]);
        offered[0].file_size = None;
        let target = scratch("sizes-target");
        let mut incoming = Incoming::new(target.clone(), offered.clone(), 0).unwrap();
        match incoming.advance() {
            Step::Request(r) => assert!(r.flags.contains(FileContentsFlags::SIZE)),
            other => panic!("expected a size request, got {other:?}"),
        }
        transfer(&mut outgoing, &mut Incoming::new(scratch("sizes-2"), offered.clone(), 0).unwrap());

        let mut incoming = Incoming::new(target.clone(), offered, 0).unwrap();
        let Step::Request(r) = incoming.advance() else { panic!("expected a request") };
        assert!(matches!(incoming.received(r.stream_id, None), Step::Failed));
        let _ = (fs::remove_dir_all(&source), fs::remove_dir_all(&target));
    }

    #[test]
    fn paths_that_leave_the_folder_are_skipped() {
        let root = Path::new("/tmp/transfer");
        let file = |name: &str, path: Option<&str>| {
            let f = FileDescriptor::new(name);
            match path {
                Some(p) => f.with_relative_path(p),
                None => f,
            }
        };
        assert_eq!(target(root, &file("a", Some("x\\y"))), Some(root.join("x/y/a")));
        assert_eq!(target(root, &file("a", Some("..\\up"))), None);
        assert_eq!(target(root, &file("/etc/passwd", None)), None);
        assert_eq!(target(root, &file("", None)), None);
    }

    #[test]
    fn locked_lists_outlive_a_new_copy_and_unknown_indices_fail() {
        let source = scratch("locks");
        fs::write(source.join("one"), b"1").unwrap();
        fs::write(source.join("two"), b"22").unwrap();
        let mut outgoing = Outgoing::default();
        outgoing.offer(&[source.join("one")]);
        outgoing.lock(5);
        outgoing.offer(&[source.join("two")]);
        let size = |o: &mut Outgoing, data_id| {
            let r = o.answer(&FileContentsRequest {
                stream_id: 1,
                index: 0,
                flags: FileContentsFlags::SIZE,
                position: 0,
                requested_size: 8,
                data_id,
            });
            (!r.is_error()).then(|| r.data_as_size().unwrap())
        };
        assert_eq!(size(&mut outgoing, Some(5)), Some(1), "the locked list");
        assert_eq!(size(&mut outgoing, None), Some(2), "the current list");
        outgoing.unlock(5);
        assert_eq!(size(&mut outgoing, Some(5)), Some(2), "unlocked: the current list");
        let bad = outgoing.answer(&FileContentsRequest {
            stream_id: 2,
            index: 9,
            flags: FileContentsFlags::RANGE,
            position: 0,
            requested_size: 1,
            data_id: None,
        });
        assert!(bad.is_error());
        let _ = fs::remove_dir_all(&source);
    }

    #[test]
    fn windows_names_and_file_times() {
        assert_eq!(windows_name("a:b?c.txt").as_deref(), Some("a_b_c.txt"));
        assert_eq!(windows_name("trailing. ").as_deref(), Some("trailing"));
        assert_eq!(windows_name(".DS_Store"), None);
        assert_eq!(windows_name("._resource"), None);
        let now = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_700);
        assert_eq!(system_time(filetime(now).unwrap()), Some(now));
    }
}
