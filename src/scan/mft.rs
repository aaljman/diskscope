//! NTFS master file table reader (the fast path, needs admin).
//!
//! Opens the raw volume (`\\.\C:`), reads the boot sector for the geometry, finds the $MFT's own
//! extents in record 0 (following its $ATTRIBUTE_LIST if the table is very fragmented), then
//! reads the whole table in 4 MB chunks through one overlapped handle with 16 reads in flight
//! (an I/O completion port), parsing each completed chunk on the pool while the next ones are
//! read. Every FILE record is parsed with its update-sequence fixups. Each in-use base record gives a name, a parent and a size; records
//! in extension segments add their $FILE_NAME / $DATA to their base. The tree is then built from
//! the parent references, starting at record 5 (the root folder).
//!
//! Counting rules match the folder walk: the unnamed $DATA stream's logical size, cloud
//! placeholders count 0, links (symlinks, junctions) are skipped. A file with several hard links
//! is counted once, under its first Win32 name. The NTFS metadata files ($MFT, $LogFile,
//! $Extend\$UsnJrnl …) and the folders' own index blocks are shown as one aggregate node at the
//! root of a drive scan, sized by what they occupy on disk.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::io;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use rayon::prelude::*;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED, PostQueuedCompletionStatus,
};
use windows_sys::Win32::System::Threading::INFINITE;

use super::{ATTR_OFFLINE, ATTR_RECALL_ON_DATA_ACCESS, TAG_NAME_SURROGATE};
use super::{file_nodes, finish_dir, is_drive_root, root_name};
use crate::model::{Kind, Node, ScanProgress, format_count, format_size};

const ROOT: usize = 5;
const EXTEND: usize = 11;
const BADCLUS: usize = 8;
/// Records below this number are the NTFS metadata files (except 5, the root folder).
const FIRST_USER_RECORD: usize = 16;

const AT_STANDARD_INFORMATION: u32 = 0x10;
const AT_ATTRIBUTE_LIST: u32 = 0x20;
const AT_FILE_NAME: u32 = 0x30;
const AT_DATA: u32 = 0x80;
const AT_REPARSE_POINT: u32 = 0xC0;
const AT_END: u32 = 0xFFFF_FFFF;

/// The record-number part of a file reference (the top 16 bits are the sequence number).
const REF_MASK: u64 = 0xFFFF_FFFF_FFFF;
/// Largest attribute list we read for the $MFT (real ones are a few KB).
const MAX_ATTR_LIST: u64 = 16 << 20;

/// "WofCompressedData" in UTF-16LE: the stream holding a CompactOS-compressed file's bytes.
const WOF_STREAM: &[u8] = b"W\0o\0f\0C\0o\0m\0p\0r\0e\0s\0s\0e\0d\0D\0a\0t\0a\0";

const REC_IN_USE: u16 = 0x01;
const REC_DIRECTORY: u16 = 0x02;

const ATTR_FLAG_COMPRESSED: u16 = 0x0001;
const ATTR_FLAG_SPARSE: u16 = 0x8000;

pub struct MftScan {
    pub root: Node,
    pub notes: Vec<String>,
}

/// Reads a volume: single sector-aligned ranges, and the bulk read of the table.
pub(crate) trait Volume: Sync {
    /// Fills `buf` from byte offset `off`. `off` and `buf.len()` are multiples of the sector
    /// size and `buf` is 4096-byte aligned.
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()>;

    /// Reads every chunk of `plan` into buffers from `pool` and hands each to `done` (chunk
    /// index, filled buffer, whether all its reads succeeded) as soon as it is complete, in any
    /// order. `done` returning false stops further reads (cancel). The default reads one chunk
    /// after another with `read_at`.
    fn read_chunks<'p>(&self, plan: &[ChunkPlan], pool: &'p BufPool, done: &mut dyn FnMut(usize, ChunkBuf<'p>, bool) -> bool) -> ReadStats {
        read_chunks_sync(self, plan, pool, done)
    }
}

/// One read of a chunk: `len` bytes at volume offset `vol` (None: a sparse run, zeros) into the
/// chunk buffer at `at`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Piece {
    vol: Option<u64>,
    at: usize,
    len: usize,
}

/// A stretch of the table read as one unit: `len` bytes, one piece per extent it crosses.
pub(crate) struct ChunkPlan {
    pieces: Vec<Piece>,
    len: usize,
    /// Some bytes are not covered by any extent (stay zero).
    gaps: bool,
}

/// Splits the first `n_records` records of the table into chunks of `chunk` bytes.
fn plan_chunks(layout: &MftLayout, g: &Geometry, n_records: usize, chunk: usize) -> Vec<ChunkPlan> {
    let end = (n_records as u64 * g.record).next_multiple_of(g.cluster);
    let mut plans = Vec::with_capacity(end.div_ceil(chunk as u64) as usize);
    let mut first = 0usize; // extents before this one end before the current chunk
    let mut a = 0u64;
    while a < end {
        let b = (a + chunk as u64).min(end);
        let mut pieces = Vec::new();
        let mut covered = 0u64;
        while first < layout.extents.len() && (layout.extents[first].vcn + layout.extents[first].len) * g.cluster <= a {
            first += 1;
        }
        for e in &layout.extents[first..] {
            let (es, ee) = (e.vcn * g.cluster, (e.vcn + e.len) * g.cluster);
            if es >= b {
                break;
            }
            let (lo, hi) = (a.max(es), b.min(ee));
            if lo < hi {
                pieces.push(Piece { vol: e.lcn.map(|l| l * g.cluster + (lo - es)), at: (lo - a) as usize, len: (hi - lo) as usize });
                covered += hi - lo;
            }
        }
        plans.push(ChunkPlan { pieces, len: (b - a) as usize, gaps: covered < b - a });
        a = b;
    }
    plans
}

/// How a bulk read went.
pub(crate) struct ReadStats {
    pub secs: f64,
    pub bytes: u64,
    /// Time the reader had nothing to read into because every buffer was with a parser.
    pub starved: f64,
    pub engine: &'static str,
}

/// Recycled, aligned chunk buffers shared by the reader and the parsers. A buffer coming back
/// wakes the reader (its condvar, and its completion port when it has one).
pub(crate) struct BufPool {
    free: Mutex<Vec<AlignedBuf>>,
    cv: Condvar,
    /// The reader's completion port (0 = none).
    port: AtomicUsize,
}

const WAKE_KEY: usize = 1;

impl BufPool {
    fn new(count: usize, size: usize) -> Self {
        BufPool {
            free: Mutex::new((0..count).map(|_| AlignedBuf::new(size)).collect()),
            cv: Condvar::new(),
            port: AtomicUsize::new(0),
        }
    }

    fn try_take(&self) -> Option<AlignedBuf> {
        self.free.lock().ok()?.pop()
    }

    /// Waits for a buffer, helping the rayon pool meanwhile when on one of its threads.
    fn take(&self) -> AlignedBuf {
        loop {
            if let Some(b) = self.try_take() {
                return b;
            }
            if matches!(rayon::yield_now(), Some(rayon::Yield::Executed)) {
                continue;
            }
            if let Ok(g) = self.free.lock() {
                if g.is_empty() {
                    let _ = self.cv.wait_timeout(g, std::time::Duration::from_millis(5));
                }
            }
        }
    }

    fn put(&self, b: AlignedBuf) {
        if let Ok(mut g) = self.free.lock() {
            g.push(b);
        }
        self.cv.notify_one();
        let port = self.port.load(Relaxed);
        if port != 0 {
            unsafe { PostQueuedCompletionStatus(port as HANDLE, 0, WAKE_KEY, std::ptr::null()) };
        }
    }
}

/// A filled chunk buffer; dropping it gives it back to the pool.
pub(crate) struct ChunkBuf<'p> {
    buf: Option<AlignedBuf>,
    pool: &'p BufPool,
}

impl<'p> ChunkBuf<'p> {
    fn new(buf: AlignedBuf, pool: &'p BufPool) -> Self {
        ChunkBuf { buf: Some(buf), pool }
    }
}

impl std::ops::Deref for ChunkBuf<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.buf.as_deref().unwrap_or(&[])
    }
}

impl std::ops::DerefMut for ChunkBuf<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.buf.as_deref_mut().unwrap_or(&mut [])
    }
}

impl Drop for ChunkBuf<'_> {
    fn drop(&mut self) {
        if let Some(b) = self.buf.take() {
            self.pool.put(b);
        }
    }
}

/// Zero what the reads won't cover.
fn prepare(buf: &mut AlignedBuf, p: &ChunkPlan) {
    if p.gaps {
        buf[..p.len].fill(0);
    }
    for pc in p.pieces.iter().filter(|pc| pc.vol.is_none()) {
        buf[pc.at..pc.at + pc.len].fill(0);
    }
}

/// One chunk after another, with blocking reads.
fn read_chunks_sync<'p, V: Volume + ?Sized>(
    vol: &V,
    plan: &[ChunkPlan],
    pool: &'p BufPool,
    done: &mut dyn FnMut(usize, ChunkBuf<'p>, bool) -> bool,
) -> ReadStats {
    let t0 = Instant::now();
    let (mut starved, mut bytes) = (0f64, 0u64);
    for (ci, p) in plan.iter().enumerate() {
        let tw = Instant::now();
        let mut buf = pool.take();
        starved += tw.elapsed().as_secs_f64();
        prepare(&mut buf, p);
        let mut ok = true;
        for pc in &p.pieces {
            if let Some(off) = pc.vol {
                ok &= vol.read_at(off, &mut buf[pc.at..pc.at + pc.len]).is_ok();
            }
        }
        bytes += p.len as u64;
        if !done(ci, ChunkBuf::new(buf, pool), ok) {
            break;
        }
    }
    ReadStats { secs: t0.elapsed().as_secs_f64(), bytes, starved, engine: "sync" }
}

/// A raw handle that may cross threads (only used for thread-safe calls).
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}
unsafe impl Sync for SendHandle {}

/// The raw volume `\\.\C:`, opened unbuffered: a few synchronous handles for single reads
/// (one per concurrent caller, so they never serialise on one file object), and one overlapped
/// handle with a completion port for the bulk read.
struct RawVolume {
    path: String,
    handles: Mutex<Vec<std::fs::File>>,
    overlapped: Option<(SendHandle, SendHandle)>, // (volume handle, completion port)
}

impl RawVolume {
    fn open(letter: char) -> io::Result<Self> {
        Self::open_path(format!("\\\\.\\{letter}:"))
    }

    /// Any path opened the same way (tests use a volume image file).
    fn open_path(path: String) -> io::Result<Self> {
        let mut v = RawVolume { path, handles: Mutex::new(Vec::new()), overlapped: None };
        let f = v.open_handle()?;
        v.handles.lock().unwrap().push(f);
        v.overlapped = v.open_overlapped();
        Ok(v)
    }

