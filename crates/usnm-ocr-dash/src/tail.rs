//! Reading an append-only JSON-lines file as it grows.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// The most read from a file in one go, so a first read of a large file
/// takes a few polls rather than one long one.
pub const MAX_READ: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct Tail {
    pub path: PathBuf,
    /// Bytes read so far: the start of the first line not yet returned.
    pub offset: u64,
}

/// What a read found.
#[derive(Debug, PartialEq)]
pub enum Got {
    /// Whole lines added since the last read (none if the file hasn't
    /// grown or isn't there).
    Lines(Vec<String>),
    /// The file is shorter than what was read: it was replaced. The caller
    /// starts over; the next read is from the start.
    Reset,
}

impl Tail {
    pub fn new(path: PathBuf) -> Self {
        Self { path, offset: 0 }
    }

    /// The whole lines past the offset. A last line without its newline
    /// is left for the next read.
    pub fn read(&mut self) -> std::io::Result<Got> {
        let mut f = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Got::Lines(Vec::new()))
            }
            Err(e) => return Err(e),
        };
        let len = f.metadata()?.len();
        if len < self.offset {
            self.offset = 0;
            return Ok(Got::Reset);
        }
        if len == self.offset {
            return Ok(Got::Lines(Vec::new()));
        }
        f.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::new();
        f.take((len - self.offset).min(MAX_READ))
            .read_to_end(&mut buf)?;
        let Some(end) = buf.iter().rposition(|b| *b == b'\n') else {
            return Ok(Got::Lines(Vec::new()));
        };
        self.offset += end as u64 + 1;
        Ok(Got::Lines(
            String::from_utf8_lossy(&buf[..end])
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_owned)
                .collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_whole_lines_as_they_come_and_starts_over_on_a_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("index.jsonl");
        let mut t = Tail::new(p.clone());
        assert_eq!(t.read().unwrap(), Got::Lines(vec![]));
        let mut f = std::fs::File::create(&p).unwrap();
        write!(f, "a\nb\npart").unwrap();
        assert_eq!(t.read().unwrap(), Got::Lines(vec!["a".into(), "b".into()]));
        writeln!(f, "ial").unwrap();
        assert_eq!(t.read().unwrap(), Got::Lines(vec!["partial".into()]));
        assert_eq!(t.read().unwrap(), Got::Lines(vec![]));
        std::fs::write(&p, "x\n").unwrap();
        assert_eq!(t.read().unwrap(), Got::Reset);
        assert_eq!(t.read().unwrap(), Got::Lines(vec!["x".into()]));
    }
}
