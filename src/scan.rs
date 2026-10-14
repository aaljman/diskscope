//! The scanner (ARCHITECTURE.md, "Scanning").
//!
//! Two ways to get the tree:
//! - **MFT** (`mft.rs`): with admin rights on an NTFS drive root, read the volume's master file
//!   table directly and build the tree from parent references. A full drive in seconds.
//! - **Walk**: a parallel directory walk on a dedicated rayon pool. Each directory is opened
//!   relative to its parent's handle (`NtOpenFile`) and listed with `NtQueryDirectoryFile` into a
//!   64 KB buffer, so a typical folder costs one open and one or two listing calls. Sizes and
//!   attributes come from the listing; no per-file syscalls.

mod mft;

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use rayon::prelude::*;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
    FileIdFullDirectoryInformation, NtOpenFile, NtQueryDirectoryFile,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, INVALID_HANDLE_VALUE, STATUS_NO_MORE_FILES, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, GetDiskFreeSpaceExW, OPEN_EXISTING, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use crate::model::{Kind, Metric, Node, ScanJob, ScanProgress, ScanResult, format_count};

/// Each directory keeps this many of its largest files as File nodes; the rest become one Rest.
pub const KEEP_FILES: usize = 64;

const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_REPARSE_POINT: u32 = 0x400;
const ATTR_OFFLINE: u32 = 0x1000;
const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
/// Reparse tags with this bit are "name surrogates": symlinks, junctions, mount points.
const TAG_NAME_SURROGATE: u32 = 0x2000_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// MFT for an NTFS drive root when it can be read (admin), else the walk.
    Auto,
    /// MFT, also for a folder (reads the whole volume's table, keeps the subtree).
    /// Falls back to the walk if the MFT cannot be read.
    Mft,
    Walk,
}

pub fn start(root_path: String) -> ScanJob {
    start_with(root_path, Method::Auto)
}

pub fn start_with(root_path: String, method: Method) -> ScanJob {
    let root_path = normalize_root(&root_path);
    let progress = Arc::new(ScanProgress::new());
    let result = Arc::new(Mutex::new(None));
    let (p, r, path) = (progress.clone(), result.clone(), root_path.clone());
    std::thread::Builder::new()
        .name("diskscope-scan".into())
        .spawn(move || {
            // always deliver a result, so the UI never waits forever
            let result = catch_unwind(AssertUnwindSafe(|| run(&path, method, &p).result))
                .unwrap_or_else(|_| failed_result(&path, &p));
            if let Ok(mut g) = r.lock() {
                *g = Some(result);
            }
        })
        .expect("spawn scan thread");
    ScanJob { root_path, progress, result }
}

/// `--mft-bench`: times reading the volume's MFT (several engines) and parsing it, separately.
pub fn mft_bench(root_path: &str) -> Result<String, String> {
    mft::bench(root_path)
}

/// A finished scan plus human-readable notes (method fallbacks, MFT statistics) for the CLI.
pub struct Outcome {
    pub result: ScanResult,
    pub notes: Vec<String>,
}

/// Runs a scan on the calling thread (the work itself happens on the scan pool).
/// `root_path` should already be normalised (`normalize_root`).
pub fn run(root_path: &str, method: Method, progress: &ScanProgress) -> Outcome {
    let started = Instant::now();
    let mut notes = Vec::new();
    let drive_root = is_drive_root(root_path);
    let want_mft = match method {
        Method::Walk => false,
        Method::Mft => true,
        // only local disks: a mapped share or a SUBST drive has no volume of its own to read
        Method::Auto => drive_root && crate::drives::is_local_volume(root_path),
    };

    let mut tree: Option<(Node, &'static str)> = None;
    if want_mft {
        let r = catch_unwind(AssertUnwindSafe(|| {
            pool().install(|| mft::scan(root_path, progress))
        }));
        match r {
            Ok(Ok(scan)) => {
                notes.extend(scan.notes);
                tree = Some((scan.root, "MFT"));
            }
            Ok(Err(e)) => notes.push(format!("MFT not used: {e}; walking folders instead")),
            Err(_) => notes.push("MFT reader crashed; walking folders instead".into()),
        }
        if tree.is_none() {
            reset_progress(progress);
        }
    }
    let (mut root, method_name) = match tree {
        Some(t) => t,
        None => {
            let r = catch_unwind(AssertUnwindSafe(|| pool().install(|| walk(root_path, progress))));
            match r {
                Ok((node, walk_notes)) => {
                    notes.extend(walk_notes);
                    (node, "walk")
                }
                Err(_) => {
                    notes.push("folder walk crashed".into());
                    (denied_dir(root_name(root_path)), "walk")
                }
            }
        }
    };
    if progress.cancelled() {
        notes.push("scan cancelled; partial result".into());
    }
    root.name = root_name(root_path);

    let hotspots = crate::hotspots::compute(&root, root_path, 40);
    let volume = if drive_root { volume_space(root_path) } else { None };
    let result = ScanResult {
        root_path: root_path.to_string(),
        root,
        hotspots,
        elapsed: started.elapsed(),
        denied_dirs: progress.denied.load(Relaxed),
        volume,
        method: method_name,
        metric: Metric::OnDisk,
    };
    Outcome { result, notes }
}

/// The result of a scan that crashed: an unreadable root.
fn failed_result(root_path: &str, progress: &ScanProgress) -> ScanResult {
    ScanResult {
        root_path: root_path.to_string(),
        root: denied_dir(root_name(root_path)),
        hotspots: Vec::new(),
        elapsed: progress.started.elapsed(),
        denied_dirs: 1,
        volume: None,
        method: "walk",
        metric: Metric::OnDisk,
    }
}

fn reset_progress(p: &ScanProgress) {
    p.files.store(0, Relaxed);
    p.dirs.store(0, Relaxed);
    p.bytes.store(0, Relaxed);
    p.denied.store(0, Relaxed);
}

// ---------------------------------------------------------------------------------------------
// paths

/// `c:` -> `C:\`, `d:/games/` -> `D:\games`, relative -> absolute, `\\?\C:\x` -> `C:\x`.
pub fn normalize_root(p: &str) -> String {
    let mut s = p.trim().trim_matches('"').replace('/', "\\");
    if let Some(rest) = s.strip_prefix("\\\\?\\") {
        if let Some(unc) = rest.strip_prefix("UNC\\") {
            s = format!("\\\\{unc}");
        } else {
            s = rest.to_string();
        }
    }
    let b = s.as_bytes();
    if b.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        s.push('\\');
    }
    if !s.starts_with("\\\\") {
        if let Ok(abs) = std::path::absolute(&s) {
            s = abs.to_string_lossy().into_owned();
        }
    }
    if s.len() >= 2 && s.as_bytes()[1] == b':' {
        s[..1].make_ascii_uppercase();
    }
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    s
}