    fn open_handle(&self) -> io::Result<std::fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(7) // read | write | delete: the volume is in use
            .custom_flags(FILE_FLAG_NO_BUFFERING)
            .open(&self.path)
    }

    fn open_overlapped(&self) -> Option<(SendHandle, SendHandle)> {
        let w: Vec<u16> = self.path.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let h = CreateFileW(
                w.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | FILE_FLAG_NO_BUFFERING,
                std::ptr::null_mut(),
            );
            if h == INVALID_HANDLE_VALUE || h.is_null() {
                return None;
            }
            let port = CreateIoCompletionPort(h, std::ptr::null_mut(), 0, 0);
            if port.is_null() {
                CloseHandle(h);
                return None;
            }
            Some((SendHandle(h), SendHandle(port)))
        }
    }

    /// Keeps as many chunk reads in flight as the pool has free buffers, on the overlapped handle; completions (and
    /// buffers coming back from the parsers) arrive on one completion port.
    fn read_chunks_iocp<'p>(
        &self,
        h: HANDLE,
        port: HANDLE,
        plan: &[ChunkPlan],
        pool: &'p BufPool,
        done: &mut dyn FnMut(usize, ChunkBuf<'p>, bool) -> bool,
    ) -> ReadStats {
        #[repr(C)]
        struct Op {
            ov: OVERLAPPED, // first: the port hands back a pointer to it
            slot: u32,
            len: u32,
        }
        struct Slot {
            buf: Option<AlignedBuf>,
            ci: usize,
            pending: u32,
            ok: bool,
        }
        pool.port.store(port as usize, Relaxed);
        let t0 = Instant::now();
        let mut slots: Vec<Slot> = Vec::new();
        let mut free_slots: Vec<usize> = Vec::new();
        let (mut next, mut inflight, mut bytes) = (0usize, 0usize, 0u64);
        let mut starved = std::time::Duration::ZERO;
        let mut stop = false;
        let mut finish = |slots: &mut Vec<Slot>, si: usize, free_slots: &mut Vec<usize>, stop: &mut bool| {
            let s = &mut slots[si];
            if let Some(b) = s.buf.take() {
                if !done(s.ci, ChunkBuf::new(b, pool), s.ok) {
                    *stop = true;
                }
            }
            free_slots.push(si);
        };
        loop {
            while next < plan.len() && !stop {
                let Some(mut buf) = pool.try_take() else { break };
                let p = &plan[next];
                prepare(&mut buf, p);
                let base = buf.as_mut_ptr();
                let si = match free_slots.pop() {
                    Some(si) => si,
                    None => {
                        slots.push(Slot { buf: None, ci: 0, pending: 0, ok: true });
                        slots.len() - 1
                    }
                };
                slots[si] = Slot { buf: Some(buf), ci: next, pending: 0, ok: true };
                for pc in &p.pieces {
                    let Some(off) = pc.vol else { continue };
                    let mut ov = OVERLAPPED::default();
                    ov.Anonymous.Anonymous.Offset = off as u32;
                    ov.Anonymous.Anonymous.OffsetHigh = (off >> 32) as u32;
                    let op = Box::into_raw(Box::new(Op { ov, slot: si as u32, len: pc.len as u32 }));
                    let r = unsafe { ReadFile(h, base.add(pc.at), pc.len as u32, std::ptr::null_mut(), &mut (*op).ov) };
                    if r == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
                        drop(unsafe { Box::from_raw(op) }); // failed at once: no completion comes
                        slots[si].ok = false;
                    } else {
                        slots[si].pending += 1; // completes through the port (also when synchronous)
                    }
                }
                bytes += p.len as u64;
                next += 1;
                if slots[si].pending == 0 {
                    finish(&mut slots, si, &mut free_slots, &mut stop);
                } else {
                    inflight += 1;
                }
            }
            if inflight == 0 && (next >= plan.len() || stop) {
                break;
            }
            let starving = inflight == 0;
            let tw = Instant::now();
            if starving && matches!(rayon::yield_now(), Some(rayon::Yield::Executed)) {
                starved += tw.elapsed();
                continue;
            }
            let (mut n, mut key, mut ovp) = (0u32, 0usize, std::ptr::null_mut::<OVERLAPPED>());
            let wait = if starving { 5 } else { INFINITE };
            let r = unsafe { GetQueuedCompletionStatus(port, &mut n, &mut key, &mut ovp, wait) };
            if starving {
                starved += tw.elapsed();
            }
            if ovp.is_null() {
                if r == 0 && !starving {
                    // the wait itself failed: cancel what is in flight and drain it
                    unsafe { CancelIoEx(h, std::ptr::null()) };
                    stop = true;
                }
                continue; // a buffer came back, or a timeout
            }
            let op = unsafe { Box::from_raw(ovp as *mut Op) };
            let si = op.slot as usize;
            if r == 0 || n != op.len {
                slots[si].ok = false;
            }
            slots[si].pending -= 1;
            if slots[si].pending == 0 {
                inflight -= 1;
                finish(&mut slots, si, &mut free_slots, &mut stop);
            }
        }
        pool.port.store(0, Relaxed);
        ReadStats { secs: t0.elapsed().as_secs_f64(), bytes, starved: starved.as_secs_f64(), engine: "overlapped" }
    }
}

impl Drop for RawVolume {
    fn drop(&mut self) {
        if let Some((h, port)) = self.overlapped.take() {
            unsafe {
                CloseHandle(port.0);
                CloseHandle(h.0);
            }
        }
    }
}

impl Volume for RawVolume {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        let popped = self.handles.lock().unwrap().pop();
        let f = match popped {
            Some(f) => f,
            None => self.open_handle()?,
        };
        let mut done = 0usize;
        let r = loop {
            if done == buf.len() {
                break Ok(());
            }
            match f.seek_read(&mut buf[done..], off + done as u64) {
                Ok(0) => break Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => break Err(e),
            }
        };
        self.handles.lock().unwrap().push(f);
        r
    }

    fn read_chunks<'p>(&self, plan: &[ChunkPlan], pool: &'p BufPool, done: &mut dyn FnMut(usize, ChunkBuf<'p>, bool) -> bool) -> ReadStats {
        match &self.overlapped {
            Some((h, port)) if std::env::var("DISKSCOPE_MFT_IO").map_or(true, |v| v != "sync") => {
                self.read_chunks_iocp(h.0, port.0, plan, pool, done)
            }
            _ => read_chunks_sync(self, plan, pool, done),
        }
    }
}

/// Bulk-read settings: `DISKSCOPE_MFT_CHUNK_MB` (default 4) and `DISKSCOPE_MFT_QD`, the number
/// of chunk buffers, i.e. reads in flight plus chunks being parsed (default 16).
fn bulk_settings(g: &Geometry) -> (usize, usize) {
    let env = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse::<usize>().ok()).filter(|&v| v > 0).unwrap_or(d);
    let chunk = ((env("DISKSCOPE_MFT_CHUNK_MB", 4).min(64) as u64) << 20).max(g.cluster).max(g.record) as usize;
    (chunk, env("DISKSCOPE_MFT_QD", 16).min(256))
}

/// A zeroed, 4096-aligned byte buffer (unbuffered I/O needs sector-aligned memory).
struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
}

unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    fn new(len: usize) -> Self {
        let len = len.max(4096).next_multiple_of(4096);
        let layout = Layout::from_size_align(len, 4096).expect("layout");
        let p = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(p).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        AlignedBuf { ptr, len }
    }
}

impl std::ops::Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl std::ops::DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr(), Layout::from_size_align_unchecked(self.len, 4096)) };
    }
}

// ---------------------------------------------------------------------------------------------
// little-endian readers that never panic on short input

fn u16_at(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8).map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap()))
}

// ---------------------------------------------------------------------------------------------
// geometry

#[derive(Clone, Copy, Debug)]
pub(crate) struct Geometry {
    pub sector: u64,
    pub cluster: u64,
    pub record: u64,
    pub mft_lcn: u64,
    /// Size of the volume, from the boot sector.
    pub volume_bytes: u64,
}

/// Geometry for reading the boot sector itself: 4096 bytes are a whole sector on 512-byte and
/// on 4K-native disks.
const BOOT_READ: Geometry = Geometry { sector: 4096, cluster: 4096, record: 1024, mft_lcn: 0, volume_bytes: 0 };

pub(crate) fn parse_boot(b: &[u8]) -> Result<Geometry, String> {
    if b.get(3..11) != Some(b"NTFS    ") {
        return Err("not an NTFS boot sector".into());
    }
    let sector = u16_at(b, 0x0B) as u64;
    if !sector.is_power_of_two() || !(256..=4096).contains(&sector) {
        return Err(format!("odd sector size {sector}"));
    }
    let spc_raw = b[0x0D];
    let spc = if spc_raw <= 0x80 { spc_raw as u64 } else { 1u64 << (256 - spc_raw as u32).min(31) };
    let cluster = sector * spc;
    if spc == 0 || !cluster.is_power_of_two() || cluster > (2 << 20) {
        return Err(format!("odd cluster size {cluster}"));
    }
    let cpr = b[0x40] as i8;
    let record = if cpr > 0 { cpr as u64 * cluster } else { 1u64 << (-(cpr as i32)).clamp(0, 31) };
    if !record.is_power_of_two() || !(512..=65536).contains(&record) {
        return Err(format!("odd file record size {record}"));
    }
    let mft_lcn = u64_at(b, 0x30);
    let volume_bytes = u64_at(b, 0x28).saturating_mul(sector);
    if volume_bytes < cluster * 16 || mft_lcn.saturating_mul(cluster) >= volume_bytes {
        return Err("odd volume size or $MFT position".into());
    }
    Ok(Geometry { sector, cluster, record, mft_lcn, volume_bytes })
}

// ---------------------------------------------------------------------------------------------
// FILE records

/// Undoes the update-sequence protection in place: the last two bytes of every 512-byte block
/// hold the sequence number and their real value is in the update-sequence array.
/// False if the record is torn or malformed.
pub(crate) fn apply_fixup(rec: &mut [u8]) -> bool {
    let usa_off = u16_at(rec, 4) as usize;
    let usa_cnt = u16_at(rec, 6) as usize;
    if usa_cnt < 2 || usa_off < 0x28 || usa_off + usa_cnt * 2 > rec.len() {
        return false;
    }
    let blocks = usa_cnt - 1;
    if blocks * 512 > rec.len() {
        return false;
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 0..blocks {
        let end = (i + 1) * 512 - 2;
        if rec[end..end + 2] != usn {
            return false;
        }
        rec[end] = rec[usa_off + 2 + 2 * i];
        rec[end + 1] = rec[usa_off + 3 + 2 * i];
    }
    true
}

#[derive(Clone, Copy)]
struct Attr<'a> {
    ty: u32,
    nonres: bool,
    name_len: u8,
    flags: u16,
    b: &'a [u8],
}

impl<'a> Attr<'a> {
    fn value(&self) -> Option<&'a [u8]> {
        if self.nonres {
            return None;
        }
        let len = u32_at(self.b, 0x10) as usize;
        let off = u16_at(self.b, 0x14) as usize;
        self.b.get(off..off.checked_add(len)?)
    }
    /// The attribute's name as UTF-16LE bytes (empty when unnamed).
    fn name_utf16le(&self) -> &'a [u8] {
        let o = u16_at(self.b, 0x0A) as usize;
        self.b.get(o..o + 2 * self.name_len as usize).unwrap_or(&[])
    }
    fn lowest_vcn(&self) -> u64 {
        u64_at(self.b, 0x10)
    }
    fn runs(&self) -> &'a [u8] {
        let o = u16_at(self.b, 0x20) as usize;
        self.b.get(o..).unwrap_or(&[])
    }
    fn allocated(&self) -> u64 {
        u64_at(self.b, 0x28)
    }
    fn data_size(&self) -> u64 {
        u64_at(self.b, 0x30)
    }
    /// Clusters actually in use: the "total allocated" field for compressed/sparse streams
    /// (holes excluded), else the allocated size.
    fn on_disk(&self) -> u64 {
        if self.flags & (ATTR_FLAG_COMPRESSED | ATTR_FLAG_SPARSE) != 0 && u16_at(self.b, 0x20) >= 0x48 {
            u64_at(self.b, 0x40)
        } else {
            self.allocated()
        }
    }
}

/// The attributes of a fixed-up record.
fn attrs(rec: &[u8]) -> impl Iterator<Item = Attr<'_>> {
    let used = (u32_at(rec, 0x18) as usize).min(rec.len());
    let mut off = u16_at(rec, 0x14) as usize;
    std::iter::from_fn(move || {
        if off + 16 > used {
            return None;
        }
        let ty = u32_at(rec, off);
        let len = u32_at(rec, off + 4) as usize;
        if ty == AT_END || len < 16 || off + len > used {
            return None;
        }
        let b = &rec[off..off + len];
        off += len;
        let nonres = b[8] != 0;
        if nonres && len < 0x40 {
            return Some(Attr { ty: 0, nonres, name_len: 0, flags: 0, b }); // malformed: ignore
        }
        Some(Attr { ty, nonres, name_len: b[9], flags: u16_at(b, 0x0C), b })
    })
}

/// What one FILE record says.
#[derive(Default)]
struct RecInfo<'a> {
    in_use: bool,
    dir: bool,
    /// The record's sequence number (bumped each time the slot is reused).
    seq: u16,
    /// An extension record of `base` (which can be record 0: the $MFT's own extensions).
    ext: bool,
    base: u64,
    base_seq: u16,
    si_attrs: u32,
    /// Best name: (parent record, parent sequence, rank, UTF-16LE bytes).
    /// Rank 3 Win32, 2 POSIX, 1 DOS-only.
    name: Option<(u64, u16, u8, &'a [u8])>,
    /// More names of rank >= 2: extra hard links (not counted again).
    extra_links: u32,
    /// Logical size of the unnamed $DATA stream (from its first extent or resident value).
    data_size: Option<u64>,
    /// The unnamed $DATA's first extent is sparse or has no clusters (Some when seen here).
    data_hollow: Option<bool>,
    /// On-disk bytes of the file's data: the unnamed $DATA (resident = 0, as Explorer's "Size on
    /// disk") plus a WofCompressedData stream (Windows' CompactOS compression keeps the real
    /// bytes there).
    data_disk: u64,
    /// On-disk size of all non-resident attributes that start in this record.
    disk: u64,
    /// The reparse tag, when the $REPARSE_POINT value is in the record.
    reparse_tag: Option<u32>,
    /// The $REPARSE_POINT's runs, when its value is non-resident.
    reparse_runs: Option<&'a [u8]>,
}

/// Rejects torn records and records that sit in the wrong slot. NTFS 3.1 records (update
/// sequence array at 0x30 or later) carry their own number at 0x2C.
fn record_ok(rec: &mut [u8], rec_no: u64) -> bool {
    if !apply_fixup(rec) {
        return false;
    }
    u16_at(rec, 4) < 0x30 || u32_at(rec, 0x2C) as u64 == rec_no & 0xFFFF_FFFF
}

