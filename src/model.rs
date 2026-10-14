//! Shared data model. This file is the contract between the scanner (scan.rs, hotspots.rs,
//! drives.rs) and the UI (app.rs, treemap.rs, theme.rs).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    /// Aggregate of the small files in a directory that were not kept individually.
    Rest,
}

/// Which size `Node::size` holds. Everything (sorting, treemap, hotspots) reads `size`; the
/// other measure waits in `Node::other` and `ScanResult::set_metric` swaps them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    /// Allocated bytes: what deleting it would free. Compressed / sparse files count what they
    /// really occupy, cloud placeholders 0. The default.
    OnDisk,
    /// File length, as in Explorer's "Size" column.
    Logical,
}

#[derive(Debug)]
pub struct Node {
    pub name: Box<str>,
    /// Bytes in the active metric (`ScanResult::metric`). Cloud placeholders count as 0.
    /// A file with several hard links counts once, at one of its names.
    pub size: u64,
    /// Bytes in the other metric.
    pub other: u64,
    /// Files beneath this node (a File counts itself; a Rest counts what it aggregates).
    pub files: u64,
    /// Directories beneath this node, not counting itself.
    pub dirs: u64,
    pub kind: Kind,
    /// The directory could not be listed (access denied etc).
    pub denied: bool,
    /// Sorted by size, largest first. Indices into this vec are stable after the scan,
    /// so navigation is a `Vec<usize>` path from the root.
    pub children: Vec<Node>,
}

/// Tears the tree down with a loop: folders can nest ~16,000 deep (the 32K-char path limit), and
/// the default recursive drop would overflow the stack of whichever thread frees the scan.
impl Drop for Node {
    fn drop(&mut self) {
        let mut stack = std::mem::take(&mut self.children);
        while let Some(mut n) = stack.pop() {
            stack.append(&mut n.children);
        }
    }
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    pub fn get(&self, path: &[usize]) -> Option<&Node> {
        let mut n = self;
        for &i in path {
            n = n.children.get(i)?;
        }
        Some(n)
    }
}

/// A child holding at least this share of its parent is "dominant": the parent is only big
/// because of it, so the UI points past the parent to where the space really is.
pub const FOCUS_SHARE: f64 = 0.5;

/// Follows the chain of dominant children down from `node` (kapa -> rapa -> kona).
/// Returns the child indices of the chain, relative to `node`; empty if no child dominates.
/// Stops at a File (included), never steps into a Rest aggregate.
pub fn focus_chain(node: &Node) -> Vec<usize> {
    let mut chain = Vec::new();
    let mut n = node;
    while n.is_dir() && n.size > 0 {
        // largest real child (a Rest aggregate may sort above it but is never a target)
        let Some((i, c)) = n
            .children
            .iter()
            .enumerate()
            .find(|(_, c)| c.kind != Kind::Rest)
        else {
            break;
        };
        if (c.size as f64) < FOCUS_SHARE * n.size as f64 {
            break;
        }
        chain.push(i);
        n = c;
    }
    chain
}

/// One entry of the smart "where is my space" list.
#[derive(Clone, Debug)]
pub struct Hotspot {
    /// Index path from the scan root.
    pub path: Vec<usize>,
    /// Full display path, e.g. `C:\Users\Sam\.cache\gradle`.
    pub display: String,
    /// Size attributed to this entry. For a residual entry it is the folder's size minus the
    /// hotspots listed inside it.
    pub size: u64,
    /// True when this entry is "the rest of this folder" after its big children were listed
    /// separately.
    pub residual: bool,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Other,
}

#[derive(Clone, Debug)]
pub struct DriveInfo {
    /// `C:\`
    pub root: String,
    pub letter: char,
    /// Volume label, may be empty.
    pub label: String,
    pub total: u64,
    pub free: u64,
    pub kind: DriveKind,
}

impl DriveInfo {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }
}

pub struct ScanResult {
    /// What was scanned, e.g. `C:\` or `D:\Games`.
    pub root_path: String,
    /// Root node; its name is the display name (`C:` or `Games`).
    pub root: Node,
    pub hotspots: Vec<Hotspot>,
    pub elapsed: Duration,
    pub denied_dirs: u64,
    /// Volume totals when the scan root is a drive root, else None.
    pub volume: Option<(u64 /* total */, u64 /* free */)>,
    /// How the scan was done, for display: `"MFT"` (read the NTFS master file table directly,
    /// needs admin) or `"walk"` (parallel directory walk). E.g. "MFT · 3.2 s".
    pub method: &'static str,
    /// Which size every `Node::size` holds right now.
    pub metric: Metric,
}

impl ScanResult {
    /// Space the volume reports as used that the scan could not see (system files, folders
    /// without access, NTFS metadata). Always measured on disk. None when not a drive root.
    pub fn unaccounted(&self) -> Option<u64> {
        let on_disk = match self.metric {
            Metric::OnDisk => self.root.size,
            Metric::Logical => self.root.other,
        };
        self.volume.map(|(total, free)| total.saturating_sub(free).saturating_sub(on_disk))
    }

    /// Switches every node to `metric`: swaps `size` and `other`, re-sorts children largest
    /// first and recomputes the hotspots. Index paths into the tree change, so callers must
    /// re-resolve any stored path (by names). No-op if already active.
    pub fn set_metric(&mut self, metric: Metric) {
        if self.metric == metric {
            return;
        }
        let mut stack: Vec<&mut Node> = vec![&mut self.root];
        while let Some(n) = stack.pop() {
            std::mem::swap(&mut n.size, &mut n.other);
            // sort by the children's coming size (each swaps itself when popped); stable
            n.children.sort_by(|a, b| b.other.cmp(&a.other));
            stack.extend(n.children.iter_mut());
        }
        self.metric = metric;
        self.hotspots = crate::hotspots::compute(&self.root, &self.root_path, 40);
    }
}

/// Live counters, written by the scanner threads and read by the UI each frame.
pub struct ScanProgress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub denied: AtomicU64,
    /// A recently visited directory, updated now and then for display.
    pub current: Mutex<String>,
    pub cancel: AtomicBool,
    pub started: Instant,
}

impl ScanProgress {
    pub fn new() -> Self {
        Self {
            files: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            denied: AtomicU64::new(0),
            current: Mutex::new(String::new()),
            cancel: AtomicBool::new(false),
            started: Instant::now(),
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Handle to a background scan. `scan::start` creates it.
pub struct ScanJob {
    pub root_path: String,
    pub progress: Arc<ScanProgress>,
    pub result: Arc<Mutex<Option<ScanResult>>>,
}

impl ScanJob {
    /// Takes the result once the scan has finished (also after a cancel, with partial data).
    pub fn take_result(&self) -> Option<ScanResult> {
        self.result.lock().ok()?.take()
    }
}

/// `12.4 GB`, `812 MB`, `4.0 KB`, `0 B`. Base 1024 with Explorer-style unit names, so the numbers
/// match what Windows shows.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

/// `1,234,567`
pub fn format_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Full path of the node at `path`, built from the scan's root path.
pub fn path_string(result: &ScanResult, path: &[usize]) -> String {
    let mut s = result.root_path.clone();
    let mut n = &result.root;
    for &i in path {
        let Some(c) = n.children.get(i) else { break };
        if !s.ends_with('\\') {
            s.push('\\');
        }
        s.push_str(&c.name);
        n = c;
    }
    s
}
