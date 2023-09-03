use super::Engine;
use crate::error::Result;

use fs4::FileExt;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// A very simple variant of BitCask, itself a very simple log-structured
/// key-value engine used e.g. by the Riak database. It is not compatible with
/// BitCask databases generated by other implementations. See:
/// https://riak.com/assets/bitcask-intro.pdf
///
/// BitCask writes key-value pairs to an append-only log file, and keeps a
/// mapping of keys to file positions in memory. All live keys must fit in
/// memory. Deletes write a tombstone value to the log file. To remove old
/// garbage, logs can be compacted by writing new logs containing only live
/// data, skipping replaced values and tombstones.
///
/// This implementation makes several significant simplifications over
/// standard BitCask:
///
/// - Instead of writing multiple fixed-size log files, it uses a single
///   append-only log file of arbitrary size. This increases the compaction
///   volume, since the entire log file must be rewritten on every compaction,
///   and can exceed the filesystem's file size limit, but ToyDB databases are
///   expected to be small.
///
/// - Compactions lock the database for reads and writes. This is ok since ToyDB
///   only compacts during node startup and files are expected to be small.
///
/// - Hint files are not used, the log itself is scanned when opened to
///   build the keydir. Hint files only omit values, and ToyDB values are
///   expected to be small, so the hint files would be nearly as large as
///   the compacted log files themselves.
///
/// - Log entries don't contain timestamps or checksums.
///
/// The structure of a log entry is:
///
/// - Key length as big-endian u32.
/// - Value length as big-endian i32, or -1 for tombstones.
/// - Key as raw bytes (max 2 GB).
/// - Value as raw bytes (max 2 GB).
pub struct BitCask {
    /// The active append-only log file.
    log: Log,
    /// Maps keys to a value position and length in the log file.
    keydir: KeyDir,
}

/// Maps keys to a value position and length in the log file.
type KeyDir = std::collections::BTreeMap<Vec<u8>, (u64, u32)>;

impl BitCask {
    /// Opens or creates a BitCask database in the given file.
    pub fn new(path: PathBuf) -> Result<Self> {
        let mut log = Log::new(path)?;
        let keydir = log.build_keydir()?;
        Ok(Self { log, keydir })
    }

    /// Opens a BitCask database, and automatically compacts it if the amount
    /// of garbage exceeds the given ratio when opened.
    pub fn new_compact(path: PathBuf, garbage_ratio_threshold: f64) -> Result<Self> {
        let mut s = Self::new(path)?;

        let (live_bytes, total_bytes) = s.compute_sizes()?;
        let garbage_bytes = total_bytes - live_bytes;
        let garbage_ratio = garbage_bytes as f64 / total_bytes as f64;
        if garbage_bytes > 0 && garbage_ratio >= garbage_ratio_threshold {
            log::info!(
                "Compacting {} to remove {:.1}MB garbage ({:.0}% of {:.1}MB)",
                s.log.path.display(),
                garbage_bytes / 1024 / 1024,
                garbage_ratio * 100.0,
                total_bytes / 1024 / 1024
            );
            s.compact()?;
            log::info!(
                "Compacted {} to size {:.1}MB",
                s.log.path.display(),
                live_bytes / 1024 / 1024
            );
        }

        Ok(s)
    }
}

impl std::fmt::Display for BitCask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bitcask")
    }
}

impl Engine for BitCask {
    type ScanIterator<'a> = ScanIterator<'a>;

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.log.write_entry(key, None)?;
        self.keydir.remove(key);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(self.log.file.sync_all()?)
    }

    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some((value_pos, value_len)) = self.keydir.get(key) {
            Ok(Some(self.log.read_value(*value_pos, *value_len)?))
        } else {
            Ok(None)
        }
    }

    fn scan<R: std::ops::RangeBounds<Vec<u8>>>(&mut self, range: R) -> Self::ScanIterator<'_> {
        ScanIterator { inner: self.keydir.range(range), log: &mut self.log }
    }

    fn set(&mut self, key: &[u8], value: Vec<u8>) -> Result<()> {
        let (pos, len) = self.log.write_entry(key, Some(&*value))?;
        let value_len = value.len() as u32;
        self.keydir.insert(key.to_vec(), (pos + len as u64 - value_len as u64, value_len));
        Ok(())
    }
}