fn parse_record(rec: &[u8]) -> RecInfo<'_> {
    let flags = u16_at(rec, 0x16);
    let base_ref = u64_at(rec, 0x20);
    let mut r = RecInfo {
        in_use: flags & REC_IN_USE != 0,
        dir: flags & REC_DIRECTORY != 0,
        seq: u16_at(rec, 0x10),
        ext: base_ref != 0,
        base: base_ref & REF_MASK,
        base_seq: (base_ref >> 48) as u16,
        ..Default::default()
    };
    if !r.in_use {
        return r;
    }
    for a in attrs(rec) {
        if a.nonres && a.lowest_vcn() == 0 {
            r.disk += a.on_disk();
        }
        match a.ty {
            AT_STANDARD_INFORMATION => {
                if let Some(v) = a.value() {
                    r.si_attrs = u32_at(v, 0x20);
                }
            }
            AT_FILE_NAME => {
                let Some(v) = a.value() else { continue };
                let parent_ref = u64_at(v, 0);
                let len = *v.get(0x40).unwrap_or(&0) as usize;
                let ns = *v.get(0x41).unwrap_or(&2);
                let Some(name) = v.get(0x42..0x42 + 2 * len) else { continue };
                if len == 0 {
                    continue;
                }
                let rank = match ns {
                    1 | 3 => 3,
                    0 => 2,
                    _ => 1,
                };
                let this = Some((parent_ref & REF_MASK, (parent_ref >> 48) as u16, rank, name));
                match r.name {
                    Some((_, _, best, _)) if best >= rank => {
                        if rank >= 2 {
                            r.extra_links += 1;
                        }
                    }
                    Some((_, _, best, _)) => {
                        if best >= 2 {
                            r.extra_links += 1;
                        }
                        r.name = this;
                    }
                    None => r.name = this,
                }
            }
            AT_DATA if a.name_len == 0 => {
                if a.nonres {
                    if a.lowest_vcn() == 0 {
                        r.data_size = Some(a.data_size());
                        r.data_hollow = Some(a.flags & ATTR_FLAG_SPARSE != 0 || a.allocated() == 0);
                        r.data_disk += a.on_disk();
                    }
                } else if let Some(v) = a.value() {
                    r.data_size = Some(v.len() as u64);
                    r.data_hollow = Some(false);
                }
            }
            AT_DATA if a.nonres && a.lowest_vcn() == 0 && a.name_utf16le() == WOF_STREAM => {
                r.data_disk += a.on_disk();
            }
            AT_REPARSE_POINT => {
                if let Some(v) = a.value() {
                    r.reparse_tag = Some(u32_at(v, 0));
                } else if a.nonres && a.lowest_vcn() == 0 {
                    r.reparse_runs = Some(a.runs());
                }
            }
            _ => {}
        }
    }
    r
}

// ---------------------------------------------------------------------------------------------
// the $MFT's own layout

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Extent {
    pub vcn: u64,
    /// None for a sparse run.
    pub lcn: Option<u64>,
    pub len: u64,
}

/// Decodes a mapping-pairs array starting at `vcn`.
pub(crate) fn decode_runs(runs: &[u8], mut vcn: u64) -> Option<Vec<Extent>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut lcn: i64 = 0;
    while i < runs.len() {
        let h = runs[i];
        if h == 0 {
            break;
        }
        let ln = (h & 0x0F) as usize;
        let lo = (h >> 4) as usize;
        i += 1;
        if ln == 0 || ln > 8 || lo > 8 || i + ln + lo > runs.len() {
            return None;
        }
        let mut len = 0u64;
        for k in 0..ln {
            len |= (runs[i + k] as u64) << (8 * k);
        }
        i += ln;
        if lo == 0 {
            out.push(Extent { vcn, lcn: None, len });
        } else {
            let mut d = 0i64;
            for k in 0..lo {
                d |= (runs[i + k] as i64) << (8 * k);
            }
            let shift = 64 - 8 * lo as u32;
            d = (d << shift) >> shift; // sign-extend
            lcn = lcn.checked_add(d)?;
            if lcn < 0 {
                return None;
            }
            out.push(Extent { vcn, lcn: Some(lcn as u64), len });
        }
        i += lo;
        vcn = vcn.checked_add(len)?;
    }
    Some(out)
}

struct MftLayout {
    extents: Vec<Extent>,
    /// Bytes of the table that hold records.
    data_size: u64,
}

/// Reads `len` bytes at any volume offset (bounce buffer, aligned superset).
fn read_any(vol: &dyn Volume, g: &Geometry, off: u64, len: usize) -> io::Result<Vec<u8>> {
    let a = g.sector.max(512);
    let start = off / a * a;
    let end = (off + len as u64).next_multiple_of(a);
    let mut buf = AlignedBuf::new((end - start) as usize);
    vol.read_at(start, &mut buf[..(end - start) as usize])?;
    let s = (off - start) as usize;
    Ok(buf[s..s + len].to_vec())
}

/// Volume offset of byte `x` of a stream laid out as `extents`.
fn map_offset(extents: &[Extent], cluster: u64, x: u64) -> Option<u64> {
    let vcn = x / cluster;
    let e = extents.iter().find(|e| e.vcn <= vcn && vcn < e.vcn + e.len)?;
    Some(e.lcn? * cluster + (vcn - e.vcn) * cluster + x % cluster)
}

/// Reads a non-resident stream's first `size` bytes.
fn read_stream(vol: &dyn Volume, g: &Geometry, extents: &[Extent], size: u64) -> io::Result<Vec<u8>> {
    let mut out = vec![0u8; size as usize];
    for e in extents {
        let start = e.vcn * g.cluster;
        if start >= size {
            continue;
        }
        let n = (e.len * g.cluster).min(size - start) as usize;
        if let Some(lcn) = e.lcn {
            let bytes = read_any(vol, g, lcn * g.cluster, n)?;
            out[start as usize..start as usize + n].copy_from_slice(&bytes);
        }
    }
    Ok(out)
}

fn read_mft_record(vol: &dyn Volume, g: &Geometry, extents: &[Extent], rec_no: u64) -> Result<Vec<u8>, String> {
    let off = map_offset(extents, g.cluster, rec_no * g.record)
        .ok_or_else(|| format!("MFT record {rec_no} is outside the known extents"))?;
    let mut rec = read_any(vol, g, off, g.record as usize).map_err(|e| format!("read: {e}"))?;
    if rec.get(0..4) != Some(b"FILE") || !record_ok(&mut rec, rec_no) {
        return Err(format!("MFT record {rec_no} is damaged"));
    }
    Ok(rec)
}

fn mft_layout(vol: &dyn Volume, g: &Geometry) -> Result<MftLayout, String> {
    let rec0_off = g.mft_lcn * g.cluster;
    let mut rec0 = read_any(vol, g, rec0_off, g.record as usize).map_err(|e| format!("read: {e}"))?;
    if rec0.get(0..4) != Some(b"FILE") || !record_ok(&mut rec0, 0) {
        return Err("$MFT record is damaged".into());
    }
    let mut extents = Vec::new();
    let mut data_size = None;
    let mut attr_list: Option<Vec<u8>> = None;
    for a in attrs(&rec0) {
        if a.ty == AT_DATA && a.name_len == 0 && a.nonres {
            let ex = decode_runs(a.runs(), a.lowest_vcn()).ok_or("bad $MFT runs")?;
            extents.extend(ex);
            if a.lowest_vcn() == 0 {
                data_size = Some(a.data_size());
            }
        } else if a.ty == AT_ATTRIBUTE_LIST {
            attr_list = Some(match a.value() {
                Some(v) => v.to_vec(),
                None => {
                    if a.data_size() > MAX_ATTR_LIST {
                        return Err("the $MFT attribute list is implausibly large".into());
                    }
                    let ex = decode_runs(a.runs(), 0).ok_or("bad $MFT attribute list runs")?;
                    read_stream(vol, g, &ex, a.data_size()).map_err(|e| format!("read: {e}"))?
                }
            });
        }
    }
    // A fragmented $MFT keeps the rest of its $DATA runs in extension records.
    if let Some(list) = attr_list {
        let mut o = 0usize;
        let mut seen = Vec::new();
        while o + 0x1A <= list.len() {
            let ty = u32_at(&list, o);
            let len = u16_at(&list, o + 4) as usize;
            let name_len = list[o + 6];
            let rec_no = u64_at(&list, o + 0x10) & 0xFFFF_FFFF_FFFF;
            if len == 0 {
                break;
            }
            if ty == AT_DATA && name_len == 0 && rec_no != 0 && !seen.contains(&rec_no) {
                seen.push(rec_no);
                let rec = read_mft_record(vol, g, &extents, rec_no)?;
                for a in attrs(&rec) {
                    if a.ty == AT_DATA && a.name_len == 0 && a.nonres {
                        let ex = decode_runs(a.runs(), a.lowest_vcn()).ok_or("bad $MFT runs")?;
                        extents.extend(ex);
                        if a.lowest_vcn() == 0 {
                            data_size = Some(a.data_size());
                        }
                    }
                }
            }
            o += len;
        }
    }
    extents.sort_by_key(|e| e.vcn);
    extents.dedup();
    let data_size = data_size.ok_or("no $MFT data size")?;
    // caps before anything is sized from it: corrupt metadata must not abort the process
    let records = data_size / g.record;
    if records > g.volume_bytes / g.record || records > u32::MAX as u64 {
        return Err(format!("the $MFT claims an implausible {} records", format_count(records)));
    }
    let covered: u64 = extents.iter().map(|e| e.len).sum::<u64>() * g.cluster;
    if covered < data_size {
        return Err("the $MFT extents do not cover the table".into());
    }
    Ok(MftLayout { extents, data_size })
}

// ---------------------------------------------------------------------------------------------
// parsing the table

const F_USED: u8 = 1; // in-use base record
const F_DIR: u8 = 2;
const F_LINK: u8 = 4; // symlink / junction / mount point: skipped
const F_PLACEHOLDER: u8 = 8; // offline / recall-on-data-access per $STANDARD_INFORMATION: 0
const F_CLOUD: u8 = 16; // cloud-files reparse tag
const F_HOLLOW: u8 = 32; // the unnamed $DATA is sparse or has no clusters

/// Reparse tag -> F_LINK / F_CLOUD.
fn tag_flags(tag: u32) -> u8 {
    let mut f = 0;
    if tag & TAG_NAME_SURROGATE != 0 {
        f |= F_LINK;
    }
    if tag & 0xFFFF_0FFF == 0x9000_001A {
        f |= F_CLOUD; // IO_REPARSE_TAG_CLOUD, CLOUD_1 .. CLOUD_F
    }
    f
}

/// One record of the table, dense by record number. 40 bytes.
#[derive(Clone, Copy, Default)]
struct Entry {
    /// Logical size of the unnamed $DATA.
    size: u64,
    /// On-disk bytes of all its non-resident attributes.
    disk: u64,
    /// On-disk bytes of the file's data (`RecInfo::data_disk`).
    data_disk: u64,
    parent: u32,
    /// Into the UTF-8 names arena (per chunk while parsing, global after).
    name_off: u32,
    name_len: u16,
    seq: u16,
    /// The sequence number the name's parent reference expects (0 = any).
    parent_seq: u16,
    /// 0 = no name yet.
    rank: u8,
    flags: u8,
}

impl Entry {
    fn used(&self) -> bool {
        self.flags & F_USED != 0
    }
    fn is_dir(&self) -> bool {
        self.flags & F_DIR != 0
    }
    /// (on disk, logical), the walk's rules: placeholders flagged offline / recall-on-access
    /// count 0; a cloud file whose data is sparse or absent counts what it holds on disk in both
    /// measures (0 when dehydrated); everything else its allocation and its length.
    fn sizes(&self) -> (u64, u64) {
        if self.flags & F_PLACEHOLDER != 0 {
            (0, 0)
        } else if self.flags & (F_CLOUD | F_HOLLOW) == F_CLOUD | F_HOLLOW {
            (self.data_disk, self.data_disk)
        } else {
            (self.data_disk, self.size)
        }
    }
}

/// What an extension record adds to its base record.
struct Ext {
    base: u32,
    base_seq: u16,
    /// parent, parent sequence, name_off (chunk arena), len, rank
    name: Option<(u32, u16, u32, u16, u8)>,
    data_size: Option<u64>,
    data_hollow: Option<bool>,
    data_disk: u64,
    /// F_LINK / F_CLOUD from a $REPARSE_POINT stored in the extension
    tag_flags: u8,
    disk: u64,
}

#[derive(Default)]
struct ChunkOut {
    /// UTF-8 names, back to back.
    names: Vec<u8>,
    exts: Vec<Ext>,
    files: u64,
    dirs: u64,
    bytes: u64,
    /// In-use records, base and extension.
    in_use: u64,
    /// On-disk bytes of every non-resident attribute in the chunk (for the allocation check).
    disk_all: u64,
    extra_links: u64,
    /// Torn records, records in the wrong slot, and the records of chunks that could not be read.
    damaged: u64,
    failed_reads: u64,
}

/// Appends a UTF-16LE name as UTF-8 (at most 255 UTF-16 units, so <= 765 bytes).
fn push_name(names: &mut Vec<u8>, utf16le: &[u8]) -> (u32, u16) {
    let off = names.len();
    let units = utf16le.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]));
    for ch in char::decode_utf16(units) {
        let ch = ch.unwrap_or(char::REPLACEMENT_CHARACTER);
        let mut b = [0u8; 4];
        names.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
    }
    (off as u32, (names.len() - off) as u16)
}

fn to_u32(x: u64) -> u32 {
    u32::try_from(x).unwrap_or(u32::MAX)
}