/// `C:\`
pub fn is_drive_root(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\'
}

/// `C:` for a drive root, else the last path component.
pub fn root_name(p: &str) -> Box<str> {
    if is_drive_root(p) {
        return p[..2].into();
    }
    match p.trim_end_matches('\\').rsplit('\\').next() {
        Some(last) if !last.is_empty() => last.into(),
        _ => p.into(),
    }
}

/// Path for the Win32 file APIs: `\\?\C:\x` or `\\?\UNC\server\share\x`, NUL-terminated.
fn win32_long_path(p: &str) -> Vec<u16> {
    let long = if let Some(unc) = p.strip_prefix("\\\\") {
        format!("\\\\?\\UNC\\{unc}")
    } else {
        format!("\\\\?\\{p}")
    };
    long.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// (total, free) bytes of the volume holding `root`.
pub fn volume_space(root: &str) -> Option<(u64, u64)> {
    let w = wide(root);
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some((total, free))
}

// ---------------------------------------------------------------------------------------------
// shared tree assembly (walk and MFT)

/// The scan pool. I/O bound, so more threads than cores keeps the NVMe queue full.
/// `DISKSCOPE_THREADS` overrides the size (for benchmarking).
pub(crate) fn pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let cpus = std::thread::available_parallelism().map_or(8, |n| n.get());
        let n = std::env::var("DISKSCOPE_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or((cpus * 4).clamp(16, 64));
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            // The walk and the MFT tree build recurse once per folder level (plus small rayon
            // join frames). 8 MB overflowed on a 15,000-level-deep test tree; 64 MB leaves ample
            // room. Only touched pages are committed, so this is address space, not memory.
            .stack_size(64 << 20)
            .thread_name(|i| format!("diskscope-scan-{i}"))
            .build()
            .expect("scan thread pool")
    })
}

pub(crate) fn denied_dir(name: Box<str>) -> Node {
    let mut n = empty_dir(name);
    n.denied = true;
    n
}

pub(crate) fn empty_dir(name: Box<str>) -> Node {
    Node {
        name,
        size: 0,
        files: 0,
        dirs: 0,
        kind: Kind::Dir,
        other: 0,
        denied: false,
        children: Vec::new(),
    }
}