pub struct ScanIterator<'a> {
    inner: std::collections::btree_map::Range<'a, Vec<u8>, (u64, u32)>,
    log: &'a mut Log,
}

impl<'a> ScanIterator<'a> {
    fn map(&mut self, item: (&Vec<u8>, &(u64, u32))) -> <Self as Iterator>::Item {
        let (key, (value_pos, value_len)) = item;
        Ok((key.clone(), self.log.read_value(*value_pos, *value_len)?))
    }
}

impl<'a> Iterator for ScanIterator<'a> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|item| self.map(item))
    }
}

impl<'a> DoubleEndedIterator for ScanIterator<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back().map(|item| self.map(item))
    }
}

impl BitCask {
    /// Compacts the current log file by writing out a new log file containing
    /// only live keys and replacing the current file with it.
    pub fn compact(&mut self) -> Result<()> {
        let mut tmp_path = self.log.path.clone();
        tmp_path.set_extension("new");
        let (mut new_log, new_keydir) = self.write_log(tmp_path)?;

        std::fs::rename(&new_log.path, &self.log.path)?;
        new_log.path = self.log.path.clone();

        self.log = new_log;
        self.keydir = new_keydir;
        Ok(())
    }

    /// Computes the live and total sizes of the log file, by iterating over the
    /// keydir and fetching the file's size from the filesystem metadata. The
    /// garbage size (i.e. old, replaced entries and tombstones) is the
    /// difference between these values.
    ///
    /// We could keep track of these values during mutations, but it's not
    /// currently needed -- we only use this to determine whether to compact the
    /// database when it's initially opened, so we'd need to run basically the
    /// same computations anyway.
    pub fn compute_sizes(&mut self) -> Result<(u64, u64)> {
        let total_size = self.log.file.metadata()?.len();
        let live_size = self.keydir.iter().fold(0, |size, (key, (_, value_len))| {
            size + 4 + 4 + key.len() as u64 + *value_len as u64
        });
        Ok((live_size, total_size))
    }

    /// Writes out a new log file with the live entries of the current log file
    /// and returns it along with its keydir. Entries are written in key order.
    fn write_log(&mut self, path: PathBuf) -> Result<(Log, KeyDir)> {
        let mut new_keydir = KeyDir::new();
        let mut new_log = Log::new(path)?;
        new_log.file.set_len(0)?; // truncate file if it exists
        for (key, (value_pos, value_len)) in self.keydir.iter() {
            let value = self.log.read_value(*value_pos, *value_len)?;
            let (pos, len) = new_log.write_entry(key, Some(&value))?;
            new_keydir.insert(key.clone(), (pos + len as u64 - *value_len as u64, *value_len));
        }
        Ok((new_log, new_keydir))
    }
}

/// Attempt to flush the file when the database is closed.
impl Drop for BitCask {
    fn drop(&mut self) {
        if let Err(error) = self.flush() {
            log::error!("failed to flush file: {}", error)
        }
    }
}

/// A BitCask append-only log file, containing a sequence of key/value
/// entries encoded as follows;
///
/// - Key length as big-endian u32.
/// - Value length as big-endian i32, or -1 for tombstones.
/// - Key as raw bytes (max 2 GB).
/// - Value as raw bytes (max 2 GB).
struct Log {
    /// Path to the log file.
    path: PathBuf,
    /// The opened file containing the log.
    file: std::fs::File,
}

