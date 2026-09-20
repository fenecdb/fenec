//! Native file persistence layer.
//!
//! The file's contents are in *the same format* as the engine's in-memory
//! byte image. A write = append to the file. Open = a single replay pass.
//! No separate WAL + data file pair, no checkpoint, no page cache.

use crate::engine::{Database, Sink, MAGIC};
use crate::error::Result;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Write buffer. One `write` call per document meant one syscall per
/// document; that was the dominant cost during bulk loading.
const WRITE_BUF: usize = 1 << 20;

pub struct FileSink {
    file: BufWriter<File>,
    path: PathBuf,
}

impl FileSink {
    pub fn open(path: impl AsRef<Path>) -> Result<(FileSink, Vec<u8>)> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;
        let mut existing = Vec::new();
        file.read_to_end(&mut existing)?;
        if existing.is_empty() {
            file.write_all(&MAGIC[..])?;
            file.flush()?;
            existing = Vec::from(&MAGIC[..]);
        }
        file.seek(SeekFrom::End(0))?;
        Ok((
            FileSink {
                file: BufWriter::with_capacity(WRITE_BUF, file),
                path,
            },
            existing,
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Sink for FileSink {
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.file.write_all(bytes)?;
        Ok(())
    }
    fn rewrite(&mut self, bytes: &[u8]) -> Result<()> {
        // Atomic replace: write to a side file first, then rename.
        self.file.flush()?;
        let tmp = self.path.with_extension("fenec.compacting");
        {
            let mut f = BufWriter::with_capacity(WRITE_BUF, File::create(&tmp)?);
            f.write_all(bytes)?;
            f.into_inner()
                .map_err(|e| crate::error::Error::Io(e.to_string()))?
                .sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        let mut f = OpenOptions::new().read(true).write(true).open(&self.path)?;
        f.seek(SeekFrom::End(0))?;
        self.file = BufWriter::with_capacity(WRITE_BUF, f);
        Ok(())
    }
    /// Flushes the buffer and pushes it to disk. Because of the buffer, the
    /// last writes can be lost if the process dies before `sync` is called;
    /// fenecdb never fsyncs every write anyway -- this buffer extends that model.
    fn sync(&mut self) -> Result<()> {
        self.file.flush()?;
        self.file.get_ref().sync_data()?;
        Ok(())
    }
}

impl Drop for FileSink {
    fn drop(&mut self) {
        // Do not let buffered leftovers vanish silently.
        let _ = self.file.flush();
    }
}

/// Opens a fenecdb file (creating it when missing) and loads its contents.
pub fn open(path: impl AsRef<Path>) -> Result<Database> {
    let (sink, existing) = FileSink::open(path)?;
    let mut db = Database::with_sink(Box::new(sink));
    if existing.len() > MAGIC.len() {
        db.load(&existing)?;
    }
    Ok(db)
}