/// Turns a directory's files into child nodes: the `KEEP_FILES` largest (on disk) as File nodes,
/// the rest folded into one Rest node. Each file is `(on_disk, logical, T)`; nodes come out with
/// `size` = on disk and `other` = logical. `name_of` is only called for the files that are kept.
pub(crate) fn file_nodes<T>(files: &mut [(u64, u64, T)], name_of: impl Fn(&T) -> Box<str>) -> Vec<Node> {
    let keep = files.len().min(KEEP_FILES);
    if files.len() > KEEP_FILES {
        files.select_nth_unstable_by(KEEP_FILES - 1, |a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    }
    let mut out = Vec::with_capacity(keep + usize::from(files.len() > keep));
    for (disk, logical, t) in &files[..keep] {
        out.push(Node {
            name: name_of(t),
            size: *disk,
            other: *logical,
            files: 1,
            dirs: 0,
            kind: Kind::File,
            denied: false,
            children: Vec::new(),
        });
    }
    let rest = &files[keep..];
    if !rest.is_empty() {
        out.push(Node {
            name: match rest.len() {
                1 => "1 smaller file".into(),
                n => format!("{} smaller files", format_count(n as u64)).into(),
            },
            size: rest.iter().map(|f| f.0).sum(),
            other: rest.iter().map(|f| f.1).sum(),
            files: rest.len() as u64,
            dirs: 0,
            kind: Kind::Rest,
            denied: false,
            children: Vec::new(),
        });
    }
    out
}

/// Child order: size, then the other measure, largest first, then name. With this tie order
/// `ScanResult::set_metric` (a stable sort on the incoming measure) swapped twice gives back the
/// exact same order.
fn child_order(a: &Node, b: &Node) -> std::cmp::Ordering {
    b.size.cmp(&a.size).then(b.other.cmp(&a.other)).then_with(|| a.name.cmp(&b.name))
}

/// A directory node from finished children: sums both measures and the counts.
fn dir_node(name: Box<str>, children: Vec<Node>) -> Node {
    let (mut size, mut other, mut files, mut dirs) = (0u64, 0u64, 0u64, 0u64);
    for c in &children {
        size += c.size;
        other += c.other;
        files += c.files;
        if c.kind == Kind::Dir {
            dirs += 1 + c.dirs;
        }
    }
    Node { name, size, other, files, dirs, kind: Kind::Dir, denied: false, children }
}

/// Builds a directory node from its subdirectory nodes and its file nodes: sorts children
/// (`child_order`), sums the totals, trims the allocation.
pub(crate) fn finish_dir(name: Box<str>, mut children: Vec<Node>, file_nodes: Vec<Node>) -> Node {
    children.extend(file_nodes);
    children.sort_unstable_by(child_order);
    children.shrink_to_fit();
    dir_node(name, children)
}

// ---------------------------------------------------------------------------------------------
// the walk

/// Slot of a listed folder in the hard-link bookkeeping; NO_SLOT for one that was not listed.
const NO_SLOT: u32 = u32::MAX;

/// One sighting of a file, kept so a file with several hard links is counted once. Packed to
/// 28 bytes: a large drive has millions of them.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct Sighting {
    id: u64,
    logical: u64,
    /// `pack_disk` of the on-disk size.
    disk: u32,
    dir: u32,
    name_hash: u32,
}

impl Sighting {
    fn id(&self) -> u64 {
        self.id
    }
    fn logical(&self) -> u64 {
        self.logical
    }
    fn disk(&self) -> u64 {
        let d = self.disk;
        if d & DISK_IN_PAGES != 0 { (d & !DISK_IN_PAGES) as u64 * 4096 } else { d as u64 * 8 }
    }
}

const DISK_IN_PAGES: u32 = 1 << 31;

/// An allocation size in 31 bits: 8-byte units up to 16 GiB (resident data is rounded to 8),
/// 4 KiB units up to 8 TiB. None otherwise (that file is then not de-duplicated).
fn pack_disk(d: u64) -> Option<u32> {
    if d % 8 == 0 && d / 8 < DISK_IN_PAGES as u64 {
        Some((d / 8) as u32)
    } else if d % 4096 == 0 && d / 4096 < DISK_IN_PAGES as u64 {
        Some(DISK_IN_PAGES | (d / 4096) as u32)
    } else {
        None
    }
}

/// Where a listed folder ended up: its parent's slot and its index among the parent's sorted
/// children, plus its own file id (the winner rule's first key).
#[derive(Clone, Copy)]
struct DirPos {
    slot: u32,
    parent: u32,
    index: u32,
    id: [u32; 2],
}

impl DirPos {
    const NONE: DirPos = DirPos { slot: NO_SLOT, parent: NO_SLOT, index: 0, id: [0, 0] };
    fn id(&self) -> u64 {
        self.id[0] as u64 | (self.id[1] as u64) << 32
    }
}

/// Hard-link bookkeeping for one walk: every file sighting and every folder's final position,
/// sharded by pool thread.
struct Links {
    next_slot: std::sync::atomic::AtomicU32,
    sightings: Vec<Mutex<Vec<Sighting>>>,
    dirs: Vec<Mutex<Vec<DirPos>>>,
}

impl Links {
    fn new() -> Self {
        let shards = pool().current_num_threads() + 1;
        Links {
            next_slot: std::sync::atomic::AtomicU32::new(0),
            sightings: (0..shards).map(|_| Mutex::new(Vec::new())).collect(),
            dirs: (0..shards).map(|_| Mutex::new(Vec::new())).collect(),
        }
    }

    fn shard(&self) -> usize {
        rayon::current_thread_index().map_or(0, |i| i + 1) % self.sightings.len()
    }
}