/// Parses the records of one chunk into `ents`. `read_tag` fetches the reparse tag of a
/// non-resident $REPARSE_POINT from its runs.
fn parse_chunk(
    buf: &mut [u8],
    first_rec: usize,
    ents: &mut [Entry],
    rs: usize,
    read_tag: &dyn Fn(&[u8]) -> Option<u32>,
) -> ChunkOut {
    let mut out = ChunkOut::default();
    for (k, e) in ents.iter_mut().enumerate() {
        let rec_no = first_rec + k;
        let Some(rec) = buf.get_mut(k * rs..(k + 1) * rs) else { break };
        match &rec[0..4] {
            b"FILE" => {}
            b"BAAD" => {
                out.damaged += 1;
                continue;
            }
            _ => continue, // never used
        }
        if !record_ok(rec, rec_no as u64) {
            out.damaged += 1;
            continue;
        }
        let r = parse_record(rec);
        if !r.in_use {
            continue;
        }
        out.in_use += 1;
        if rec_no != BADCLUS {
            out.disk_all += r.disk;
        }
        let tag = r.reparse_tag.or_else(|| r.reparse_runs.and_then(read_tag));
        let tflags = tag.map_or(0, tag_flags);
        if r.ext {
            let name = r.name.map(|(p, ps, rank, nb)| {
                let (off, len) = push_name(&mut out.names, nb);
                (to_u32(p), ps, off, len, rank)
            });
            out.exts.push(Ext {
                base: to_u32(r.base),
                base_seq: r.base_seq,
                name,
                data_size: r.data_size,
                data_hollow: r.data_hollow,
                data_disk: r.data_disk,
                tag_flags: tflags,
                disk: r.disk,
            });
            continue;
        }
        let mut flags = F_USED | tflags;
        if r.dir {
            flags |= F_DIR;
        }
        if r.si_attrs & (ATTR_OFFLINE | ATTR_RECALL_ON_DATA_ACCESS) != 0 {
            flags |= F_PLACEHOLDER;
        }
        if r.data_hollow == Some(true) {
            flags |= F_HOLLOW;
        }
        *e = Entry {
            size: r.data_size.unwrap_or(0),
            disk: r.disk,
            data_disk: r.data_disk,
            seq: r.seq,
            flags,
            ..Default::default()
        };
        if let Some((p, ps, rank, nb)) = r.name {
            let (off, len) = push_name(&mut out.names, nb);
            e.parent = to_u32(p);
            e.parent_seq = ps;
            e.name_off = off;
            e.name_len = len;
            e.rank = rank;
        }
        out.extra_links += r.extra_links as u64;
        if r.dir {
            out.dirs += 1;
        } else {
            out.files += 1;
            out.bytes += e.sizes().0;
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// entry points

pub fn scan(root_path: &str, progress: &ScanProgress) -> Result<MftScan, String> {
    let b = root_path.as_bytes();
    if !(b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\') {
        return Err("not on a lettered drive".into());
    }
    let letter = b[0].to_ascii_uppercase() as char;
    if !crate::drives::is_local_volume(&root_path[..3]) {
        return Err(format!("{letter}: is not a local disk volume"));
    }
    match crate::drives::filesystem_name(&root_path[..3]) {
        Some(fs) if fs == "NTFS" => {}
        Some(fs) => return Err(format!("{letter}: is {fs}, not NTFS")),
        None => return Err(format!("{letter}: has no readable file system")),
    }
    let vol = RawVolume::open(letter).map_err(|e| match e.raw_os_error() {
        Some(5) => "reading the volume needs admin rights".to_string(), // ERROR_ACCESS_DENIED
        _ => format!("cannot open the volume: {e}"),
    })?;
    let used = super::volume_space(&root_path[..3]).map(|(t, f)| t.saturating_sub(f));
    scan_volume(&vol, root_path, progress, used)
}

pub(crate) fn scan_volume(
    vol: &dyn Volume,
    root_path: &str,
    progress: &ScanProgress,
    volume_used: Option<u64>,
) -> Result<MftScan, String> {
    let t0 = Instant::now();
    if let Ok(mut g) = progress.current.lock() {
        *g = "Reading the master file table…".into();
    }
    // 4096 bytes: a whole sector on 512-byte and on 4K-native disks
    let boot = read_any(vol, &BOOT_READ, 0, 512).map_err(|e| format!("cannot read the boot sector: {e}"))?;
    let g = parse_boot(&boot)?;
    let layout = mft_layout(vol, &g)?;
    let rs = g.record as usize;
    let n = (layout.data_size / g.record) as usize;
    if n <= FIRST_USER_RECORD {
        return Err("the MFT is too small".into());
    }
    let (chunk, buffers) = bulk_settings(&g);
    let rpc = chunk / rs;
    let plan = plan_chunks(&layout, &g, n, chunk);
    let n_chunks = plan.len();

    // a non-resident $REPARSE_POINT: its tag is the first 4 bytes of its first cluster
    let tag_reads = AtomicU64::new(0);
    let read_tag = |runs: &[u8]| -> Option<u32> {
        tag_reads.fetch_add(1, Relaxed);
        let lcn = decode_runs(runs, 0)?.into_iter().find_map(|e| e.lcn)?;
        let b = read_any(vol, &g, lcn.checked_mul(g.cluster)?, 4).ok()?;
        Some(u32_at(&b, 0))
    };

    // Read the table with many reads in flight; each completed chunk is parsed on the pool
    // while the next ones are read.
    let mut entries = vec![Entry::default(); n];
    let pool = BufPool::new(buffers, chunk);
    let outs_by_chunk: Vec<Mutex<Option<ChunkOut>>> = (0..n_chunks).map(|_| Mutex::new(None)).collect();
    let parse_nanos = AtomicU64::new(0);
    let done_chunks = AtomicUsize::new(0);
    let stats = {
        let mut slices: Vec<Option<&mut [Entry]>> = entries.chunks_mut(rpc).map(Some).collect();
        let (outs_by_chunk, parse_nanos, done_chunks, read_tag, plan) =
            (&outs_by_chunk, &parse_nanos, &done_chunks, &read_tag, &plan);
        rayon::scope(|sc| {
            vol.read_chunks(plan, &pool, &mut |ci, buf, ok| -> bool {
                if progress.cancelled() {
                    return false;
                }
                let Some(ents) = slices.get_mut(ci).and_then(Option::take) else { return true };
                let len = plan[ci].len;
                sc.spawn(move |_| {
                    let mut buf = buf;
                    let out = if ok {
                        let t = Instant::now();
                        let o = parse_chunk(&mut buf[..len], ci * rpc, ents, rs, read_tag);
                        parse_nanos.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                        o
                    } else {
                        // an unreadable chunk: its records count as damaged (see the gate)
                        ChunkOut { damaged: ents.len() as u64, failed_reads: 1, ..Default::default() }
                    };
                    drop(buf); // back to the reader first
                    progress.files.fetch_add(out.files, Relaxed);
                    progress.dirs.fetch_add(out.dirs, Relaxed);
                    progress.bytes.fetch_add(out.bytes, Relaxed);
                    let d = done_chunks.fetch_add(1, Relaxed) + 1;
                    if d % 16 == 0 {
                        if let Ok(mut g) = progress.current.try_lock() {
                            *g = format!("Reading the master file table… {}%", d * 100 / n_chunks.max(1));
                        }
                    }
                    if let Ok(mut slot) = outs_by_chunk[ci].lock() {
                        *slot = Some(out);
                    }
                });
                true
            })
        })
    };
    drop(pool);
    let outs: Vec<ChunkOut> =
        outs_by_chunk.into_iter().map(|m| m.into_inner().ok().flatten().unwrap_or_default()).collect();
    let t_read = t0.elapsed();

    // one names arena; entry offsets become global
    let bases: Vec<u32> = outs
        .iter()
        .scan(0u64, |acc, o| {
            let b = *acc;
            *acc += o.names.len() as u64;
            Some(b)
        })
        .map(to_u32)
        .collect();
    let total_names: usize = outs.iter().map(|o| o.names.len()).sum();
    if total_names >= u32::MAX as usize {
        return Err("too many names".into());
    }
    entries.par_chunks_mut(rpc).zip(bases.par_iter()).for_each(|(ents, &base)| {
        for e in ents.iter_mut().filter(|e| e.rank > 0) {
            e.name_off += base;
        }
    });
    let mut names: Vec<u8> = Vec::with_capacity(total_names);
    let mut exts = Vec::new();
    let (mut disk_all, mut extra_links, mut damaged, mut in_use, mut failed_reads) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for (o, &base) in outs.into_iter().zip(&bases) {
        names.extend_from_slice(&o.names);
        disk_all += o.disk_all;
        extra_links += o.extra_links;
        damaged += o.damaged;
        in_use += o.in_use;
        failed_reads += o.failed_reads;
        exts.extend(o.exts.into_iter().map(|mut x| {
            if let Some(nm) = x.name.as_mut() {
                nm.2 += base;
            }
            x
        }));
    }

    // extension records complete their base (if they still belong to it)
    let mut stale = 0u64;
    for x in &exts {
        let Some(e) = entries.get_mut(x.base as usize) else { continue };
        if !e.used() {
            continue;
        }
        if x.base_seq != 0 && x.base_seq != e.seq {
            stale += 1; // left over from an earlier file in that slot
            continue;
        }
        if let Some((parent, parent_seq, off, len, rank)) = x.name {
            if rank > e.rank {
                e.parent = parent;
                e.parent_seq = parent_seq;
                e.name_off = off;
                e.name_len = len;
                e.rank = rank;
            }
        }
        if let Some(s) = x.data_size {
            e.size = s;
        }
        if let Some(hollow) = x.data_hollow {
            e.flags = if hollow { e.flags | F_HOLLOW } else { e.flags & !F_HOLLOW };
        }
        // the unnamed $DATA's first extent lives in exactly one record, so adding is right
        e.data_disk += x.data_disk;
        e.flags |= x.tag_flags;
        e.disk += x.disk;
    }
    drop(exts);

    // Sanity gate: a table that parsed badly must not be shown; the caller walks instead.
    let root_rec = entries[ROOT];
    if !(root_rec.used() && root_rec.is_dir()) {
        return Err("the MFT has no root folder record".into());
    }
    if damaged > (in_use / 100).max(8) {
        return Err(format!(
            "{} of {} MFT records are damaged or unreadable",
            format_count(damaged),
            format_count(in_use)
        ));
    }
    if let Some(u) = volume_used {
        if !progress.cancelled() && disk_all < u / 2 {
            return Err(format!(
                "the MFT accounts for only {} of the {} the volume uses",
                format_size(disk_all),
                format_size(u)
            ));
        }
    }

    let t1 = Instant::now();
    let tree = Tree::new(&entries, &names);
    let mut notes = Vec::new();
    let drive_root = is_drive_root(root_path);
    let start = if drive_root { ROOT } else { tree.find(&root_path[3..])? };
    let mut root = tree.build(start, root_name(root_path), drive_root);
    let mut meta_size = 0u64;
    if drive_root && !progress.cancelled() {
        let (size, count) = tree.metadata_size();
        meta_size = size;
        // what the metadata occupies is the only meaningful size: both measures
        let node = Node {
            name: "NTFS metadata ($MFT, journals, folder indexes)".into(),
            size,
            other: size,
            files: count,
            dirs: 0,
            kind: Kind::Rest,
            denied: false,
            children: Vec::new(),
        };
        let children = std::mem::take(&mut root.children);
        let name = std::mem::take(&mut root.name);
        let (dirs, files): (Vec<Node>, Vec<Node>) = children.into_iter().partition(|c| c.kind == Kind::Dir);
        let mut files = files;
        files.push(node);
        root = finish_dir(name, dirs, files);
    }
    let t_build = t1.elapsed();

    // the table's totals, for the CLI and for checking the parser against the volume
    notes.push(format!(
        "MFT: {} records of {} B ({} in use, {}), {} damaged{}{}",
        format_count(n as u64),
        g.record,
        format_count(in_use),
        format_size(layout.data_size),
        format_count(damaged),
        if failed_reads > 0 { format!(" ({failed_reads} unreadable chunks)") } else { String::new() },
        if stale > 0 { format!(", {stale} stale extension records") } else { String::new() },
    ));
    let parse_cpu = parse_nanos.load(Relaxed) as f64 / 1e9;
    let per_sec = |bytes: u64, secs: f64| format_size((bytes as f64 / secs.max(1e-9)) as u64);
    notes.push(format!(
        "MFT timing: read {} in {:.2} s = {}/s ({}, {} x {} MB buffers); parse {:.2} s CPU on the pool = {}/s \
         per thread; reader waited {:.2} s for parsers; read+parse {:.2} s wall, tree {:.2} s; {} non-resident \
         reparse tags read",
        format_size(stats.bytes),
        stats.secs,
        per_sec(stats.bytes, stats.secs),
        stats.engine,
        buffers,
        chunk >> 20,
        parse_cpu,
        per_sec(stats.bytes, parse_cpu),
        stats.starved,
        t_read.as_secs_f64(),
        t_build.as_secs_f64(),
        format_count(tag_reads.load(Relaxed)),
    ));
    if drive_root {
        notes.push(format!(
            "MFT sizes: files {} on disk ({} logical) + NTFS metadata {} = {}; every allocated cluster per \
             the MFT: {}{}",
            format_size(root.size - meta_size),
            format_size(root.other - meta_size),
            format_size(meta_size),
            format_size(root.size),
            format_size(disk_all),
            match volume_used {
                Some(u) => format!(
                    "; volume reports {} used (allocation check off by {})",
                    format_size(u),
                    format_size(u.abs_diff(disk_all))
                ),
                None => String::new(),
            }
        ));
        let (orphans, orphan_bytes) = tree.orphans();
        notes.push(format!(
            "MFT: {} extra hard links counted once; {} orphaned records ({} on disk)",
            format_count(extra_links),
            format_count(orphans),
            format_size(orphan_bytes)
        ));
    } else {
        notes.push(
            "MFT folder scan: a file hard-linked into several folders is listed under one of its names \
             only, which may be outside this folder"
                .into(),
        );
    }
    Ok(MftScan { root, notes })
}

/// `diskscope --cli C:\ --mft-bench`: times the bulk read alone with several engines and
/// settings (the first 2 GB of the table each), then the parse alone, from memory. Needs admin.
pub fn bench(root_path: &str) -> Result<String, String> {
    use std::fmt::Write;
    let b = root_path.as_bytes();
    if !(b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':') {
        return Err("not on a lettered drive".into());
    }
    let letter = b[0].to_ascii_uppercase() as char;
    let vol = RawVolume::open(letter).map_err(|e| format!("cannot open the volume: {e}"))?;
    let boot = read_any(&vol, &BOOT_READ, 0, 512).map_err(|e| format!("cannot read the boot sector: {e}"))?;
    let g = parse_boot(&boot)?;
    let layout = mft_layout(&vol, &g)?;
    let rs = g.record as usize;
    let n = (layout.data_size / g.record) as usize;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "MFT of {letter}: {} in {} extents, {} records of {} B, cluster {} B, overlapped handle: {}",
        format_size(layout.data_size),
        layout.extents.len(),
        format_count(n as u64),
        g.record,
        g.cluster,
        if vol.overlapped.is_some() { "yes" } else { "no" }
    );
    let rate = |bytes: u64, secs: f64| format!("{}/s", format_size((bytes as f64 / secs.max(1e-9)) as u64));

    // 1. the read alone
    let n_bench = n.min(((2u64 << 30) / g.record) as usize);
    let configs: [(&str, usize, u64); 7] = [
        ("overlapped", 16, 4),
        ("overlapped", 32, 8),
        ("overlapped", 64, 1),
        ("overlapped", 4, 4),
        ("overlapped", 1, 4),
        ("sync", 1, 4),
        ("thread handles", 64, 1),
    ];
    for (engine, qd, mb) in configs {
        let chunk = ((mb << 20).max(g.cluster).max(g.record)) as usize;
        let plan = plan_chunks(&layout, &g, n_bench, chunk);
        let t = Instant::now();
        let (bytes, failed) = if engine == "thread handles" {
            // the old way: every pool thread reads with its own synchronous handle
            let failed = AtomicU64::new(0);
            let bytes: u64 = super::pool().install(|| {
                plan.par_iter()
                    .map_init(
                        || AlignedBuf::new(chunk),
                        |buf, p| {
                            prepare(buf, p);
                            for pc in &p.pieces {
                                if let Some(off) = pc.vol {
                                    if vol.read_at(off, &mut buf[pc.at..pc.at + pc.len]).is_err() {
                                        failed.fetch_add(1, Relaxed);
                                    }
                                }
                            }
                            p.len as u64
                        },
                    )
                    .sum()
            });
            (bytes, failed.into_inner())
        } else {
            let pool = BufPool::new(qd, chunk);
            let mut failed = 0u64;
            let mut done = |_ci: usize, buf: ChunkBuf<'_>, ok: bool| {
                failed += u64::from(!ok);
                drop(buf);
                true
            };
            let stats = match (&vol.overlapped, engine) {
                (Some((h, port)), "overlapped") => vol.read_chunks_iocp(h.0, port.0, &plan, &pool, &mut done),
                _ => read_chunks_sync(&vol, &plan, &pool, &mut done),
            };
            (stats.bytes, failed)
        };
        let secs = t.elapsed().as_secs_f64();
        let _ = writeln!(
            out,
            "  read   {engine:<15} {qd:>3} x {mb} MB: {} in {secs:.2} s = {}{}",
            format_size(bytes),
            rate(bytes, secs),
            if failed > 0 { format!(" ({failed} failed reads)") } else { String::new() }
        );
    }

    // 2. the parse alone: 512 MB read into memory first
    let chunk = (4u64 << 20).max(g.cluster).max(g.record) as usize;
    let n_mem = n.min(((512u64 << 20) / g.record) as usize);
    let plan = plan_chunks(&layout, &g, n_mem, chunk);
    let mut bufs: Vec<AlignedBuf> = Vec::with_capacity(plan.len());
    for p in &plan {
        let mut buf = AlignedBuf::new(chunk);
        prepare(&mut buf, p);
        for pc in &p.pieces {
            if let Some(off) = pc.vol {
                let _ = vol.read_at(off, &mut buf[pc.at..pc.at + pc.len]);
            }
        }
        bufs.push(buf);
    }
    let rpc = chunk / rs;
    let tag_reads = AtomicU64::new(0);
    let read_tag = |runs: &[u8]| -> Option<u32> {
        tag_reads.fetch_add(1, Relaxed);
        let lcn = decode_runs(runs, 0)?.into_iter().find_map(|e| e.lcn)?;
        let b = read_any(&vol, &g, lcn.checked_mul(g.cluster)?, 4).ok()?;
        Some(u32_at(&b, 0))
    };
    let mut ents = vec![Entry::default(); n_mem];
    let mut slices: Vec<&mut [Entry]> = ents.chunks_mut(rpc).collect();
    // one thread over the first 64 MB (parsing fixes records up in place, so each chunk is
    // parsed once: the pool gets the rest)
    let one = plan.len().min(16);
    let t = Instant::now();
    let mut one_bytes = 0u64;
    for ci in 0..one {
        parse_chunk(&mut bufs[ci][..plan[ci].len], ci * rpc, slices[ci], rs, &read_tag);
        one_bytes += plan[ci].len as u64;
    }
    let one_secs = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let rest_bytes: u64 = plan[one..].iter().map(|p| p.len as u64).sum();
    super::pool().install(|| {
        bufs[one..].par_iter_mut().zip(slices[one..].par_iter_mut()).enumerate().for_each(|(k, (buf, ents))| {
            let ci = one + k;
            parse_chunk(&mut buf[..plan[ci].len], ci * rpc, ents, rs, &read_tag);
        });
    });
    let pool_secs = t.elapsed().as_secs_f64();
    let _ = writeln!(
        out,
        "  parse  1 thread: {} in {one_secs:.2} s = {} ({:.2} us per record)",
        format_size(one_bytes),
        rate(one_bytes, one_secs),
        one_secs * 1e6 / (one_bytes as f64 / rs as f64).max(1.0)
    );
    let _ = writeln!(
        out,
        "  parse  pool ({} threads): {} in {pool_secs:.2} s = {}",
        super::pool().current_num_threads(),
        format_size(rest_bytes),
        rate(rest_bytes, pool_secs)
    );
    let _ = writeln!(out, "  non-resident reparse tags read while parsing: {}", format_count(tag_reads.into_inner()));
    Ok(out)
}

/// Parent -> children index over the parsed table.
struct Tree<'a> {
    e: &'a [Entry],
    names: &'a [u8],
    /// children of record i are kids[off[i]..off[i + 1]]
    off: Vec<u32>,
    kids: Vec<u32>,
}

