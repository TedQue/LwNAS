//! Log targets
//!
use std::ffi::OsString;
use std::fmt::Debug;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub trait Target: Sync + Send + Debug {
    fn append(&mut self, s: &[u8]);
    fn flush(&mut self);
}

/// Types implement std::io::Write trait are `WriteTargets`
#[derive(Debug)]
pub struct WriteTarget<T: Write> {
    w: T,
}

impl<T: Write> WriteTarget<T> {
    pub fn new(w: T) -> Self {
        WriteTarget { w: w }
    }
}

impl<T: Write + Sync + Send + Debug> Target for WriteTarget<T> {
    fn append(&mut self, s: &[u8]) {
        let _ = self.w.write(s);
    }

    fn flush(&mut self) {
        let _ = self.w.flush();
    }
}

#[derive(Debug)]
pub struct RotatedFileTarget {
    max_size: u64,
    max_count: usize,
    base: PathBuf,
    f: File,
}

impl RotatedFileTarget {
    pub fn new<P: AsRef<Path>>(base: P, max_size: u64, max_count: usize) -> Self {
        RotatedFileTarget {
            max_size: max_size,
            max_count: max_count,
            base: PathBuf::from(base.as_ref()),
            f: Self::open_the_file(base),
        }
    }

    /// Make path like "xxx.log.0/1/2/3"
    fn make_path<P: AsRef<Path>>(base: P, idx: usize) -> PathBuf {
        let mut s = OsString::from(base.as_ref().as_os_str());
        s.push(format!(".{}", idx));
        s.into()
    }

    /// Open the *only* file, xxx.log.0
    fn open_the_file<P: AsRef<Path>>(base: P) -> File {
        let path = Self::make_path(&base, 0);
        OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .expect("failed to open the log file")
    }

    /// Roll over log file when it's full
    fn roll(&self, idx: usize) {
        let path_from = Self::make_path(&self.base, idx);

        // if the next rolling file doesn't exists, do nothing
        if std::fs::metadata(&path_from).is_ok() {
            if idx + 1 >= self.max_count {
                // rolling out the last file means delete it
                let _ = std::fs::remove_file(&path_from);
            } else {
                // roll to next index
                let path_to = Self::make_path(&self.base, idx + 1);
                let _ = std::fs::rename(&path_from, &path_to);
            }
        }
    }

    /// Check current file size, roll it if it's necessary
    fn try_rotate(&mut self, s: &[u8]) {
        let cur_size = self.f.metadata().unwrap().len();
        if cur_size + s.len() as u64 > self.max_size {
            let _ = self.f.sync_all();

            for i in (0..self.max_count).rev() {
                self.roll(i);
            }

            // re-open the *only* file
            self.f = Self::open_the_file(&self.base);
        }
    }
}

impl Target for RotatedFileTarget {
    fn append(&mut self, s: &[u8]) {
        self.try_rotate(s);
        let _ = self.f.write(s);
    }

    fn flush(&mut self) {
        let _ = self.f.flush();
    }
}