/// FNV-1a over the UTF-16 name: the deterministic tie-break between names in one folder, and
/// how a sighting finds its File node again.
fn name_hash(units: impl IntoIterator<Item = u16>) -> u32 {
    let mut h = 0x811C_9DC5u32;
    for u in units {
        h ^= u as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

struct Walk<'a> {
    progress: &'a ScanProgress,
    visited: AtomicU64,
    /// None on file systems without hard links (FAT, exFAT).
    links: Option<Links>,
}

impl<'a> Walk<'a> {
    /// Counts one listed directory; now and then publishes its path for the progress view.
    fn visited(&self, path: impl FnOnce() -> String) {
        let n = self.visited.fetch_add(1, Relaxed);
        if n % 256 == 0 {
            if let Ok(mut g) = self.progress.current.try_lock() {
                *g = path();
            }
        }
    }

    fn count(&self, subdirs: usize, files: usize, bytes: u64) {
        let p = self.progress;
        p.dirs.fetch_add(subdirs as u64, Relaxed);
        p.files.fetch_add(files as u64, Relaxed);
        p.bytes.fetch_add(bytes, Relaxed);
    }
}

fn is_placeholder(attrs: u32) -> bool {
    attrs & (ATTR_RECALL_ON_DATA_ACCESS | ATTR_OFFLINE) != 0
}

fn is_link(attrs: u32, tag: u32) -> bool {
    attrs & ATTR_REPARSE_POINT != 0 && tag & TAG_NAME_SURROGATE != 0
}

/// Walks `root_path`. Returns the tree (size = on disk, other = logical) and notes.
pub(crate) fn walk(root_path: &str, progress: &ScanProgress) -> (Node, Vec<String>) {
    crate::drives::enable_backup_privilege();
    // DISKSCOPE_NO_LINKS=1: count every name (for measuring the cost of the bookkeeping)
    let wanted = std::env::var_os("DISKSCOPE_NO_LINKS").is_none();
    let links = (wanted && crate::drives::supports_hard_links(root_path)).then(Links::new);
    let ctx = Walk { progress, visited: AtomicU64::new(0), links };
    let path = win32_long_path(root_path);
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_LIST_DIRECTORY | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    let name = root_name(root_path);
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        progress.denied.fetch_add(1, Relaxed);
        return (denied_dir(name), Vec::new());
    }
    let display: Vec<u16> = root_path.trim_end_matches('\\').encode_utf16().collect();
    let chain = Chain { name: &display, parent: None };
    let (mut node, root_slot) = walk_dir(Handle(h), name, &chain, &ctx);
    let mut notes = Vec::new();
    if let Some(links) = ctx.links {
        if root_slot != NO_SLOT {
            if let Ok(mut d) = links.dirs[0].lock() {
                d.push(DirPos { slot: root_slot, parent: NO_SLOT, index: 0, id: [0, 0] });
            }
            let (names, disk, logical) = count_links_once(&mut node, links);
            if names > 0 {
                notes.push(format!(
                    "walk: {} extra hard-link names counted once ({} on disk, {} logical not counted twice)",
                    format_count(names),
                    crate::model::format_size(disk),
                    crate::model::format_size(logical)
                ));
            }
        }
    } else {
        notes.push("walk: this file system has no hard links".into());
    }
    (node, notes)
}

/// Settles hard links after the walk: of all the names of one file (same file id), the one in
/// the folder with the smallest file id, then the smallest name hash, keeps the file's bytes and
/// its file count; every other name becomes a 0-byte entry, debited from its folder and the
/// folders above. Deterministic: independent of thread timing. Returns (names, disk, logical).
fn count_links_once(root: &mut Node, links: Links) -> (u64, u64, u64) {
    let slots = links.next_slot.load(Relaxed) as usize;
    let mut pos = vec![DirPos::NONE; slots];
    for shard in links.dirs {
        for p in shard.into_inner().unwrap_or_default() {
            if let Some(x) = pos.get_mut(p.slot as usize) {
                *x = p;
            }
        }
    }
    let total: usize = links.sightings.iter().map(|s| s.lock().map_or(0, |v| v.len())).sum();
    let mut all: Vec<Sighting> = Vec::with_capacity(total);
    for shard in links.sightings {
        all.extend_from_slice(&shard.into_inner().unwrap_or_default());
    }
    all.par_sort_unstable_by_key(Sighting::id);
    let dir_id = |s: &Sighting| pos.get({ s.dir } as usize).map_or(u64::MAX, |p| p.id());
    // keep only the losing names, compacted to the front in place (no second array at the peak)
    let (mut kept, mut i) = (0usize, 0usize);
    while i < all.len() {
        let mut j = i + 1;
        while j < all.len() && all[j].id() == all[i].id() {
            j += 1;
        }
        if j - i > 1 {
            let winner = (i..j).min_by_key(|&k| (dir_id(&all[k]), { all[k].name_hash })).unwrap_or(i);
            for k in (i..j).filter(|&k| k != winner) {
                all[kept] = all[k]; // kept <= k: never overwrites an unread sighting
                kept += 1;
            }
        }
        i = j;
    }
    all.truncate(kept);
    all.shrink_to_fit();
    let losers = all;

    let path_of = |slot: u32| -> Option<Vec<u32>> {
        let mut path = Vec::new();
        let mut s = slot;
        loop {
            let p = pos.get(s as usize)?;
            if p.slot != s {
                return None; // never placed (a cancelled scan)
            }
            if p.parent == NO_SLOT {
                break;
            }
            path.push(p.index);
            s = p.parent;
            if path.len() > slots {
                return None;
            }
        }
        path.reverse();
        Some(path)
    };
    let (mut names, mut disk, mut logical) = (0u64, 0u64, 0u64);
    // folders whose sizes changed: the debited ones and everything above them
    let mut touched = vec![false; slots];
    let mut folders: Vec<(u32, u32)> = Vec::new(); // (depth, slot)
    for s in &losers {
        let Some(path) = path_of(s.dir) else { continue };
        if !debit(root, &path, s) {
            continue;
        }
        names += 1;
        disk += s.disk();
        logical += s.logical();
        let (mut slot, mut depth) = ({ s.dir }, path.len() as u32);
        while slot != NO_SLOT && !touched[slot as usize] {
            touched[slot as usize] = true;
            folders.push((depth, slot));
            slot = pos[slot as usize].parent;
            depth = depth.saturating_sub(1);
        }
    }
    drop(touched);
    // re-sort them deepest first, so the paths to the ones still waiting stay valid
    folders.sort_unstable_by(|a, b| b.cmp(a));
    for (_, slot) in folders {
        if let Some(n) = path_of(slot).and_then(|p| node_at_mut(root, &p)) {
            n.children.sort_unstable_by(child_order);
        }
    }
    (names, disk, logical)
}

