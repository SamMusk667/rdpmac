//! A record of the H.264 stream exactly as it was sent (`h264-dump`), to tell afterwards whether
//! a client showed what it was sent.
//!
//! Every stream gets a directory under `h264/` in the log directory, named after the UTC time it
//! started, the picture size and the codec. The stream is written in segments that each begin
//! with a key frame, so that each decodes on its own (`ffmpeg -i 0001.h264 …`); for AVC444 every
//! picture is its main view followed by its auxiliary view. Beside each segment a text file lists
//! what happened meanwhile, a line each with the UTC time: the frames sent, the client's
//! acknowledgements and quality reports, and its requests for the whole picture.
//!
//! ```text
//! 2026-09-25T19:14:09.144Z frame 1234 main 5120 aux 2048 key 0 qp 24
//! 2026-09-25T19:14:09.160Z ack 1234 queue 1 decoded 1230
//! 2026-09-25T19:14:09.161Z qoe 1234 timestamp 5812 se 1200 dr 3400
//! 2026-09-25T19:14:10.002Z refresh
//! ```
//!
//! A directory keeps its newest gigabyte, deleting whole segments from the oldest, and only the
//! newest few directories are kept.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, LineWriter, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rdpmac_encode::h264::EncodedFrame;

/// What a stream's directory keeps at most; whole segments go, the oldest first.
const STREAM_LIMIT: u64 = 1 << 30;
/// A key frame starts a new segment once the current one is this large.
const SEGMENT_SIZE: u64 = 64 << 20;
/// Directories kept, the new one included.
const STREAMS_KEPT: usize = 3;

pub struct Dump {
    dir: PathBuf,
    /// The current segment: its number, its stream and its list of events.
    number: u32,
    stream: File,
    events: LineWriter<File>,
    written: u64,
    /// Earlier segments still on disk, the oldest first, with their sizes.
    kept: VecDeque<(u32, u64)>,
    segment_size: u64,
    limit: u64,
}

impl Dump {
    /// Starts the directory for a stream of `width` x `height` in `codec` under `logs`.
    pub fn start(logs: &Path, width: u32, height: u32, codec: &str) -> io::Result<Self> {
        let base = logs.join("h264");
        fs::create_dir_all(&base)?;
        prune(&base, STREAMS_KEPT - 1)?;
        let started = utc(SystemTime::now());
        let dir = base.join(format!("{}Z-{width}x{height}-{codec}", started[..23].replace(':', "")));
        fs::create_dir(&dir)?;
        let (stream, events) = segment(&dir, 1)?;
        Ok(Self {
            dir,
            number: 1,
            stream,
            events,
            written: 0,
            kept: VecDeque::new(),
            segment_size: SEGMENT_SIZE,
            limit: STREAM_LIMIT,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes a frame that went out as `frame_id`.
    pub fn frame(&mut self, frame_id: u32, frame: &EncodedFrame) -> io::Result<()> {
        if frame.key_frame && self.written >= self.segment_size {
            self.next_segment()?;
        }
        let auxiliary = frame.auxiliary.as_deref().unwrap_or_default();
        self.stream.write_all(&frame.data)?;
        self.stream.write_all(auxiliary)?;
        self.written += (frame.data.len() + auxiliary.len()) as u64;
        self.event(format_args!(
            "frame {frame_id} main {} aux {} key {} qp {}",
            frame.data.len(),
            auxiliary.len(),
            u8::from(frame.key_frame),
            frame.qp.unwrap_or(-1)
        ))
    }

    /// Notes something that happened besides a frame.
    pub fn event(&mut self, what: fmt::Arguments<'_>) -> io::Result<()> {
        writeln!(self.events, "{} {what}", utc(SystemTime::now()))
    }

    fn next_segment(&mut self) -> io::Result<()> {
        let number = self.number + 1;
        let (stream, events) = segment(&self.dir, number)?;
        self.kept.push_back((self.number, self.written));
        self.number = number;
        self.stream = stream;
        self.events = events;
        self.written = 0;
        let mut total: u64 = self.kept.iter().map(|&(_, size)| size).sum();
        while total + self.segment_size > self.limit {
            let Some((old, size)) = self.kept.pop_front() else {
                break;
            };
            for extension in ["h264", "txt"] {
                // Deleted by hand meanwhile is as good as deleted.
                let _ = fs::remove_file(self.dir.join(format!("{old:04}.{extension}")));
            }
            total -= size;
        }
        Ok(())
    }
}

/// Creates segment `number`'s stream and event files in `dir`.
fn segment(dir: &Path, number: u32) -> io::Result<(File, LineWriter<File>)> {
    let stream = File::create(dir.join(format!("{number:04}.h264")))?;
    let events = File::create(dir.join(format!("{number:04}.txt")))?;
    Ok((stream, LineWriter::new(events)))
}

/// Deletes the stream directories in `base` but the newest `keep`. Their names start with the
/// time they began, so they sort by it.
fn prune(base: &Path, keep: usize) -> io::Result<()> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(base)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(|c: char| c.is_ascii_digit()))
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    let excess = dirs.len().saturating_sub(keep);
    for dir in &dirs[..excess] {
        fs::remove_dir_all(dir)?;
    }
    Ok(())
}

/// `time` in UTC as RFC 3339 with milliseconds, the way the log writes it.
fn utc(time: SystemTime) -> String {
    let since = time.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let (days, seconds) = ((since.as_secs() / 86_400) as i64, since.as_secs() % 86_400);
    // The civil date of a day count, after Howard Hinnant's days_from_civil inverse.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60,
        since.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn times_are_utc_with_milliseconds() {
        let at = |seconds: u64, millis: u64| {
            utc(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds) + Duration::from_millis(millis))
        };
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(at(951_782_400, 5), "2000-02-29T00:00:00.005Z");
        assert_eq!(at(1_700_000_000, 999), "2023-11-14T22:13:20.999Z");
        assert_eq!(at(1_790_363_649, 144), "2026-09-25T19:14:09.144Z");
    }