impl<'a> Tree<'a> {
    fn new(e: &'a [Entry], names: &'a [u8]) -> Self {
        let n = e.len();
        let mut off = vec![0u32; n + 1];
        for i in 0..n {
            if let Some(p) = Self::parent_of(e, i) {
                off[p + 1] += 1;
            }
        }
        for i in 0..n {
            off[i + 1] += off[i];
        }
        let mut fill = off.clone();
        let mut kids = vec![0u32; off[n] as usize];
        for i in 0..n {
            if let Some(p) = Self::parent_of(e, i) {
                kids[fill[p] as usize] = i as u32;
                fill[p] += 1;
            }
        }
        Tree { e, names, off, kids }
    }

    /// The folder that lists record `i`, if `i` is a counted entry and its parent is a folder
    /// whose sequence number matches the reference (else the parent slot was reused: orphan).
    fn parent_of(e: &[Entry], i: usize) -> Option<usize> {
        let x = &e[i];
        if !x.used() || x.rank == 0 || x.flags & F_LINK != 0 || i == ROOT {
            return None;
        }
        let p = x.parent as usize;
        let seq_ok = |q: &Entry| x.parent_seq == 0 || q.seq == x.parent_seq;
        (p < e.len() && p != i && e[p].used() && e[p].is_dir() && seq_ok(&e[p])).then_some(p)
    }

    fn children(&self, i: usize) -> &[u32] {
        &self.kids[self.off[i] as usize..self.off[i + 1] as usize]
    }

    fn name_str(&self, i: usize) -> &str {
        let x = &self.e[i];
        let s = x.name_off as usize;
        // the arena only ever receives whole UTF-8 strings
        self.names.get(s..s + x.name_len as usize).and_then(|b| std::str::from_utf8(b).ok()).unwrap_or("")
    }

    fn name(&self, i: usize) -> Box<str> {
        self.name_str(i).into()
    }

    /// Record of the folder at `rel` (`Users\Sam`) below the root, matched case-insensitively.
    fn find(&self, rel: &str) -> Result<usize, String> {
        let mut cur = ROOT;
        for comp in rel.split('\\').filter(|c| !c.is_empty()) {
            let want = comp.to_lowercase();
            cur = self
                .children(cur)
                .iter()
                .map(|&k| k as usize)
                .find(|&k| self.e[k].is_dir() && self.name_str(k).to_lowercase() == want)
                .ok_or_else(|| format!("{comp} was not found in the MFT"))?;
        }
        Ok(cur)
    }

    fn build(&self, i: usize, name: Box<str>, drive_root: bool) -> Node {
        let mut files: Vec<(u64, u64, u32)> = Vec::new();
        let mut dirs: Vec<u32> = Vec::new();
        for &k in self.children(i) {
            if drive_root && i == ROOT && (k as usize) < FIRST_USER_RECORD {
                continue; // $MFT, $LogFile, … and $Extend: in the metadata node
            }
            let x = &self.e[k as usize];
            if x.is_dir() {
                dirs.push(k);
            } else {
                let (disk, logical) = x.sizes();
                files.push((disk, logical, k));
            }
        }
        let fnodes = file_nodes(&mut files, |&k| self.name(k as usize));
        drop(files);
        let one = |&k: &u32| self.build(k as usize, self.name(k as usize), false);
        let dnodes: Vec<Node> =
            if dirs.len() <= 1 { dirs.iter().map(one).collect() } else { dirs.par_iter().map(one).collect() };
        finish_dir(name, dnodes, fnodes)
    }

    /// On-disk size and count of the NTFS metadata: records 0..16 (not the root, not the
    /// bad-cluster map whose sparse stream spans the volume), everything under $Extend, and the
    /// index blocks of all folders.
    fn metadata_size(&self) -> (u64, u64) {
        let mut size = 0u64;
        let mut count = 0u64;
        for i in 0..FIRST_USER_RECORD.min(self.e.len()) {
            if i != ROOT && i != BADCLUS && self.e[i].used() {
                size += self.e[i].disk;
                count += 1;
            }
        }
        let mut in_extend = vec![false; 0];
        if self.e.len() > EXTEND && self.e[EXTEND].used() {
            in_extend = vec![false; self.e.len()];
            let mut stack = vec![EXTEND];
            while let Some(d) = stack.pop() {
                for &k in self.children(d) {
                    let k = k as usize;
                    if in_extend[k] {
                        continue;
                    }
                    in_extend[k] = true;
                    size += self.e[k].disk;
                    count += 1;
                    if self.e[k].is_dir() {
                        stack.push(k);
                    }
                }
            }
        }
        for (i, x) in self.e.iter().enumerate() {
            let meta = i < FIRST_USER_RECORD && i != ROOT || in_extend.get(i).copied().unwrap_or(false);
            if x.used() && x.is_dir() && !meta && x.flags & F_LINK == 0 {
                size += x.disk;
            }
        }
        (size, count)
    }

    /// (named files whose folder is gone or reused, their on-disk bytes)
    fn orphans(&self) -> (u64, u64) {
        let mut orphans = 0u64;
        let mut orphan_bytes = 0u64;
        for (i, x) in self.e.iter().enumerate() {
            if !x.used() || x.is_dir() || i < FIRST_USER_RECORD || x.flags & F_LINK != 0 {
                continue;
            }
            if Self::parent_of(self.e, i).is_none() && x.rank > 0 {
                orphans += 1;
                orphan_bytes += x.sizes().0;
            }
        }
        (orphans, orphan_bytes)
    }
}