fn node_at_mut<'n>(root: &'n mut Node, path: &[u32]) -> Option<&'n mut Node> {
    let mut n = root;
    for &i in path {
        n = n.children.get_mut(i as usize)?;
    }
    Some(n)
}

/// Takes one losing name's bytes and file count out of its folder (its File node, or the Rest
/// node it was folded into) and out of every folder above. False if it cannot be found.
fn debit(root: &mut Node, path: &[u32], s: &Sighting) -> bool {
    let is_entry = |c: &Node| {
        c.kind == Kind::File
            && c.files == 1
            && c.size == s.disk()
            && c.other == s.logical()
            && name_hash(c.name.encode_utf16()) == { s.name_hash }
    };
    // find the target first, so nothing is debited when it is missing
    let target = {
        let Some(dir) = node_at_mut(root, path) else { return false };
        match dir.children.iter().position(is_entry) {
            Some(i) => Some(i),
            None => match dir.children.iter().position(|c| c.kind == Kind::Rest) {
                Some(i) if dir.children[i].files > 0 && dir.children[i].size >= s.disk() && dir.children[i].other >= s.logical() => None,
                _ => return false,
            },
        }
    };
    let sub = |n: &mut Node| {
        n.size = n.size.saturating_sub(s.disk());
        n.other = n.other.saturating_sub(s.logical());
        n.files = n.files.saturating_sub(1);
    };
    let mut n = root;
    sub(n);
    for &i in path {
        n = &mut n.children[i as usize];
        sub(n);
    }
    match target {
        Some(i) => sub(&mut n.children[i]),
        None => {
            if let Some(r) = n.children.iter_mut().find(|c| c.kind == Kind::Rest) {
                sub(r);
            }
        }
    }
    true
}

/// An owned directory handle, closed on drop.
struct Handle(HANDLE);
// The handle is only used for relative opens and listing, both thread-safe.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Handle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// The path of a directory as a linked list up the recursion, rendered only for display.
struct Chain<'a> {
    name: &'a [u16],
    parent: Option<&'a Chain<'a>>,
}

impl Chain<'_> {
    fn render(&self) -> String {
        let mut parts = Vec::new();
        let mut c = Some(self);
        while let Some(x) = c {
            parts.push(x.name);
            c = x.parent;
        }
        let mut s = String::new();
        for (i, p) in parts.iter().rev().enumerate() {
            if i > 0 {
                s.push('\\');
            }
            s.push_str(&String::from_utf16_lossy(p));
        }
        s
    }
}

thread_local! {
    /// 64 KB listing buffer per pool thread (u64 for 8-byte alignment of the entries).
    static LIST_BUF: RefCell<Vec<u64>> = RefCell::new(vec![0u64; 64 * 1024 / 8]);
}

const STATUS_NO_SUCH_FILE: i32 = 0xC000_000F_u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034_u32 as i32;
const STATUS_OBJECT_PATH_NOT_FOUND: i32 = 0xC000_003A_u32 as i32;
const STATUS_DELETE_PENDING: i32 = 0xC000_0056_u32 as i32;
const STATUS_NOT_A_DIRECTORY: i32 = 0xC000_0103_u32 as i32;

/// The directory went away (or turned into a file) between listing and opening.
fn vanished(status: i32) -> bool {
    matches!(
        status,
        STATUS_NO_SUCH_FILE
            | STATUS_OBJECT_NAME_NOT_FOUND
            | STATUS_OBJECT_PATH_NOT_FOUND
            | STATUS_DELETE_PENDING
            | STATUS_NOT_A_DIRECTORY
    )
}

/// One entry of a directory listing.
struct Listed<'a> {
    name: &'a [u16],
    attrs: u32,
    /// The reparse tag, when `attrs` has FILE_ATTRIBUTE_REPARSE_POINT.
    tag: u32,
    size: u64,
    allocated: u64,
    /// File id (NTFS: record number and sequence); 0 or !0 when the file system has none.
    id: u64,
}

