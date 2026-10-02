//! Sector-aligned buffered reader over a raw volume.
//!
//! A raw `\\.\C:` handle rejects any read whose byte offset or length is not a
//! multiple of the volume's sector size, while the `ntfs` crate reads at
//! arbitrary offsets and in arbitrary lengths (a 1024-byte file record, a
//! 4-byte reparse tag). This module bridges the two: it serves every read out
//! of one aligned block, refilling the block only when the requested offset
//! falls outside it. One block is a cache for scattered reads, not for the
//! table itself: the sequential pass over the MFT is `super::stream`'s job.
//!
//! The alignment arithmetic is split behind [`BlockSource`] so it can be tested
//! against an ordinary file — an off-by-one-sector bug here would silently
//! corrupt every parsed record, and that is not a bug worth needing
//! Administrator to catch.

use std::io::{self, Read, Seek, SeekFrom};

/// A source that can only be read at aligned offsets.
pub trait BlockSource {
    /// Reads into `out` starting at `offset`. `offset` and `out.len()` are
    /// always multiples of the sector size the reader was built with. Returns
    /// the number of bytes read, which is short only at the end of the device.
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<usize>;
}

/// Buffered `Read + Seek` over a [`BlockSource`], honouring its alignment.
pub struct AlignedReader<S> {
    source: S,
    sector: u64,
    /// Logical position, unconstrained by alignment.
    pos: u64,
    buf: Vec<u8>,
    buf_start: u64,
    buf_len: usize,
}

impl<S: BlockSource> AlignedReader<S> {
    /// `sector` is the device's required alignment; `block` is the read-ahead
    /// size, rounded up to a whole number of sectors.
    pub fn new(source: S, sector: u64, block: usize) -> Self {
        let sector = sector.max(1);
        let block = block.max(sector as usize).next_multiple_of(sector as usize);
        Self { source, sector, pos: 0, buf: vec![0; block], buf_start: 0, buf_len: 0 }
    }

    /// The source underneath, for a caller that reads large aligned spans
    /// itself and must not disturb this reader's cached block.
    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    fn buffered(&self, pos: u64) -> Option<usize> {
        let end = self.buf_start.checked_add(self.buf_len as u64)?;
        (pos >= self.buf_start && pos < end).then(|| (pos - self.buf_start) as usize)
    }

    fn refill(&mut self, pos: u64) -> io::Result<()> {
        // Anchor the block on a multiple of its own length, which is itself a
        // multiple of the sector size — so the offset is aligned, and repeated
        // sequential reads land in the same block instead of re-reading a
        // window that slides by one record each time.
        let block = self.buf.len() as u64;
        let start = pos - (pos % block);
        let mut buf = std::mem::take(&mut self.buf);
        let read = self.source.read_at(start, &mut buf);
        self.buf = buf;
        self.buf_len = read?;
        self.buf_start = start;
        debug_assert_eq!(start % self.sector, 0);
        Ok(())
    }
}

impl<S: BlockSource> Read for AlignedReader<S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut written = 0usize;
        while written < out.len() {
            let offset = match self.buffered(self.pos) {
                Some(offset) => offset,
                None => {
                    self.refill(self.pos)?;
                    match self.buffered(self.pos) {
                        Some(offset) => offset,
                        // Past the end of the device.
                        None => break,
                    }
                }
            };
            let n = (self.buf_len - offset).min(out.len() - written);
            out[written..written + n].copy_from_slice(&self.buf[offset..offset + n]);
            written += n;
            self.pos += n as u64;
        }
        Ok(written)
    }
}

impl<S: BlockSource> Seek for AlignedReader<S> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => self.pos.checked_add_signed(d).ok_or_else(overflowed)?,
            // A raw volume handle has no cheap, reliable length, and `ntfs`
            // never seeks from the end. Failing loudly beats guessing.
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "seeking from the end of a raw volume is not supported",
                ));
            }
        };
        Ok(self.pos)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.pos)
    }
}

fn overflowed() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "seek position out of range")
}

/// What a [`CountingSource`] has seen go through it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IoStats {
    pub reads: u64,
    pub bytes: u64,
    /// Reads that started at an offset some earlier read already started at.
    /// A sequential pass keeps this near zero; a cache that thrashes between
    /// two places on the device drives it toward `reads`.
    pub rereads: u64,
    pub reread_bytes: u64,
}

/// A [`BlockSource`] that counts every device read, so the IO a scan costs is a
/// measured number rather than a guess.
pub struct CountingSource<S> {
    inner: S,
    stats: IoStats,
    seen: std::collections::HashSet<u64>,
}

impl<S> CountingSource<S> {
    pub fn new(inner: S) -> Self {
        Self { inner, stats: IoStats::default(), seen: Default::default() }
    }

    pub fn stats(&self) -> IoStats {
        self.stats
    }
}

impl<S: BlockSource> BlockSource for CountingSource<S> {
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read_at(offset, out)?;
        self.stats.reads += 1;
        self.stats.bytes += n as u64;
        if !self.seen.insert(offset) {
            self.stats.rereads += 1;
            self.stats.reread_bytes += n as u64;
        }
        Ok(n)
    }
}

/// A [`BlockSource`] over an ordinary file, so the alignment arithmetic can be
/// exercised without a raw volume handle.
pub struct FileSource(std::fs::File);

impl FileSource {
    pub fn new(file: std::fs::File) -> Self {
        Self(file)
    }
}

impl BlockSource for FileSource {
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<usize> {
        self.0.seek(SeekFrom::Start(offset))?;
        let mut filled = 0;
        while filled < out.len() {
            match self.0.read(&mut out[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(filled)
    }
}

#[cfg(windows)]
pub use win::VolumeSource;

#[cfg(windows)]
mod win {
    use super::*;

    use std::ffi::OsStr;

    /// A raw volume handle, e.g. `\\?\Volume{...}` or `\\.\C:`.
    ///
    /// The handle comes from `std::fs` rather than a hand-rolled `CreateFileW`:
    /// the only thing the default open lacks is the write share mode a mounted
    /// volume demands, and `OpenOptionsExt::share_mode` supplies exactly that.
    /// That keeps this module free of `unsafe` altogether, leaving the alignment
    /// arithmetic above as the only thing here that can be subtly wrong.
    pub struct VolumeSource(FileSource);

    impl VolumeSource {
        pub fn open(device: &OsStr) -> io::Result<Self> {
            Self::open_with(device, 0)
        }

        /// A second handle for the sequential pass over the table, bypassing
        /// the cache manager: the table is read once, so caching it only
        /// costs a copy and gigabytes of standby memory, and an uncached read
        /// goes to the device as one large transfer. Every read through it
        /// must land in a sector-aligned buffer, which `read_records` honours.
        pub fn open_unbuffered(device: &OsStr) -> io::Result<Self> {
            const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
            const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
            Self::open_with(device, FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN)
        }

        fn open_with(device: &OsStr, flags: u32) -> io::Result<Self> {
            use std::os::windows::fs::OpenOptionsExt;

            const FILE_SHARE_READ: u32 = 0x0000_0001;
            const FILE_SHARE_WRITE: u32 = 0x0000_0002;

            let file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(flags)
                .open(device)?;
            Ok(Self(FileSource::new(file)))
        }
    }

    impl BlockSource for VolumeSource {
        fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<usize> {
            self.0.read_at(offset, out)
        }
    }
}