impl Log {
    /// Opens a log file, or creates one if it does not exist. Takes out an
    /// exclusive lock on the file until it is closed, or errors if the lock is
    /// already held.
    fn new(path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?
        }
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).open(&path)?;
        file.try_lock_exclusive()?;
        Ok(Self { path, file })
    }

    /// Builds a keydir by scanning the log file. If an incomplete entry is
    /// encountered, it is assumed to be caused by an incomplete write operation
    /// and the remainder of the file is truncated.
    fn build_keydir(&mut self) -> Result<KeyDir> {
        let mut len_buf = [0u8; 4];
        let mut keydir = KeyDir::new();
        let file_len = self.file.metadata()?.len();
        let mut r = BufReader::new(&mut self.file);
        let mut pos = r.seek(SeekFrom::Start(0))?;

        while pos < file_len {
            // Read the next entry from the file, returning the key, value
            // position, and value length or None for tombstones.
            let result = || -> std::result::Result<(Vec<u8>, u64, Option<u32>), std::io::Error> {
                r.read_exact(&mut len_buf)?;
                let key_len = u32::from_be_bytes(len_buf);
                r.read_exact(&mut len_buf)?;
                let value_len_or_tombstone = match i32::from_be_bytes(len_buf) {
                    l if l >= 0 => Some(l as u32),
                    _ => None, // -1 for tombstones
                };
                let value_pos = pos + 4 + 4 + key_len as u64;

                let mut key = vec![0; key_len as usize];
                r.read_exact(&mut key)?;

                if let Some(value_len) = value_len_or_tombstone {
                    if value_pos + value_len as u64 > file_len {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "value extends beyond end of file",
                        ));
                    }
                    r.seek_relative(value_len as i64)?; // avoids discarding buffer
                }

                Ok((key, value_pos, value_len_or_tombstone))
            }();

            match result {
                // Populate the keydir with the entry, or remove it on tombstones.
                Ok((key, value_pos, Some(value_len))) => {
                    keydir.insert(key, (value_pos, value_len));
                    pos = value_pos + value_len as u64;
                }
                Ok((key, value_pos, None)) => {
                    keydir.remove(&key);
                    pos = value_pos;
                }
                // If an incomplete entry was found at the end of the file, assume an
                // incomplete write and truncate the file.
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    log::error!("Found incomplete entry at offset {}, truncating file", pos);
                    self.file.set_len(pos)?;
                    break;
                }
                Err(err) => return Err(err.into()),
            }
        }

        Ok(keydir)
    }

    /// Reads a value from the log file.
    fn read_value(&mut self, value_pos: u64, value_len: u32) -> Result<Vec<u8>> {
        let mut value = vec![0; value_len as usize];
        self.file.seek(SeekFrom::Start(value_pos))?;
        self.file.read_exact(&mut value)?;
        Ok(value)
    }

    /// Appends a key/value entry to the log file, using a None value for
    /// tombstones. It returns the position and length of the entry.
    fn write_entry(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<(u64, u32)> {
        let key_len = key.len() as u32;
        let value_len = value.map_or(0, |v| v.len() as u32);
        let value_len_or_tombstone = value.map_or(-1, |v| v.len() as i32);
        let len = 4 + 4 + key_len + value_len;

        let pos = self.file.seek(SeekFrom::End(0))?;
        let mut w = BufWriter::with_capacity(len as usize, &mut self.file);
        w.write_all(&key_len.to_be_bytes())?;
        w.write_all(&value_len_or_tombstone.to_be_bytes())?;
        w.write_all(key)?;
        if let Some(value) = value {
            w.write_all(value)?;
        }
        w.flush()?;

        Ok((pos, len))
    }

    #[cfg(test)]
    /// Prints the entire log file to the given writer in human-readable form.
    fn print<W: Write>(&mut self, w: &mut W) -> Result<()> {
        let mut len_buf = [0u8; 4];
        let file_len = self.file.metadata()?.len();
        let mut r = BufReader::new(&mut self.file);
        let mut pos = r.seek(SeekFrom::Start(0))?;
        let mut idx = 0;

        while pos < file_len {
            writeln!(w, "entry = {}, offset {}", idx, pos)?;

            r.read_exact(&mut len_buf)?;
            let key_len = u32::from_be_bytes(len_buf);
            writeln!(w, "klen  = {} {:x?}", key_len, len_buf)?;

            r.read_exact(&mut len_buf)?;
            let value_len_or_tombstone = i32::from_be_bytes(len_buf); // NB: -1 for tombstones
            let value_len = value_len_or_tombstone.max(0) as u32;
            writeln!(w, "vlen  = {} {:x?}", value_len_or_tombstone, len_buf)?;

            let mut key = vec![0; key_len as usize];
            r.read_exact(&mut key)?;
            write!(w, "key   = ")?;
            if let Ok(str) = std::str::from_utf8(&key) {
                write!(w, r#""{}" "#, str)?;
            }
            writeln!(w, "{:x?}", key)?;

            let mut value = vec![0; value_len as usize];
            r.read_exact(&mut value)?;
            write!(w, "value = ")?;
            if value_len_or_tombstone < 0 {
                write!(w, "tombstone ")?;
            } else if let Ok(str) = std::str::from_utf8(&value) {
                if str.chars().all(|c| !c.is_control()) {
                    write!(w, r#""{}" "#, str)?;
                }
            }
            write!(w, "{:x?}\n\n", value)?;

            pos += 4 + 4 + key_len as u64 + value_len as u64;
            idx += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_DIR: &str = "src/storage/engine/golden/bitcask";

    super::super::tests::test_engine!({
        let path = tempdir::TempDir::new("toydb")?.path().join("toydb");
        BitCask::new(path)?
    });

    /// Creates a new BitCask engine for testing.
    fn setup() -> Result<BitCask> {
        BitCask::new(tempdir::TempDir::new("toydb")?.path().join("toydb"))
    }

    /// Writes various values primarily for testing log file handling.
    ///
    /// - '': empty key and value
    /// - a: write
    /// - b: write, write
    /// - c: write, delete, write
    /// - d: delete, write
    /// - e: write, delete
    /// - f: delete
    fn setup_log(s: &mut BitCask) -> Result<()> {
        s.set(b"b", vec![0x01])?;
        s.set(b"b", vec![0x02])?;

        s.set(b"e", vec![0x05])?;
        s.delete(b"e")?;

        s.set(b"c", vec![0x00])?;
        s.delete(b"c")?;
        s.set(b"c", vec![0x03])?;

        s.set(b"", vec![])?;

        s.set(b"a", vec![0x01])?;

        s.delete(b"f")?;

        s.delete(b"d")?;
        s.set(b"d", vec![0x04])?;

        // Make sure the scan yields the expected results.
        assert_eq!(
            vec![
                (b"".to_vec(), vec![]),
                (b"a".to_vec(), vec![0x01]),
                (b"b".to_vec(), vec![0x02]),
                (b"c".to_vec(), vec![0x03]),
                (b"d".to_vec(), vec![0x04]),
            ],
            s.scan(..).collect::<Result<Vec<_>>>()?,
        );

        Ok(())
    }

    #[test]
    /// Tests that logs are written correctly using a golden file.
    fn log() -> Result<()> {
        let mut s = setup()?;
        setup_log(&mut s)?;

        let mut mint = goldenfile::Mint::new(GOLDEN_DIR);
        s.log.print(&mut mint.new_goldenfile("log")?)?;
        Ok(())
    }

    #[test]
    /// Tests that writing and then reading a file yields the same results.
    fn reopen() -> Result<()> {
        // NB: Don't use setup(), because the tempdir will be removed when
        // the path falls out of scope.
        let path = tempdir::TempDir::new("toydb")?.path().join("toydb");
        let mut s = BitCask::new(path.clone())?;
        setup_log(&mut s)?;

        let expect = s.scan(..).collect::<Result<Vec<_>>>()?;
        drop(s);
        let mut s = BitCask::new(path)?;
        assert_eq!(expect, s.scan(..).collect::<Result<Vec<_>>>()?,);

        Ok(())
    }

    #[test]
    /// Tests log compaction, by writing golden files of the before/after state,
    /// and checking that the database contains the same results, even after
    /// reopening the file.
    fn compact() -> Result<()> {
        // NB: Don't use setup(), because the tempdir will be removed when
        // the path falls out of scope.
        let path = tempdir::TempDir::new("toydb")?.path().join("toydb");
        let mut s = BitCask::new(path.clone())?;
        setup_log(&mut s)?;

        // Dump the initial log file.
        let mut mint = goldenfile::Mint::new(GOLDEN_DIR);
        s.log.print(&mut mint.new_goldenfile("compact-before")?)?;
        let expect = s.scan(..).collect::<Result<Vec<_>>>()?;

        // Compact the log file and assert the new log file contents.
        s.compact()?;
        assert_eq!(path, s.log.path);
        assert_eq!(expect, s.scan(..).collect::<Result<Vec<_>>>()?,);
        s.log.print(&mut mint.new_goldenfile("compact-after")?)?;

        // Reopen the log file and assert that the contents are the same.
        drop(s);
        let mut s = BitCask::new(path)?;
        assert_eq!(expect, s.scan(..).collect::<Result<Vec<_>>>()?,);

        Ok(())
    }

    #[test]
    /// Tests that new_compact() will automatically compact the file when appropriate.
    fn new_compact() -> Result<()> {
        // Create an initial log file with a few entries.
        let dir = tempdir::TempDir::new("toydb")?;
        let path = dir.path().join("orig");
        let compactpath = dir.path().join("compact");

        let mut s = BitCask::new_compact(path.clone(), 0.2)?;
        setup_log(&mut s)?;
        let (live_bytes, total_bytes) = s.compute_sizes()?;
        let garbage_ratio = (total_bytes - live_bytes) as f64 / total_bytes as f64;
        drop(s);

        // Test a few threshold value and assert whether it should trigger compaction.
        let cases = vec![
            (-1.0, true),
            (0.0, true),
            (garbage_ratio - 0.001, true),
            (garbage_ratio, true),
            (garbage_ratio + 0.001, false),
            (1.0, false),
            (2.0, false),
        ];
        for (threshold, expect_compact) in cases.into_iter() {
            std::fs::copy(&path, &compactpath)?;
            let mut s = BitCask::new_compact(compactpath.clone(), threshold)?;
            let (new_live, new_total) = s.compute_sizes()?;
            assert_eq!(new_live, live_bytes);
            if expect_compact {
                assert_eq!(new_total, live_bytes);
            } else {
                assert_eq!(new_total, total_bytes);
            }
        }

        Ok(())
    }

    #[test]
    /// Tests that exclusive locks are taken out on log files, released when the
    /// database is closed, and that an error is returned if a lock is already
    /// held.
    fn log_lock() -> Result<()> {
        let path = tempdir::TempDir::new("toydb")?.path().join("toydb");
        let s = BitCask::new(path.clone())?;

        assert!(BitCask::new(path.clone()).is_err());
        drop(s);
        assert!(BitCask::new(path.clone()).is_ok());

        Ok(())
    }

    #[test]
    /// Tests that an incomplete write at the end of the log file can be
    /// recovered by discarding the last entry.
    fn recovery() -> Result<()> {
        // Create an initial log file with a few entries.
        let dir = tempdir::TempDir::new("toydb")?;
        let path = dir.path().join("complete");
        let truncpath = dir.path().join("truncated");

        let mut log = Log::new(path.clone())?;
        let mut ends = vec![];

        let (pos, len) = log.write_entry("deleted".as_bytes(), Some(&[1, 2, 3]))?;
        ends.push(pos + len as u64);

        let (pos, len) = log.write_entry("deleted".as_bytes(), None)?;
        ends.push(pos + len as u64);

        let (pos, len) = log.write_entry(&[], Some(&[]))?;
        ends.push(pos + len as u64);

        let (pos, len) = log.write_entry("key".as_bytes(), Some(&[1, 2, 3, 4, 5]))?;
        ends.push(pos + len as u64);

        drop(log);

        // Copy the file, and truncate it at each byte, then try to open it
        // and assert that we always retain a prefix of entries.
        let size = std::fs::metadata(&path)?.len();
        for pos in 0..=size {
            std::fs::copy(&path, &truncpath)?;
            let f = std::fs::OpenOptions::new().write(true).open(&truncpath)?;
            f.set_len(pos)?;
            drop(f);

            let mut expect = vec![];
            if pos >= ends[0] {
                expect.push((b"deleted".to_vec(), vec![1, 2, 3]))
            }
            if pos >= ends[1] {
                expect.pop(); // "deleted" key removed
            }
            if pos >= ends[2] {
                expect.push((b"".to_vec(), vec![]))
            }
            if pos >= ends[3] {
                expect.push((b"key".to_vec(), vec![1, 2, 3, 4, 5]))
            }

            let mut s = BitCask::new(truncpath.clone())?;
            assert_eq!(expect, s.scan(..).collect::<Result<Vec<_>>>()?);
        }

        Ok(())
    }

    #[test]
    /// Tests compute_sizes(), both for a log file with known garbage, and
    /// after compacting it when the live size must equal the file size.
    fn compute_sizes() -> Result<()> {
        let mut s = setup()?;
        setup_log(&mut s)?;

        // Before compaction, the log contains garbage, so the live size must be
        // less than the log size.
        let (live_size, total_size) = s.compute_sizes()?;
        assert_eq!(total_size, s.log.file.metadata()?.len());
        assert!(live_size < total_size);

        // After compaction, the live size should not have changed. Furthermore,
        // the log now only contains live data, so the live size must equal the
        // log file size.
        s.compact()?;
        assert_eq!((live_size, live_size), s.compute_sizes()?);
        Ok(())
    }
}