/// Lists a directory handle: calls `f` for every entry except `.` and `..`. Err(status) if the
/// listing failed before any entry was returned; Ok(false) if it failed part way (the entries
/// so far were delivered).
fn list_dir(h: HANDLE, mut f: impl FnMut(&Listed)) -> Result<bool, i32> {
    LIST_BUF.with_borrow_mut(|buf| {
        let bytes = buf.len() * 8;
        let base = buf.as_mut_ptr() as *mut u8;
        let mut first = true;
        loop {
            let mut iosb = IO_STATUS_BLOCK::default();
            let st = unsafe {
                NtQueryDirectoryFile(
                    h,
                    std::ptr::null_mut(),
                    None,
                    std::ptr::null(),
                    &mut iosb,
                    base.cast(),
                    bytes as u32,
                    FileIdFullDirectoryInformation,
                    false,
                    std::ptr::null(),
                    false,
                )
            };
            if st == STATUS_NO_MORE_FILES || (st == STATUS_NO_SUCH_FILE && first) {
                return Ok(true);
            }
            if st != 0 {
                return if first { Err(st) } else { Ok(false) };
            }
            first = false;
            // FILE_ID_FULL_DIR_INFORMATION: NextEntryOffset u32 @0, EndOfFile i64 @40,
            // AllocationSize i64 @48, FileAttributes u32 @56, FileNameLength u32 @60 (bytes),
            // EaSize u32 @64 (the reparse tag when the entry is a reparse point), FileId i64 @72,
            // FileName @80.
            let mut off = 0usize;
            loop {
                if off + 80 > bytes {
                    break;
                }
                unsafe {
                    let e = base.add(off);
                    let next = (e as *const u32).read_unaligned() as usize;
                    let name_bytes = (e.add(60) as *const u32).read_unaligned() as usize;
                    if off + 80 + name_bytes > bytes {
                        break;
                    }
                    // entries are 8-byte aligned, so the name (at +80) is too
                    let name = std::slice::from_raw_parts(e.add(80) as *const u16, name_bytes / 2);
                    if name != [46] && name != [46, 46] {
                        f(&Listed {
                            name,
                            attrs: (e.add(56) as *const u32).read_unaligned(),
                            tag: (e.add(64) as *const u32).read_unaligned(),
                            size: (e.add(40) as *const u64).read_unaligned(),
                            allocated: (e.add(48) as *const u64).read_unaligned(),
                            id: (e.add(72) as *const u64).read_unaligned(),
                        });
                    }
                    if next == 0 {
                        break;
                    }
                    off += next;
                }
            }
        }
    })
}

fn open_relative(parent: HANDLE, name: &[u16]) -> Result<Handle, i32> {
    let us = UNICODE_STRING {
        Length: (name.len() * 2) as u16,
        MaximumLength: (name.len() * 2) as u16,
        Buffer: name.as_ptr() as *mut u16,
    };
    let oa = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent,
        ObjectName: &us,
        Attributes: 0, // exact name from the listing; also right in case-sensitive folders
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut h: HANDLE = std::ptr::null_mut();
    let mut iosb = IO_STATUS_BLOCK::default();
    let st = unsafe {
        NtOpenFile(
            &mut h,
            FILE_LIST_DIRECTORY | SYNCHRONIZE,
            &oa,
            &mut iosb,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            // FILE_OPEN_REPARSE_POINT: a non-link reparse folder (a cloud placeholder folder) is
            // listed as it is on disk, without asking its provider to populate it
            FILE_DIRECTORY_FILE
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_OPEN_FOR_BACKUP_INTENT
                | FILE_OPEN_REPARSE_POINT,
        )
    };
    if st >= 0 { Ok(Handle(h)) } else { Err(st) }
}

