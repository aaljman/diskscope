//! `diskscope --cli <path> [--top N] [--depth N] [--out file.txt] [--method auto|mft|walk]
//!  [--metric disk|size]`
//!
//! Scans, then prints the summary, the largest children of the root (each with its focus chain)
//! and the hotspot list. The exe is a windows-subsystem binary, so with no redirected stdout it
//! attaches to the parent's console; `--out` writes the report to a file instead (UTF-8).
//! Never opens a window.

use std::io::Write;

use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE};

use crate::model::{Kind, Metric, Node, ScanProgress, focus_chain, format_count, format_size};
use crate::scan::{self, Method, Outcome};

const USAGE: &str = "usage: diskscope --cli <path> [--top N] [--depth N] [--out file.txt] [--method auto|mft|walk] \
                     [--metric disk|size] [--mft-bench]\n  --metric disk (default): size on disk; size: file length\n  \
                     --mft-bench: time reading and parsing the drive's MFT (admin), no scan\n";

/// Makes `print!` reach the console that started us, if stdout isn't already redirected.
fn attach_console() {
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() || h as isize == -1 {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

fn emit(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

pub fn run(args: &[String]) {
    let mut path: Option<String> = None;
    let mut top = 20usize;
    let mut depth = 1usize;
    let mut out_file: Option<String> = None;
    let mut method = Method::Auto;
    let mut metric = Metric::OnDisk;
    let mut bench = false;
    let mut it = args.iter();
    let mut bad: Option<String> = None;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--top" => top = it.next().and_then(|v| v.parse().ok()).unwrap_or(top),
            "--depth" => depth = it.next().and_then(|v| v.parse().ok()).unwrap_or(depth).max(1),
            "--out" => out_file = it.next().cloned(),
            "--method" => {
                method = match it.next().map(String::as_str) {
                    Some("mft") => Method::Mft,
                    Some("walk") => Method::Walk,
                    Some("auto") => Method::Auto,
                    other => {
                        bad = Some(format!("unknown method {}", other.unwrap_or("(missing)")));
                        method
                    }
                }
            }
            "--metric" => {
                metric = match it.next().map(String::as_str) {
                    Some("disk") => Metric::OnDisk,
                    Some("size") => Metric::Logical,
                    other => {
                        bad = Some(format!("unknown metric {}", other.unwrap_or("(missing)")));
                        metric
                    }
                }
            }
            "--mft-bench" => bench = true,
            "-h" | "--help" => {
                attach_console();
                emit(USAGE);
                return;
            }
            s if s.starts_with("--") => bad = Some(format!("unknown option {s}")),
            s => path = Some(s.to_string()),
        }
    }
    if out_file.is_none() {
        attach_console();
    }
    let (Some(path), None) = (path, bad.clone()) else {
        let msg = format!("{}{USAGE}", bad.map(|b| format!("{b}\n")).unwrap_or_default());
        match &out_file {
            Some(f) => {
                let _ = std::fs::write(f, msg);
            }
            None => emit(&msg),
        }
        return;
    };

    let root_path = scan::normalize_root(&path);
    if !std::path::Path::new(&root_path).is_dir() {
        let msg = format!("not a folder: {root_path}\n");
        match &out_file {
            Some(f) => {
                let _ = std::fs::write(f, msg);
            }
            None => emit(&msg),
        }
        std::process::exit(2);
    }
    if bench {
        let text = match scan::mft_bench(&root_path) {
            Ok(t) => t,
            Err(e) => format!("MFT bench failed: {e}\n"),
        };
        match &out_file {
            Some(f) => {
                let _ = std::fs::write(f, text);
            }
            None => emit(&text),
        }
        return;
    }
    let progress = ScanProgress::new();
    let mut outcome = scan::run(&root_path, method, &progress);
    outcome.result.set_metric(metric);
    let text = report(&outcome, top, depth);
    match &out_file {
        Some(f) => {
            if let Err(e) = std::fs::write(f, &text) {
                attach_console();
                emit(&format!("cannot write {f}: {e}\n"));
            }
        }
        None => emit(&text),
    }
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 { 0.0 } else { part as f64 * 100.0 / whole as f64 }
}

/// `abc…xyz` to at most `max` chars.
fn ellipsize(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let head = (max - 1) / 2;
    let tail = max - 1 - head;
    let mut out: String = s.chars().take(head).collect();
    out.push('…');
    out.extend(s.chars().skip(n - tail));
    out
}

fn label(n: &Node) -> String {
    match n.kind {
        Kind::Dir if n.denied => format!("{}\\ (no access)", n.name),
        Kind::Dir => format!("{}\\", n.name),
        Kind::File => n.name.to_string(),
        Kind::Rest => format!("({})", n.name),
    }
}

/// `→ rapa › kona · 14 GB`, empty if no child dominates.
fn chain_text(n: &Node) -> String {
    let chain = focus_chain(n);
    if chain.is_empty() {
        return String::new();
    }
    let mut names = Vec::new();
    let mut cur = n;
    for &i in &chain {
        cur = &cur.children[i];
        names.push(cur.name.to_string());
    }
    format!("→ {} · {}", names.join(" › "), format_size(cur.size))
}

/// Depth of the deepest folder (iterative, the tree can be very deep) and its path.
fn deepest(root: &Node) -> (usize, String) {
    let mut best: (usize, Vec<&str>) = (0, Vec::new());
    let mut stack: Vec<(&Node, usize)> = vec![(root, 0)];
    let mut names: Vec<&str> = Vec::new();
    while let Some((n, d)) = stack.pop() {
        if d > 0 {
            names.truncate(d - 1);
            names.push(&n.name);
        }
        if d > best.0 {
            best = (d, names.clone());
        }
        for c in n.children.iter().filter(|c| c.is_dir()) {
            stack.push((c, d + 1));
        }
    }
    (best.0, best.1.join("\\"))
}

fn report(o: &Outcome, top: usize, depth: usize) -> String {
    use std::fmt::Write;
    let r = &o.result;
    let root = &r.root;
    let mut s = String::new();
    let _ = writeln!(s, "diskscope · {}", r.root_path);
    let _ = writeln!(s, "  method       {} · {:.2} s", r.method, r.elapsed.as_secs_f64());
    let (on_disk, logical) = match r.metric {
        Metric::OnDisk => (root.size, root.other),
        Metric::Logical => (root.other, root.size),
    };
    let _ = writeln!(
        s,
        "  size         {} on disk · {} logical, in {} files, {} folders",
        format_size(on_disk),
        format_size(logical),
        format_count(root.files),
        format_count(root.dirs)
    );
    let _ = writeln!(
        s,
        "  listing by   {}",
        match r.metric {
            Metric::OnDisk => "size on disk (--metric size: file length)",
            Metric::Logical => "file length (--metric disk: size on disk)",
        }
    );
    let _ = writeln!(s, "  denied       {} folders", format_count(r.denied_dirs));
    if let Some((total, free)) = r.volume {
        let used = total.saturating_sub(free);
        let _ = writeln!(
            s,
            "  volume       {} total, {} free, {} used",
            format_size(total),
            format_size(free),
            format_size(used)
        );
        let un = r.unaccounted().unwrap_or(0);
        let _ = writeln!(s, "  unaccounted  {} ({:.1}% of used, measured on disk)", format_size(un), pct(un, used));
        if on_disk > used {
            let _ = writeln!(
                s,
                "               the scan's on-disk total is {} more than the volume uses (files changing \
                 during the scan?)",
                format_size(on_disk - used)
            );
        }
    }
    for n in &o.notes {
        let _ = writeln!(s, "  note         {n}");
    }
    let (levels, deepest) = deepest(root);
    let _ = writeln!(s, "  deepest      {levels} levels: {}", ellipsize(&deepest, 400));

    let _ = writeln!(s, "\nLargest in {} (top {top}):", r.root_path);
    fn rows(s: &mut String, n: &Node, whole: u64, top: usize, depth: usize, level: usize) {
        for (i, c) in n.children.iter().take(top).enumerate() {
            let indent = "    ".repeat(level);
            let name = ellipsize(&label(c), 44usize.saturating_sub(level * 4).max(16));
            let width = 46usize.saturating_sub(level * 4).max(18);
            let _ = writeln!(
                s,
                "  {indent}{:>3}  {name:<width$} {:>9} {:>6.1}%  {:>11} files  {}",
                i + 1,
                format_size(c.size),
                pct(c.size, whole),
                format_count(c.files),
                if c.kind == Kind::Rest { String::new() } else { chain_text(c) },
            );
            if level + 1 < depth && c.kind == Kind::Dir && !c.children.is_empty() {
                rows(s, c, whole, top, depth, level + 1);
            }
        }
    }
    rows(&mut s, root, root.size, top, depth, 0);

    let _ = writeln!(s, "\nHotspots ({}):", r.hotspots.len());
    for (i, h) in r.hotspots.iter().enumerate() {
        let tag = match (h.residual, h.kind) {
            (true, _) => "  (rest)",
            (false, Kind::File) => "  [file]",
            _ => "",
        };
        let _ = writeln!(s, "  {:>3}  {:>9}  {}{}", i + 1, format_size(h.size), h.display, tag);
    }
    s
}