    /// A frame of `bytes` whose auxiliary view is half as large.
    fn frame(bytes: usize, key_frame: bool) -> EncodedFrame {
        EncodedFrame {
            data: vec![1; bytes],
            auxiliary: Some(vec![2; bytes / 2]),
            key_frame,
            qp: Some(24),
        }
    }

    fn stream_bytes(dir: &Path) -> u64 {
        fs::read_dir(dir)
            .expect("segments")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "h264"))
            .map(|entry| entry.metadata().map_or(0, |m| m.len()))
            .sum()
    }

    #[test]
    fn segments_start_at_key_frames_and_the_oldest_go() {
        let logs = std::env::temp_dir().join(format!("rdpmac-dump-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&logs);
        for _ in 0..STREAMS_KEPT + 1 {
            Dump::start(&logs, 640, 480, "avc444").expect("start");
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut dump = Dump::start(&logs, 1280, 720, "avc444").expect("start");
        assert_eq!(fs::read_dir(logs.join("h264")).expect("dirs").count(), STREAMS_KEPT);
        (dump.segment_size, dump.limit) = (100, 500);

        dump.frame(1, &frame(10, true)).expect("frame");
        dump.event(format_args!("ack 1 queue 0 decoded 1")).expect("event");
        dump.frame(2, &frame(60, false)).expect("frame");
        dump.frame(3, &frame(10, true)).expect("frame");
        assert_eq!(dump.number, 2, "a key frame after a full segment starts the next");
        dump.frame(4, &frame(10, true)).expect("frame");
        assert_eq!(dump.number, 2, "not before the segment is full");
        let events = fs::read_to_string(dump.dir().join("0001.txt")).expect("events");
        let lines: Vec<&str> = events.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].ends_with("Z frame 1 main 10 aux 5 key 1 qp 24"), "{}", lines[0]);
        assert!(lines[1].ends_with(" ack 1 queue 0 decoded 1"), "{}", lines[1]);
        assert_eq!(fs::metadata(dump.dir().join("0001.h264")).expect("stream").len(), 15 + 90);

        for id in 5..25 {
            dump.frame(id, &frame(60, false)).expect("frame");
            dump.frame(id, &frame(10, true)).expect("frame");
        }
        assert!(!dump.dir().join("0001.h264").exists() && !dump.dir().join("0001.txt").exists());
        assert!(dump.dir().join(format!("{:04}.txt", dump.number)).exists());
        assert!(stream_bytes(dump.dir()) <= 500, "{} bytes kept", stream_bytes(dump.dir()));
        fs::remove_dir_all(&logs).expect("clean up");
    }
}