/// Walks one listed directory. Returns its node and its slot (NO_SLOT if it was not listed).
fn walk_dir(h: Handle, name: Box<str>, chain: &Chain, ctx: &Walk) -> (Node, u32) {
    if ctx.progress.cancelled() {
        return (empty_dir(name), NO_SLOT);
    }
    // names of this directory's entries, back to back; entries refer to (offset, len)
    let mut arena: Vec<u16> = Vec::new();
    let mut files: Vec<(u64, u64, (u32, u16))> = Vec::new();
    let mut subdirs: Vec<(u32, u16, u64)> = Vec::new();
    let mut sightings: Vec<Sighting> = Vec::new();
    let mut bytes = 0u64;
    let listed = list_dir(h.raw(), |e| {
        if is_link(e.attrs, e.tag) {
            return; // symlinks, junctions, mount points: never followed, never counted
        }
        let r = (arena.len() as u32, e.name.len() as u16);
        arena.extend_from_slice(e.name);
        if e.attrs & ATTR_DIRECTORY != 0 {
            subdirs.push((r.0, r.1, e.id));
        } else {
            // cloud placeholders (online-only) count 0 in both measures
            let (disk, logical) = if is_placeholder(e.attrs) { (0, 0) } else { (e.allocated, e.size) };
            bytes += disk;
            files.push((disk, logical, r));
            if ctx.links.is_some() && e.id != 0 && e.id != u64::MAX {
                if let Some(packed) = pack_disk(disk) {
                    let hash = name_hash(e.name.iter().copied());
                    sightings.push(Sighting { id: e.id, logical, disk: packed, dir: NO_SLOT, name_hash: hash });
                }
            }
        }
    });
    let complete = match listed {
        Ok(c) => c,
        Err(st) if vanished(st) => return (empty_dir(name), NO_SLOT),
        Err(_) => {
            ctx.progress.denied.fetch_add(1, Relaxed);
            return (denied_dir(name), NO_SLOT);
        }
    };
    let slot = match &ctx.links {
        Some(links) => {
            let slot = links.next_slot.fetch_add(1, Relaxed);
            if !sightings.is_empty() {
                for s in &mut sightings {
                    s.dir = slot;
                }
                if let Ok(mut v) = links.sightings[links.shard()].lock() {
                    v.extend_from_slice(&sightings);
                }
            }
            slot
        }
        None => NO_SLOT,
    };
    drop(sightings);
    ctx.count(subdirs.len(), files.len(), bytes);
    ctx.visited(|| chain.render());

    let text = |off: u32, len: u16| -> Box<str> {
        String::from_utf16_lossy(&arena[off as usize..off as usize + len as usize]).into()
    };
    let fnodes = file_nodes(&mut files, |r| text(r.0, r.1));
    drop(files);

    let walk_one = |&(off, len, id): &(u32, u16, u64)| -> (Node, u32, u64) {
        let nm = &arena[off as usize..off as usize + len as usize];
        let child_name = text(off, len);
        match open_relative(h.raw(), nm) {
            Ok(ch) => {
                let c = Chain { name: nm, parent: Some(chain) };
                let (n, s) = walk_dir(ch, child_name, &c, ctx);
                (n, s, id)
            }
            Err(st) if vanished(st) => (empty_dir(child_name), NO_SLOT, id),
            Err(_) => {
                ctx.progress.denied.fetch_add(1, Relaxed);
                (denied_dir(child_name), NO_SLOT, id)
            }
        }
    };
    let dnodes: Vec<(Node, u32, u64)> = if subdirs.len() <= 1 {
        subdirs.iter().map(walk_one).collect()
    } else {
        subdirs.par_iter().map(walk_one).collect()
    };
    drop(h);

    // sort the children, remembering where each listed subfolder ended up
    let mut tagged: Vec<(Node, u32, u64)> = dnodes;
    tagged.extend(fnodes.into_iter().map(|n| (n, NO_SLOT, 0)));
    tagged.sort_unstable_by(|a, b| child_order(&a.0, &b.0));
    let mut children = Vec::with_capacity(tagged.len());
    let mut placed = Vec::new();
    for (i, (n, s, id)) in tagged.into_iter().enumerate() {
        if s != NO_SLOT {
            placed.push(DirPos { slot: s, parent: slot, index: i as u32, id: [id as u32, (id >> 32) as u32] });
        }
        children.push(n);
    }
    if let (Some(links), false) = (&ctx.links, placed.is_empty()) {
        if let Ok(mut v) = links.dirs[links.shard()].lock() {
            v.extend_from_slice(&placed);
        }
    }
    let mut node = dir_node(name, children);
    if !complete {
        // the listing broke off part way: keep what was seen, flag the folder as incomplete
        node.denied = true;
        ctx.progress.denied.fetch_add(1, Relaxed);
    }
    (node, slot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Metric;

    #[test]
    fn normalizes_roots() {
        assert_eq!(normalize_root("c:"), "C:\\");
        assert_eq!(normalize_root("d:/games/"), "D:\\games");
        assert_eq!(normalize_root("\\\\?\\C:\\x\\"), "C:\\x");
        assert_eq!(normalize_root("\\\\srv\\share\\"), "\\\\srv\\share");
        assert!(is_drive_root("C:\\"));
        assert!(!is_drive_root("C:\\x"));
        assert_eq!(&*root_name("C:\\"), "C:");
        assert_eq!(&*root_name("D:\\apps\\diskscope"), "diskscope");
    }

    #[test]
    fn packs_disk_sizes() {
        assert_eq!(size_of::<Sighting>(), 28);
        for d in [0u64, 8, 104, 4096, 16 << 30, 40 << 30, (8 << 40) - 4096] {
            let s = Sighting { id: 1, logical: 2, disk: pack_disk(d).unwrap(), dir: 0, name_hash: 0 };
            assert_eq!(s.disk(), d);
        }
        assert_eq!(pack_disk(7), None);
        assert_eq!(pack_disk(8 << 40), None);
    }

    #[test]
    fn keeps_64_largest_files_and_sums_both_sizes() {
        // on disk i*4096 (by which the 64 are chosen), logical i*10
        let mut files: Vec<(u64, u64, u32)> = (0..200u32).map(|i| (i as u64 * 4096, i as u64 * 10, i)).collect();
        let nodes = file_nodes(&mut files, |i| i.to_string().into());
        assert_eq!(nodes.len(), 65);
        let rest = nodes.iter().find(|n| n.kind == Kind::Rest).unwrap();
        assert_eq!(rest.files, 136);
        assert_eq!(&*rest.name, "136 smaller files");
        assert_eq!(rest.size, (0..136u64).map(|i| i * 4096).sum::<u64>());
        assert_eq!(rest.other, (0..136u64).map(|i| i * 10).sum::<u64>());
        let kept_min = nodes.iter().filter(|n| n.kind == Kind::File).map(|n| n.size).min();
        assert_eq!(kept_min, Some(136 * 4096));
        let d = finish_dir("d".into(), vec![], nodes);
        assert_eq!(d.size, (0..200u64).map(|i| i * 4096).sum::<u64>());
        assert_eq!(d.other, (0..200u64).map(|i| i * 10).sum::<u64>());
        assert_eq!(d.files, 200);
        assert!(d.children.windows(2).all(|w| w[0].size >= w[1].size));
    }

    /// The walk agrees with std::fs on this crate's own sources (logical bytes, file count).
    #[test]
    fn walk_matches_std_fs() {
        fn std_totals(p: &std::path::Path) -> (u64, u64, u64) {
            let (mut bytes, mut files, mut dirs) = (0, 0, 0);
            for e in std::fs::read_dir(p).unwrap().flatten() {
                let m = e.metadata().unwrap();
                if m.is_dir() {
                    let (b, f, d) = std_totals(&e.path());
                    bytes += b;
                    files += f;
                    dirs += d + 1;
                } else {
                    bytes += m.len();
                    files += 1;
                }
            }
            (bytes, files, dirs)
        }
        let root = normalize_root(env!("CARGO_MANIFEST_DIR"));
        let src = format!("{root}\\src");
        let p = ScanProgress::new();
        let (a, _) = pool().install(|| walk(&src, &p));
        assert_eq!((a.other, a.files, a.dirs), std_totals(std::path::Path::new(&src)));
        assert!(a.size >= a.other - a.other % 4096, "on disk: whole clusters");
        assert!(a.children.iter().any(|c| &*c.name == "scan.rs"));
    }

    fn dump(n: &Node, depth: usize, out: &mut String) {
        use std::fmt::Write;
        let _ = writeln!(out, "{}{} {:?} {} {} {} {} {}", " ".repeat(depth), n.name, n.kind, n.size, n.other, n.files, n.dirs, n.denied);
        for c in &n.children {
            dump(c, depth + 1, out);
        }
    }

    /// A real fixture with hard links: one 300,000-byte file with four names, two of them in a
    /// folder of 70 bigger files (so one name sits in the Rest node). It must count once, at the
    /// same name on every run, and the files count must be of unique files.
    #[test]
    fn walk_counts_hard_links_once() {
        let base = std::env::temp_dir().join(format!("diskscope_hl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for d in ["a", "b", "c\\deep"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::fs::write(base.join("a\\orig.bin"), vec![7u8; 300_000]).unwrap();
        for i in 0..70 {
            std::fs::write(base.join(format!("b\\big{i:02}.bin")), vec![1u8; 400_000]).unwrap();
        }
        std::fs::hard_link(base.join("a\\orig.bin"), base.join("a\\same_dir.bin")).unwrap();
        std::fs::hard_link(base.join("a\\orig.bin"), base.join("b\\in_rest.bin")).unwrap();
        std::fs::hard_link(base.join("a\\orig.bin"), base.join("c\\deep\\far.bin")).unwrap();
        let root = normalize_root(&base.to_string_lossy());
        let unique_logical = 300_000 + 70 * 400_000;
        let mut first: Option<String> = None;
        for _ in 0..3 {
            let p = ScanProgress::new();
            let (n, notes) = pool().install(|| walk(&root, &p));
            assert_eq!(n.other, unique_logical, "{notes:?}");
            assert_eq!(n.files, 71, "unique files");
            assert!(notes.iter().any(|x| x.contains("3 extra hard-link names")), "{notes:?}");
            // children still sorted and sums still exact after the debits
            fn check(n: &Node) {
                if n.kind == Kind::Dir {
                    assert_eq!(n.size, n.children.iter().map(|c| c.size).sum::<u64>(), "{}", n.name);
                    assert_eq!(n.other, n.children.iter().map(|c| c.other).sum::<u64>(), "{}", n.name);
                    assert_eq!(n.files, n.children.iter().map(|c| c.files).sum::<u64>(), "{}", n.name);
                    assert!(n.children.windows(2).all(|w| child_order(&w[0], &w[1]).is_le()), "{}", n.name);
                    n.children.iter().for_each(check);
                }
            }
            check(&n);
            let b = n.children.iter().find(|c| &*c.name == "b").unwrap();
            assert!(b.children.iter().any(|c| c.kind == Kind::Rest), "in_rest.bin is folded");
            let mut s = String::new();
            dump(&n, 0, &mut s);
            match &first {
                None => first = Some(s),
                Some(f) => assert_eq!(f, &s, "same winner, same tree on every run"),
            }
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `set_metric` swapped twice gives back the identical tree (the tie order of `child_order`).
    #[test]
    fn set_metric_round_trip() {
        let src = format!("{}\\src", normalize_root(env!("CARGO_MANIFEST_DIR")));
        let out = run(&src, Method::Walk, &ScanProgress::new());
        let mut r = out.result;
        assert_eq!(r.metric, Metric::OnDisk);
        let (disk, logical) = (r.root.size, r.root.other);
        let mut before = String::new();
        dump(&r.root, 0, &mut before);
        r.set_metric(Metric::Logical);
        assert_eq!((r.root.size, r.root.other), (logical, disk));
        assert!(r.root.children.windows(2).all(|w| w[0].size >= w[1].size));
        r.set_metric(Metric::OnDisk);
        let mut after = String::new();
        dump(&r.root, 0, &mut after);
        assert_eq!(before, after);
    }
}
