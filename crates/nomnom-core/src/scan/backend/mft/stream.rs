//! The MFT read as one sequential stream.
//!
//! `Ntfs::file` is the only way into a file record, and every call to it reads
//! two places: record 0 (`$MFT`, to find where the requested record lives) and
//! then the record itself. Behind a single-block cache those two evict each
//! other, so a scan of a 4.6 GB table re-read a 1 MiB block twice per record.
//!
//! The three kinds of read are kept apart so none can evict another:
//! - the table itself is read forward in large [`Chunk`]s, each one handed to
//!   the parsing workers as soon as it lands, so the next device read runs
//!   while the previous chunk is parsed;
//! - record 0 is pinned in [`SharedVolume`], read once;
//! - everything else (non-resident reparse values, attribute lists, extension
//!   records outside the chunk) goes through one small-block reader behind a
//!   lock. Those reads are rare, so the lock is uncontended in practice.
//!
//! A [`ChunkReader`] is the `Read + Seek` one worker hands `ntfs` for one
//! record. Every byte still comes from the volume at the position `ntfs` asked
//! for, so the parse is the same parse; only where the bytes are cached
//! changes.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Mutex, MutexGuard};

use ntfs::Ntfs;
use ntfs::attribute_value::NtfsAttributeValue;

use super::volume::{AlignedReader, BlockSource};

/// Where each part of the table sits on the volume, read off `$MFT`'s own
/// `$DATA` run list.
pub struct MftLayout {
    record_size: u64,
    /// Total length of the table in bytes, `$DATA`'s logical length.
    len: u64,
    /// In VCN order, so `vbyte` is increasing.
    runs: Vec<Run>,
}

struct Run {
    /// Offset of the run's first byte within the table.
    vbyte: u64,
    /// Volume byte offset, `None` for a sparse run.
    phys: Option<u64>,
    len: u64,
}

/// Where one record's bytes start on the volume, and where the contiguous run
/// holding them ends.
#[derive(Debug, Clone, Copy)]
pub struct RecordSpan {
    pub phys: u64,
    pub run_end: u64,
}

impl MftLayout {
    /// Reads `$MFT`'s run list. Errors carry the wording the scan reports.
    pub fn read<T: Read + Seek>(ntfs: &Ntfs, fs: &mut T) -> Result<Self, String> {
        let mft = ntfs.file(fs, 0).map_err(|err| format!("reading $MFT failed: {err}"))?;
        let data = mft
            .data(fs, "")
            .ok_or_else(|| "$MFT has no $DATA attribute".to_string())?
            .map_err(|err| format!("reading $MFT $DATA failed: {err}"))?;
        let attribute =
            data.to_attribute().map_err(|err| format!("parsing $MFT $DATA failed: {err}"))?;
        let record_size = u64::from(ntfs.file_record_size());
        if record_size == 0 {
            return Err("boot sector reports a zero record size".into());
        }

        // A run list `ntfs` cannot hand over (an `$MFT` that itself needs an
        // attribute list) leaves `runs` empty: every record then goes through
        // the scattered-read path, slower but still correct.
        let mut runs = Vec::new();
        if let Ok(NtfsAttributeValue::NonResident(value)) = attribute.value(fs) {
            let mut vbyte = 0u64;
            for run in value.data_runs() {
                let Ok(run) = run else { break };
                let len = run.allocated_size();
                runs.push(Run { vbyte, phys: run.data_position().value().map(|p| p.get()), len });
                vbyte = vbyte.saturating_add(len);
            }
        }

        Ok(Self { record_size, len: attribute.value_length(), runs })
    }

    pub fn record_count(&self) -> u64 {
        self.len / self.record_size
    }

    /// Bytes of table a full pass has to read.
    pub fn bytes(&self) -> u64 {
        self.len
    }

    pub fn span(&self, number: u64) -> Option<RecordSpan> {
        let vbyte = number.checked_mul(self.record_size)?;
        let index = self.runs.partition_point(|run| run.vbyte <= vbyte).checked_sub(1)?;
        let run = &self.runs[index];
        let into = vbyte - run.vbyte;
        if into >= run.len {
            return None;
        }
        let phys = run.phys?;
        Some(RecordSpan { phys: phys + into, run_end: phys + run.len })
    }
}

/// What every parsing worker shares: the scattered reader, behind a lock, and
/// the pinned `$MFT` record.
pub struct SharedVolume<S> {
    scattered: Mutex<AlignedReader<S>>,
    sector: u64,
    chunk_cap: usize,
    pinned: Vec<u8>,
    pinned_start: u64,
}