#[cfg(test)]
mod tests {
    //! A small NTFS volume built byte by byte: boot sector, a two-extent $MFT (optionally with
    //! its second extent described in an extension record via $ATTRIBUTE_LIST), metadata files,
    //! folders, a hard link, a junction, a cloud placeholder, a file whose $DATA lives in an
    //! extension record, a deleted record, a torn record and an orphan.
    use super::*;

    const CS: usize = 4096;
    const RS: usize = 1024;
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;

    struct Mem(Vec<u8>);
    impl Volume for Mem {
        fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
            // the raw-volume rules: sector-aligned offset, length and buffer
            assert_eq!(off % 512, 0, "unaligned offset");
            assert_eq!(buf.len() % 512, 0, "unaligned length");
            assert_eq!(buf.as_ptr() as usize % 512, 0, "unaligned buffer");
            let s = off as usize;
            let src = self.0.get(s..s + buf.len()).ok_or(io::ErrorKind::UnexpectedEof)?;
            buf.copy_from_slice(src);
            Ok(())
        }
    }

    fn pad8(mut v: Vec<u8>) -> Vec<u8> {
        while v.len() % 8 != 0 {
            v.push(0);
        }
        v
    }

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect()
    }

    fn resident(ty: u32, name: &str, value: &[u8]) -> Vec<u8> {
        let nm = utf16(name);
        let name_off = 0x18usize;
        let value_off = (name_off + nm.len()).next_multiple_of(8);
        let mut a = vec![0u8; value_off];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[8] = 0;
        a[9] = (nm.len() / 2) as u8;
        a[0x0A..0x0C].copy_from_slice(&(name_off as u16).to_le_bytes());
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&(value_off as u16).to_le_bytes());
        a[name_off..name_off + nm.len()].copy_from_slice(&nm);
        a.extend_from_slice(value);
        let mut a = pad8(a);
        let len = a.len() as u32;
        a[4..8].copy_from_slice(&len.to_le_bytes());
        a
    }

    #[allow(clippy::too_many_arguments)]
    fn nonresident(ty: u32, name: &str, flags: u16, vcn: (u64, u64), alloc: u64, size: u64, total: Option<u64>, runs: &[u8]) -> Vec<u8> {
        let nm = utf16(name);
        let hdr = if total.is_some() { 0x48 } else { 0x40 };
        let name_off = hdr;
        let runs_off = (name_off + nm.len()).next_multiple_of(8);
        let mut a = vec![0u8; runs_off];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[8] = 1;
        a[9] = (nm.len() / 2) as u8;
        a[0x0A..0x0C].copy_from_slice(&(name_off as u16).to_le_bytes());
        a[0x0C..0x0E].copy_from_slice(&flags.to_le_bytes());
        a[0x10..0x18].copy_from_slice(&vcn.0.to_le_bytes());
        a[0x18..0x20].copy_from_slice(&vcn.1.to_le_bytes());
        a[0x20..0x22].copy_from_slice(&(runs_off as u16).to_le_bytes());
        a[0x28..0x30].copy_from_slice(&alloc.to_le_bytes());
        a[0x30..0x38].copy_from_slice(&size.to_le_bytes());
        a[0x38..0x40].copy_from_slice(&size.min(alloc).to_le_bytes());
        if let Some(t) = total {
            a[0x40..0x48].copy_from_slice(&t.to_le_bytes());
        }
        a[name_off..name_off + nm.len()].copy_from_slice(&nm);
        a.extend_from_slice(runs);
        a.push(0);
        let mut a = pad8(a);
        let len = a.len() as u32;
        a[4..8].copy_from_slice(&len.to_le_bytes());
        a
    }

    fn si(attrs: u32) -> Vec<u8> {
        let mut v = vec![0u8; 0x48];
        v[0x20..0x24].copy_from_slice(&attrs.to_le_bytes());
        resident(AT_STANDARD_INFORMATION, "", &v)
    }

    fn fname(parent: u64, name: &str, ns: u8) -> Vec<u8> {
        fname_seq(parent, 1, name, ns)
    }

    /// A $FILE_NAME whose parent reference expects sequence number `seq`.
    fn fname_seq(parent: u64, seq: u64, name: &str, ns: u8) -> Vec<u8> {
        let nm = utf16(name);
        let mut v = vec![0u8; 0x42];
        v[0..8].copy_from_slice(&(parent | (seq << 48)).to_le_bytes());
        v[0x40] = (nm.len() / 2) as u8;
        v[0x41] = ns;
        v.extend_from_slice(&nm);
        resident(AT_FILE_NAME, "", &v)
    }

    fn reparse(tag: u32) -> Vec<u8> {
        let mut rp = vec![0u8; 16];
        rp[0..4].copy_from_slice(&tag.to_le_bytes());
        resident(AT_REPARSE_POINT, "", &rp)
    }

    /// Mapping pairs with 4-byte length and 4-byte signed delta fields.
    fn runs(pairs: &[(u64, Option<i64>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for &(len, delta) in pairs {
            match delta {
                Some(d) => {
                    out.push(0x44);
                    out.extend_from_slice(&(len as u32).to_le_bytes());
                    out.extend_from_slice(&(d as i32).to_le_bytes());
                }
                None => {
                    out.push(0x04);
                    out.extend_from_slice(&(len as u32).to_le_bytes());
                }
            }
        }
        out
    }

    /// A 1 KB FILE record (sequence number 1) with fixups applied, as on disk.
    fn record(rec_no: u32, flags: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; RS];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes()); // update sequence array offset
        r[6..8].copy_from_slice(&3u16.to_le_bytes()); // 1 + 2 sectors
        r[0x10..0x12].copy_from_slice(&1u16.to_le_bytes());
        r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        r[0x1C..0x20].copy_from_slice(&(RS as u32).to_le_bytes());
        r[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        r[0x2C..0x30].copy_from_slice(&rec_no.to_le_bytes());
        let mut off = 0x38;
        for a in attrs {
            r[off..off + a.len()].copy_from_slice(a);
            off += a.len();
        }
        r[off..off + 4].copy_from_slice(&AT_END.to_le_bytes());
        off += 8;
        r[0x18..0x1C].copy_from_slice(&(off as u32).to_le_bytes());
        // protect: move the last two bytes of each 512-byte block into the array
        let usn = [0x07u8, 0x00];
        r[0x30..0x32].copy_from_slice(&usn);
        for i in 0..2 {
            let end = (i + 1) * 512 - 2;
            r[0x32 + 2 * i] = r[end];
            r[0x33 + 2 * i] = r[end + 1];
            r[end..end + 2].copy_from_slice(&usn);
        }
        r
    }

    fn torn(rec_no: u32) -> Vec<u8> {
        let mut t = record(rec_no, FILE, 0, &[si(0x20), fname(25, "torn.txt", 3), resident(AT_DATA, "", &[0u8; 60])]);
        t[1022] = 0x55;
        t
    }

    const DIR: u16 = REC_IN_USE | REC_DIRECTORY;
    const FILE: u16 = REC_IN_USE;
    const TOTAL_CLUSTERS: u64 = 48;
    /// Cluster holding the non-resident reparse data of Junction2.
    const REPARSE_LCN: u64 = 40;

    /// $MFT extents (vcn, lcn, clusters): 64 records = 16 clusters in two pieces.
    fn mft_extents(with_list: bool) -> Vec<(u64, u64, u64)> {
        if with_list { vec![(0, 4, 5), (5, 12, 11)] } else { vec![(0, 4, 2), (2, 10, 14)] }
    }

    /// The test volume's records. `with_list`: the $MFT's second extent is described in
    /// extension record 16, reached through an $ATTRIBUTE_LIST in record 0.
    fn records(with_list: bool) -> Vec<(u32, Vec<u8>)> {
        let mft_alloc = 16 * CS as u64;
        let mft_size = 64 * RS as u64;
        let mut recs: Vec<(u32, Vec<u8>)> = Vec::new();

        let rec0_data = if with_list {
            let mut list = Vec::new();
            for (ty, vcn, rec) in [(AT_STANDARD_INFORMATION, 0u64, 0u64), (AT_FILE_NAME, 0, 0), (AT_DATA, 0, 0), (AT_DATA, 5, 16)] {
                let mut e = vec![0u8; 0x20];
                e[0..4].copy_from_slice(&ty.to_le_bytes());
                e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
                e[8..16].copy_from_slice(&vcn.to_le_bytes());
                e[0x10..0x18].copy_from_slice(&(rec | (1 << 48)).to_le_bytes());
                list.extend(e);
            }
            let ext_piece = nonresident(AT_DATA, "", 0, (5, 15), 0, 0, None, &runs(&[(11, Some(12))]));
            recs.push((16, record(16, FILE, 1 << 48, &[ext_piece])));
            vec![
                si(0x6),
                resident(AT_ATTRIBUTE_LIST, "", &list),
                fname(5, "$MFT", 3),
                nonresident(AT_DATA, "", 0, (0, 4), mft_alloc, mft_size, None, &runs(&[(5, Some(4))])),
            ]
        } else {
            vec![
                si(0x6),
                fname(5, "$MFT", 3),
                nonresident(AT_DATA, "", 0, (0, 15), mft_alloc, mft_size, None, &runs(&[(2, Some(4)), (14, Some(6))])),
            ]
        };
        recs.push((0, record(0, FILE, 0, &rec0_data)));
        recs.push((2, record(2, FILE, 0, &[si(6), fname(5, "$LogFile", 3), nonresident(AT_DATA, "", 0, (0, 16383), 64 * MIB, 64 * MIB, None, &runs(&[(16384, Some(100))]))])));
        recs.push((5, record(5, DIR, 0, &[si(0x16), fname(5, ".", 3), nonresident(0xA0, "$I30", 0, (0, 0), 4096, 4096, None, &runs(&[(1, Some(200))]))])));
        recs.push((8, record(8, FILE, 0, &[si(6), fname(5, "$BadClus", 3), nonresident(AT_DATA, "$Bad", ATTR_FLAG_SPARSE, (0, TOTAL_CLUSTERS - 1), TOTAL_CLUSTERS * CS as u64, TOTAL_CLUSTERS * CS as u64, Some(0), &runs(&[(TOTAL_CLUSTERS, None)]))])));
        recs.push((11, record(11, DIR, 0, &[si(6), fname(5, "$Extend", 3)])));
        recs.push((18, record(18, FILE, 0, &[si(0x26), fname(11, "$UsnJrnl", 3), nonresident(AT_DATA, "$J", ATTR_FLAG_SPARSE, (0, 16383), 64 * MIB, 64 * MIB, Some(32 * MIB), &runs(&[(8192, None), (8192, Some(300))]))])));
        // user data
        recs.push((24, record(24, DIR, 0, &[si(0x10), fname(5, "Users", 3), nonresident(0xA0, "$I30", 0, (0, 1), 8192, 8192, None, &runs(&[(2, Some(210))]))])));
        recs.push((25, record(25, DIR, 0, &[si(0x10), fname(24, "Sam", 3)])));
        recs.push((26, record(26, FILE, 0, &[si(0x20), fname(25, "big.bin", 1), fname(25, "BIG~1.BIN", 2), nonresident(AT_DATA, "", 0, (0, 5 * GIB / CS as u64 - 1), 5 * GIB, 5 * GIB, None, &runs(&[(5 * GIB / CS as u64, Some(1000))]))])));
        // hard link: POSIX name in Users first, Win32 name in Sam second -> counted once, in Sam
        recs.push((27, record(27, FILE, 0, &[si(0x20), fname(24, "link.txt", 0), fname(25, "a.txt", 1), resident(AT_DATA, "", &[b'x'; 100])])));
        // junction at the root: skipped
        recs.push((28, record(28, DIR, 0, &[si(0x410), fname(5, "Junction", 3), reparse(0xA000_0003)])));
        // junction whose reparse data is non-resident (in cluster REPARSE_LCN): skipped
        recs.push((23, record(23, DIR, 0, &[si(0x410), fname(5, "Junction2", 3), nonresident(AT_REPARSE_POINT, "", 0, (0, 0), 4096, 24, None, &runs(&[(1, Some(REPARSE_LCN as i64))]))])));
        // online-only cloud file flagged in $STANDARD_INFORMATION: counts 0
        recs.push((29, record(29, FILE, 0, &[si(0x40_0020), fname(25, "cloud.dat", 3), nonresident(AT_DATA, "", ATTR_FLAG_SPARSE, (0, 262143), GIB, GIB, Some(0), &runs(&[(262144, None)]))])));
        // dehydrated cloud file known only by its reparse tag: counts what is on disk (0)
        recs.push((21, record(21, FILE, 0, &[si(0x20), fname(25, "cloud2.dat", 3), reparse(0x9000_101A), nonresident(AT_DATA, "", ATTR_FLAG_SPARSE, (0, 524287), 2 * GIB, 2 * GIB, Some(0), &runs(&[(524288, None)]))])));
        // hydrated cloud file: counts its size
        recs.push((22, record(22, FILE, 0, &[si(0x20), fname(25, "cloud3.dat", 3), reparse(0x9000_001A), nonresident(AT_DATA, "", 0, (0, 1), 8192, 8000, None, &runs(&[(2, Some(3000))]))])));
        // $DATA in an extension record
        recs.push((30, record(30, FILE, 0, &[si(0x20), resident(AT_ATTRIBUTE_LIST, "", &[0u8; 8]), fname(25, "frag.bin", 3)])));
        recs.push((31, record(31, FILE, 30 | (1 << 48), &[nonresident(AT_DATA, "", 0, (0, 189), 778_240, 777_777, None, &runs(&[(190, Some(2000))]))])));
        // symlink whose $REPARSE_POINT lives in an extension record: skipped
        recs.push((32, record(32, FILE, 0, &[si(0x420), resident(AT_ATTRIBUTE_LIST, "", &[0u8; 8]), fname(25, "sym.txt", 3)])));
        recs.push((33, record(33, FILE, 32 | (1 << 48), &[reparse(0xA000_000C)])));
        // parent reference with a stale sequence number (Sam's slot is at sequence 1): orphan
        recs.push((34, record(34, FILE, 0, &[si(0x20), fname_seq(25, 2, "wrongseq.txt", 3), resident(AT_DATA, "", &[0u8; 40])])));
        // an extension record left over from an earlier file in slot 35 (base sequence 2): ignored
        recs.push((35, record(35, FILE, 0, &[si(0x20), resident(AT_ATTRIBUTE_LIST, "", &[0u8; 8]), fname(25, "staleext.bin", 3)])));
        recs.push((36, record(36, FILE, 35 | (2 << 48), &[nonresident(AT_DATA, "", 0, (0, 0), 4096, 999, None, &runs(&[(1, Some(2500))]))])));
        // CompactOS-compressed: the main stream is sparse, the bytes sit in WofCompressedData
        recs.push((38, record(38, FILE, 0, &[
            si(0x620),
            fname(25, "wof.exe", 3),
            nonresident(AT_DATA, "", ATTR_FLAG_SPARSE, (0, 24), 102_400, 100_000, Some(0), &runs(&[(25, None)])),
            nonresident(AT_DATA, "WofCompressedData", 0, (0, 9), 40_960, 40_000, None, &runs(&[(10, Some(3500))])),
        ])));
        // a record sitting in the wrong slot (claims to be 99): damaged
        recs.push((37, record(99, FILE, 0, &[si(0x20), fname(25, "misplaced.txt", 3), resident(AT_DATA, "", &[0u8; 30])])));
        // deleted (not in use): skipped
        recs.push((17, record(17, 0, 0, &[si(0x20), fname(25, "ghost.txt", 3), resident(AT_DATA, "", &[0u8; 50])])));
        // torn write: fixup mismatch -> damaged
        recs.push((19, torn(19)));
        // parent out of range: orphan
        recs.push((20, record(20, FILE, 0, &[si(0x20), fname(99, "orphan.txt", 3), resident(AT_DATA, "", &[0u8; 70])])));
        recs
    }

    /// Replaces record `no` (or adds it).
    fn set(recs: &mut Vec<(u32, Vec<u8>)>, no: u32, bytes: Vec<u8>) {
        recs.retain(|(n, _)| *n != no);
        recs.push((no, bytes));
    }

    fn assemble(with_list: bool, recs: Vec<(u32, Vec<u8>)>) -> Vec<u8> {
        let mut img = vec![0u8; TOTAL_CLUSTERS as usize * CS];
        // boot sector
        img[3..11].copy_from_slice(b"NTFS    ");
        img[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        img[0x0D] = 8;
        img[0x28..0x30].copy_from_slice(&(TOTAL_CLUSTERS * 8).to_le_bytes());
        img[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        img[0x40] = 0xF6; // 2^10 = 1024-byte records
        // Junction2's reparse data
        let rp = REPARSE_LCN as usize * CS;
        img[rp..rp + 4].copy_from_slice(&0xA000_0003u32.to_le_bytes());
        let extents = mft_extents(with_list);
        for (no, bytes) in recs {
            let x = no as u64 * RS as u64;
            let (vcn, lcn, _) = extents.iter().copied().find(|&(v, _, l)| v * CS as u64 <= x && x < (v + l) * CS as u64).unwrap();
            let off = (lcn * CS as u64 + x - vcn * CS as u64) as usize;
            img[off..off + RS].copy_from_slice(&bytes);
        }
        img
    }

    fn image(with_list: bool) -> Vec<u8> {
        assemble(with_list, records(with_list))
    }

    fn image_edit(edit: impl FnOnce(&mut Vec<(u32, Vec<u8>)>)) -> Vec<u8> {
        let mut recs = records(false);
        edit(&mut recs);
        assemble(false, recs)
    }

    fn child<'a>(n: &'a Node, name: &str) -> &'a Node {
        n.children.iter().find(|c| &*c.name == name).unwrap_or_else(|| panic!("{name} missing under {}", n.name))
    }

    fn has(n: &Node, name: &str) -> bool {
        n.children.iter().any(|c| &*c.name == name)
    }

    fn note(s: &MftScan, text: &str) -> bool {
        s.notes.iter().any(|n| n.contains(text))
    }

    fn scan_ok(img: Vec<u8>, path: &str) -> MftScan {
        let p = ScanProgress::new();
        scan_volume(&Mem(img), path, &p, None).unwrap()
    }

    fn scan_err(img: Vec<u8>, used: Option<u64>) -> String {
        let p = ScanProgress::new();
        match scan_volume(&Mem(img), "T:\\", &p, used) {
            Ok(_) => panic!("expected the scan to be refused"),
            Err(e) => e,
        }
    }

    fn check_drive(with_list: bool) {
        let vol = Mem(image(with_list));
        let p = ScanProgress::new();
        let s = scan_volume(&vol, "T:\\", &p, None).unwrap();
        let root = &s.root;
        assert_eq!(&*root.name, "T:");
        let names: Vec<&str> = root.children.iter().map(|c| &*c.name).collect();
        assert_eq!(names.len(), 2, "{names:?}"); // Users + the metadata node; junctions skipped
        let users = child(root, "Users");
        assert_eq!(users.children.len(), 1, "the hard link is counted once, under Sam");
        let sam = child(users, "Sam");
        // (on disk, logical)
        let sz = |name: &str| (child(sam, name).size, child(sam, name).other);
        assert_eq!(sz("big.bin"), (5 * GIB, 5 * GIB));
        assert_eq!(sz("a.txt"), (0, 100), "resident data takes no clusters");
        assert_eq!(sz("frag.bin"), (778_240, 777_777), "$DATA from an extension record");
        assert_eq!(sz("cloud.dat"), (0, 0));
        assert_eq!(sz("cloud2.dat"), (0, 0));
        assert_eq!(sz("cloud3.dat"), (8192, 8000));
        assert_eq!(sz("staleext.bin"), (0, 0));
        assert_eq!(sz("wof.exe"), (40_960, 100_000), "CompactOS: the bytes are in WofCompressedData");
        assert_eq!(sam.children.len(), 8, "{:?}", sam.children.iter().map(|c| &*c.name).collect::<Vec<_>>());
        assert_eq!(sam.files, 8);
        // both measures are summed exactly at every level
        assert_eq!(sam.size, 5 * GIB + 778_240 + 8192 + 40_960);
        assert_eq!(sam.other, 5 * GIB + 100 + 777_777 + 8000 + 100_000);
        fn sums(n: &Node) {
            if n.kind == Kind::Dir {
                assert_eq!(n.size, n.children.iter().map(|c| c.size).sum::<u64>(), "{}", n.name);
                assert_eq!(n.other, n.children.iter().map(|c| c.other).sum::<u64>(), "{}", n.name);
                assert_eq!(n.files, n.children.iter().map(|c| c.files).sum::<u64>(), "{}", n.name);
                n.children.iter().for_each(sums);
            }
        }
        sums(root);
        assert_eq!(users.dirs, 1);
        let meta = root.children.iter().find(|c| c.kind == Kind::Rest).unwrap();
        // $MFT 64 KB + $LogFile 64 MB + $UsnJrnl:$J 32 MB on disk + root index 4 KB + Users index 8 KB
        assert_eq!(meta.size, 16 * CS as u64 + 64 * MIB + 32 * MIB + 4096 + 8192);
        assert_eq!(meta.other, meta.size);
        assert_eq!(root.size, sam.size + meta.size);
        assert_eq!(root.other, sam.other + meta.size);
        assert_eq!(p.files.load(Relaxed), 15); // in-use base records that are files, incl. metafiles
        assert!(note(&s, " 2 damaged"), "{:?}", s.notes); // torn + wrong slot
        assert!(note(&s, "1 extra hard links"), "{:?}", s.notes);
        assert!(note(&s, "2 orphaned"), "{:?}", s.notes); // out of range + stale parent sequence
        assert!(note(&s, "1 stale extension records"), "{:?}", s.notes);
    }

    #[test]
    fn drive_from_a_built_volume() {
        check_drive(false);
    }

    /// The hard-linked a.txt (names in Users and in Sam) lands at the same name every time, and
    /// both drives' trees are identical across runs.
    #[test]
    fn hard_links_once_and_deterministic() {
        let dump = |s: &MftScan| {
            fn go(n: &Node, d: usize, out: &mut String) {
                out.push_str(&format!("{}{} {} {} {}\n", " ".repeat(d), n.name, n.size, n.other, n.files));
                n.children.iter().for_each(|c| go(c, d + 1, out));
            }
            let mut out = String::new();
            go(&s.root, 0, &mut out);
            out
        };
        let a = scan_ok(image(false), "T:\\");
        let b = scan_ok(image(false), "T:\\");
        assert_eq!(dump(&a), dump(&b));
        let users = child(&a.root, "Users");
        assert!(!has(users, "link.txt"));
        assert_eq!(child(child(users, "Sam"), "a.txt").other, 100);
        assert_eq!(users.files, 8, "unique files");
    }

    #[test]
    fn mft_with_attribute_list() {
        check_drive(true);
    }

    #[test]
    fn folder_from_a_built_volume() {
        let s = scan_ok(image(false), "T:\\users\\SAM");
        assert_eq!(&*s.root.name, "SAM");
        assert_eq!(s.root.other, 5 * GIB + 100 + 777_777 + 8000 + 100_000);
        assert_eq!(s.root.size, 5 * GIB + 778_240 + 8192 + 40_960);
        assert!(s.root.children.iter().all(|c| c.kind == Kind::File));
        assert!(note(&s, "hard-linked"));
        let p = ScanProgress::new();
        assert!(scan_volume(&Mem(image(false)), "T:\\nope", &p, None).is_err());
    }

    /// A table that parses badly is refused (the caller then walks).
    #[test]
    fn gate_refuses_bad_tables() {
        // the root folder record is not in use
        let e = scan_err(image_edit(|r| set(r, 5, record(5, 0, 0, &[si(0x16), fname(5, ".", 3)]))), None);
        assert!(e.contains("root folder"), "{e}");
        // the root folder record is not a folder
        let e = scan_err(image_edit(|r| set(r, 5, record(5, FILE, 0, &[si(0x16), fname(5, ".", 3)]))), None);
        assert!(e.contains("root folder"), "{e}");
        // too many damaged records (12 of ~30 in use)
        let e = scan_err(image_edit(|r| (40..50).for_each(|n| set(r, n, torn(n)))), None);
        assert!(e.contains("damaged"), "{e}");
        // records in the wrong slots count as damaged too
        let e = scan_err(image_edit(|r| (40..50).for_each(|n| set(r, n, record(n + 1, FILE, 0, &[si(0x20)])))), None);
        assert!(e.contains("damaged"), "{e}");
        // the table explains far less than the volume uses
        let e = scan_err(image(false), Some(1 << 40));
        assert!(e.contains("accounts for only"), "{e}");
        // ... but a plausible used figure passes
        let p = ScanProgress::new();
        assert!(scan_volume(&Mem(image(false)), "T:\\", &p, Some(6 * GIB)).is_ok());
        // an unreadable chunk (here: the table's second extent) is not an abort, it is refused
        struct Failing(Mem);
        impl Volume for Failing {
            fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
                if off == 10 * CS as u64 && buf.len() > RS {
                    return Err(io::ErrorKind::Other.into());
                }
                self.0.read_at(off, buf)
            }
        }
        let p = ScanProgress::new();
        let e = match scan_volume(&Failing(Mem(image(false))), "T:\\", &p, None) {
            Ok(_) => panic!("expected the scan to be refused"),
            Err(e) => e,
        };
        assert!(e.contains("root folder") || e.contains("damaged"), "{e}");
    }

    /// Reparse tags that are non-resident or in extension records; cloud files.
    #[test]
    fn reparse_points_and_cloud_files() {
        let s = scan_ok(image(false), "T:\\");
        assert!(!has(&s.root, "Junction"), "resident junction skipped");
        assert!(!has(&s.root, "Junction2"), "non-resident junction skipped");
        let sam = child(child(&s.root, "Users"), "Sam");
        assert!(!has(sam, "sym.txt"), "symlink tagged in an extension record skipped");
        let c2 = child(sam, "cloud2.dat");
        assert_eq!((c2.size, c2.other), (0, 0), "dehydrated cloud file by tag");
        assert_eq!(child(sam, "cloud3.dat").other, 8000, "hydrated cloud file");
        assert!(tag_flags(0x9000_F01A) & F_CLOUD != 0 && tag_flags(0x9000_001A) & F_LINK == 0);
        assert!(tag_flags(0xA000_0003) & F_LINK != 0 && tag_flags(0x8000_001B) == 0);
    }

    /// Absurd sizes are refused before anything is allocated from them.
    #[test]
    fn caps_refuse_absurd_sizes() {
        let huge_mft = image_edit(|r| {
            let data = nonresident(AT_DATA, "", 0, (0, 15), 1 << 50, 1 << 50, None, &runs(&[(2, Some(4)), (14, Some(6))]));
            set(r, 0, record(0, FILE, 0, &[si(6), fname(5, "$MFT", 3), data]));
        });
        let e = scan_err(huge_mft, None);
        assert!(e.contains("implausible"), "{e}");
        let huge_list = image_edit(|r| {
            let list = nonresident(AT_ATTRIBUTE_LIST, "", 0, (0, 0), 1 << 30, 1 << 30, None, &runs(&[(1, Some(41))]));
            let data = nonresident(AT_DATA, "", 0, (0, 15), 16 * CS as u64, 64 * RS as u64, None, &runs(&[(2, Some(4)), (14, Some(6))]));
            set(r, 0, record(0, FILE, 0, &[si(6), list, fname(5, "$MFT", 3), data]));
        });
        let e = scan_err(huge_list, None);
        assert!(e.contains("attribute list"), "{e}");
    }

    /// Sequence numbers on parent and base references.
    #[test]
    fn sequence_numbers() {
        let s = scan_ok(image(false), "T:\\");
        let sam = child(child(&s.root, "Users"), "Sam");
        assert!(!has(sam, "wrongseq.txt"), "parent reference from an earlier file in that slot");
        assert_eq!(child(sam, "staleext.bin").other, 0, "stale extension record ignored");
        assert!(note(&s, "1 stale extension records"), "{:?}", s.notes);
        // the same file with the right sequence number is listed
        let s = scan_ok(
            image_edit(|r| set(r, 34, record(34, FILE, 0, &[si(0x20), fname_seq(25, 1, "rightseq.txt", 3), resident(AT_DATA, "", &[0u8; 40])]))),
            "T:\\",
        );
        let sam = child(child(&s.root, "Users"), "Sam");
        assert_eq!(child(sam, "rightseq.txt").other, 40);
    }

    #[test]
    fn boot_and_runs() {
        let img = image(false);
        let g = parse_boot(&img[..512]).unwrap();
        assert_eq!((g.sector, g.cluster, g.record, g.mft_lcn), (512, 4096, 1024, 4));
        // negative deltas and sparse runs
        let r = [0x21, 0x10, 0x00, 0x01, 0x11, 0x05, 0xFE, 0x01, 0x08, 0x00];
        let ex = decode_runs(&r, 0).unwrap();
        assert_eq!(
            ex,
            vec![
                Extent { vcn: 0, lcn: Some(256), len: 16 },
                Extent { vcn: 16, lcn: Some(254), len: 5 },
                Extent { vcn: 21, lcn: None, len: 8 },
            ]
        );
        assert!(decode_runs(&[0x44, 1, 0, 0], 0).is_none()); // truncated
        assert!(parse_boot(&[0u8; 512]).is_err());
    }

    /// A volume image file (no alignment rules).
    struct ImageFile(std::fs::File);
    impl Volume for ImageFile {
        fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
            use std::os::windows::fs::FileExt;
            let mut done = 0;
            while done < buf.len() {
                let n = self.0.seek_read(&mut buf[done..], off + done as u64)?;
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                done += n;
            }
            Ok(())
        }
    }

    /// Used bytes per the volume's own $Bitmap (record 6), to check the allocation total.
    fn used_from_bitmap(vol: &dyn Volume) -> u64 {
        let boot = read_any(vol, &BOOT_READ, 0, 512).unwrap();
        let g = parse_boot(&boot).unwrap();
        let layout = mft_layout(vol, &g).unwrap();
        let rec = read_mft_record(vol, &g, &layout.extents, 6).unwrap();
        let a = attrs(&rec).find(|a| a.ty == AT_DATA && a.name_len == 0).unwrap();
        let bits = match a.value() {
            Some(v) => v.to_vec(),
            None => read_stream(vol, &g, &decode_runs(a.runs(), 0).unwrap(), a.data_size()).unwrap(),
        };
        let set: u64 = bits.iter().map(|b| b.count_ones() as u64).sum();
        set * g.cluster
    }

    /// A real NTFS image (made with mkntfs + ntfs-3g) given by `DISKSCOPE_TEST_IMAGE`.
    /// `cargo test real_image -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_image() {
        let Ok(path) = std::env::var("DISKSCOPE_TEST_IMAGE") else { return };
        let vol = ImageFile(std::fs::File::open(&path).unwrap());
        let used = used_from_bitmap(&vol);
        let p = ScanProgress::new();
        let t = Instant::now();
        let s = scan_volume(&vol, "T:\\", &p, Some(used)).unwrap();
        println!("scanned {path} in {:.2} s", t.elapsed().as_secs_f64());
        for n in &s.notes {
            println!("note: {n}");
        }
        fn dump(n: &Node, level: usize, max_level: usize) {
            println!("{}{} {:?} size={} files={} dirs={}", "  ".repeat(level), n.name, n.kind, n.size, n.files, n.dirs);
            if level < max_level {
                for c in n.children.iter().take(12) {
                    dump(c, level + 1, max_level);
                }
            }
        }
        dump(&s.root, 0, 1);
        // The copied trees must match their sources, counted name by name (the copy turned any
        // hard links in the sources into separate files).
        fn every_name(p: &std::path::Path) -> (u64, u64, u64) {
            let (mut bytes, mut files, mut dirs) = (0, 0, 0);
            for e in std::fs::read_dir(p).unwrap().flatten() {
                let m = e.metadata().unwrap();
                if m.is_dir() {
                    let (b, f, d) = every_name(&e.path());
                    (bytes, files, dirs) = (bytes + b, files + f, dirs + d + 1);
                } else {
                    (bytes, files) = (bytes + m.len(), files + 1);
                }
            }
            (bytes, files, dirs)
        }
        // DISKSCOPE_TEST_SOURCES: `;`-separated folders that were copied to the image root
        let sources = std::env::var("DISKSCOPE_TEST_SOURCES").unwrap_or_default();
        for src in sources.split(';').filter(|s| !s.is_empty()) {
            let src = std::path::Path::new(src);
            let img_name = src.file_name().unwrap().to_string_lossy().into_owned();
            let a = child(&s.root, &img_name);
            let b = every_name(src);
            println!("{img_name}: MFT {} B / {} files / {} dirs; source {b:?}", a.other, a.files, a.dirs);
            assert_eq!((a.other, a.files, a.dirs), b, "{img_name}");
        }
        let many = child(&s.root, "many");
        assert_eq!(many.children.iter().filter(|c| c.kind == Kind::File).count(), 64);
        assert_eq!(child(&s.root, "sparse.bin").other, 700 * MIB);
        assert_eq!(child(&s.root, "sparse.bin").size, 0, "an unwritten sparse file takes no clusters");
        assert_eq!(child(child(&s.root, "ünï cødé"), "日本語.txt").other, 5000);
        // ntfs-3g writes symlinks as Interix files ("IntxLNK" + target), not reparse points:
        // Windows lists them as small system files too. Junctions are covered by the built volume.
        assert_eq!(child(&s.root, "link_to_root").other, 18);
        // the hard-linked 3 MB file is counted once
        let hl: Vec<&str> = ["many", "deep"]
            .iter()
            .flat_map(|d| child(&s.root, d).children.iter())
            .filter(|c| c.other == 3_000_000)
            .map(|c| &*c.name)
            .collect();
        println!("hard link counted as {hl:?}");
        assert_eq!(hl.len(), 1);
    }

    /// The overlapped engine on a plain file: chunks made of several out-of-order pieces and a
    /// sparse one, few buffers, slow parsers. Every chunk must arrive once with the right bytes.
    #[test]
    fn overlapped_reads_chunks_right() {
        let path = std::env::temp_dir().join(format!("diskscope_iocp_{}.bin", std::process::id()));
        let words: Vec<u8> = (0..(6u32 << 20) / 4).flat_map(|i| i.to_le_bytes()).collect(); // 6 MB
        std::fs::write(&path, &words).unwrap();
        let vol = RawVolume::open_path(path.to_string_lossy().into_owned()).unwrap();
        let (h, port) = vol.overlapped.as_ref().map(|(h, p)| (h.0, p.0)).expect("overlapped handle");
        const K: usize = 64 << 10;
        // chunk c (256 KB): its four 64 KB quarters come from blocks 23-c, c, 40+c and a hole
        let plan: Vec<ChunkPlan> = (0..16usize)
            .map(|c| ChunkPlan {
                pieces: vec![
                    Piece { vol: Some(((23 - c) * K) as u64), at: 0, len: K },
                    Piece { vol: Some((c * K) as u64), at: K, len: K },
                    Piece { vol: Some(((40 + c) * K) as u64), at: 2 * K, len: K },
                    Piece { vol: None, at: 3 * K, len: K },
                ],
                len: 4 * K,
                gaps: false,
            })
            .collect();
        let pool = BufPool::new(3, 4 * K);
        let got: Vec<Mutex<Option<(Vec<u8>, bool)>>> = (0..plan.len()).map(|_| Mutex::new(None)).collect();
        let stats = rayon::scope(|sc| {
            let got = &got;
            vol.read_chunks(&plan, &pool, &mut |ci, mut buf, ok| {
                sc.spawn(move |_| {
                    std::thread::sleep(std::time::Duration::from_millis(3)); // a slow parser
                    let bytes = buf[..4 * K].to_vec();
                    buf[0] = 0xEE; // scribble: the next chunk must not see it
                    drop(buf);
                    assert!(got[ci].lock().unwrap().replace((bytes, ok)).is_none(), "chunk {ci} twice");
                });
                true
            })
        });
        assert_eq!(stats.bytes, 16 * 4 * K as u64);
        assert_eq!(stats.engine, "overlapped");
        for (c, g) in got.iter().enumerate() {
            let (bytes, ok) = g.lock().unwrap().take().unwrap_or_else(|| panic!("chunk {c} missing"));
            assert!(ok);
            assert_eq!(&bytes[..K], &words[(23 - c) * K..(24 - c) * K], "chunk {c} piece 0");
            assert_eq!(&bytes[K..2 * K], &words[c * K..(c + 1) * K], "chunk {c} piece 1");
            assert_eq!(&bytes[2 * K..3 * K], &words[(40 + c) * K..(41 + c) * K], "chunk {c} piece 2");
            assert!(bytes[3 * K..].iter().all(|&b| b == 0), "chunk {c} hole");
        }
        // a read past the end fails that chunk, and only that one
        let bad = vec![
            ChunkPlan { pieces: vec![Piece { vol: Some(0), at: 0, len: K }], len: K, gaps: false },
            ChunkPlan { pieces: vec![Piece { vol: Some(200 * K as u64), at: 0, len: K }], len: K, gaps: false },
        ];
        let mut oks = vec![None; 2];
        vol.read_chunks_iocp(h, port, &bad, &pool, &mut |ci, buf, ok| {
            oks[ci] = Some(ok);
            drop(buf);
            true
        });
        assert_eq!(oks, vec![Some(true), Some(false)]);
        drop(vol);
        let _ = std::fs::remove_file(&path);
    }

    /// The whole MFT scan through the overlapped engine on the real image equals the plain one.
    #[test]
    #[ignore]
    fn real_image_overlapped() {
        let Ok(path) = std::env::var("DISKSCOPE_TEST_IMAGE") else { return };
        let dump = |s: &MftScan| {
            fn go(n: &Node, d: usize, out: &mut String) {
                out.push_str(&format!("{}{} {} {} {} {}\n", " ".repeat(d), n.name, n.size, n.other, n.files, n.dirs));
                n.children.iter().for_each(|c| go(c, d + 1, out));
            }
            let mut out = String::new();
            go(&s.root, 0, &mut out);
            out
        };
        let raw = RawVolume::open_path(path.clone()).unwrap();
        assert!(raw.overlapped.is_some());
        let a = scan_volume(&raw, "T:\\", &ScanProgress::new(), None).unwrap();
        let plain = ImageFile(std::fs::File::open(&path).unwrap());
        let b = scan_volume(&plain, "T:\\", &ScanProgress::new(), None).unwrap();
        assert_eq!(dump(&a), dump(&b));
        let timing = a.notes.iter().find(|n| n.starts_with("MFT timing")).unwrap();
        assert!(timing.contains("overlapped"), "{timing}");
        println!("{timing}");
    }

    #[test]
    fn fixup_detects_torn_records() {
        let mut r = record(30, FILE, 0, &[si(0), fname(5, "x", 3)]);
        let mut torn = r.clone();
        assert!(apply_fixup(&mut r));
        assert_eq!(u32_at(&r, 0x38), AT_STANDARD_INFORMATION);
        torn[510] ^= 1;
        assert!(!apply_fixup(&mut torn));
    }
}