impl<S: BlockSource> SharedVolume<S> {
    /// `chunk` is the stream's read size; the scattered reader keeps whatever
    /// block size it was built with.
    pub fn new(scattered: AlignedReader<S>, sector: u64, chunk: usize) -> Self {
        let sector = sector.max(1);
        let chunk_cap = chunk.max(sector as usize).next_multiple_of(sector as usize);
        Self {
            scattered: Mutex::new(scattered),
            sector,
            chunk_cap,
            pinned: Vec::new(),
            pinned_start: 0,
        }
    }

    /// Keeps `len` bytes at `at` for the life of the volume. A failed read
    /// pins nothing, so the bytes are fetched — and fail — the ordinary way.
    pub fn pin(&mut self, at: u64, len: usize) {
        let mut bytes = vec![0; len];
        let scattered = self.scattered.get_mut().unwrap_or_else(|poisoned| poisoned.into_inner());
        let ok =
            scattered.seek(SeekFrom::Start(at)).is_ok() && scattered.read_exact(&mut bytes).is_ok();
        if ok {
            self.pinned = bytes;
            self.pinned_start = at;
        }
    }

    /// The chunk holding the `len` bytes at `span.phys` and the rest of the
    /// run after them, up to a full chunk. A failed read yields an empty
    /// chunk, so the bytes are fetched — and fail — through the scattered
    /// reader exactly as an unbuffered read would.
    pub fn load_chunk(&self, span: RecordSpan) -> Chunk {
        let start = span.phys - span.phys % self.sector;
        let want = span
            .run_end
            .next_multiple_of(self.sector)
            .saturating_sub(start)
            .min(self.chunk_cap as u64) as usize;
        let mut bytes = vec![0; want];
        let read = self.scattered().source_mut().read_at(start, &mut bytes).unwrap_or_default();
        bytes.truncate(read);
        Chunk { start, bytes }
    }

    pub fn source_mut(&mut self) -> &mut S {
        self.scattered.get_mut().unwrap_or_else(|poisoned| poisoned.into_inner()).source_mut()
    }

    fn scattered(&self) -> MutexGuard<'_, AlignedReader<S>> {
        self.scattered.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A run of table bytes read in one device read.
#[derive(Default)]
pub struct Chunk {
    start: u64,
    bytes: Vec<u8>,
}

impl Chunk {
    /// Whether the `len` bytes at `phys` are all in this chunk.
    pub fn holds(&self, phys: u64, len: u64) -> bool {
        phys >= self.start && phys + len <= self.start + self.bytes.len() as u64
    }
}

/// `Read + Seek` for one worker: the chunk its records came in, then the
/// pinned record, then the shared scattered reader.
pub struct ChunkReader<'a, S> {
    volume: &'a SharedVolume<S>,
    chunk: &'a Chunk,
    pos: u64,
}

impl<'a, S> ChunkReader<'a, S> {
    pub fn new(volume: &'a SharedVolume<S>, chunk: &'a Chunk) -> Self {
        Self { volume, chunk, pos: 0 }
    }
}

/// Copies from `buf` (which holds the device bytes starting at `start`) when
/// `pos` falls inside it.
fn serve(buf: &[u8], start: u64, pos: u64, out: &mut [u8]) -> Option<usize> {
    let offset = usize::try_from(pos.checked_sub(start)?).ok()?;
    let available = buf.get(offset..).filter(|rest| !rest.is_empty())?;
    let n = available.len().min(out.len());
    out[..n].copy_from_slice(&available[..n]);
    Some(n)
}

impl<S: BlockSource> Read for ChunkReader<'_, S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let volume = self.volume;
        let n = match serve(&self.chunk.bytes, self.chunk.start, self.pos, out)
            .or_else(|| serve(&volume.pinned, volume.pinned_start, self.pos, out))
        {
            Some(n) => n,
            None => {
                let mut scattered = volume.scattered();
                scattered.seek(SeekFrom::Start(self.pos))?;
                scattered.read(out)?
            }
        };
        self.pos += n as u64;
        Ok(n)
    }
}

impl<S: BlockSource> Seek for ChunkReader<'_, S> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => self.pos.checked_add_signed(d).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "seek position out of range")
            })?,
            // Same contract as `AlignedReader`: `ntfs` never seeks from the end.
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
