//! The smart "where is my space" list (ARCHITECTURE.md, "The hotspot list").
//!
//! A max-heap walk from the root: a folder whose space is dominated by a few children is split
//! into them (plus a "rest of" entry if what is left is still big); a folder whose space is
//! spread over many things is itself the answer. The result is non-overlapping.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::model::{Hotspot, Kind, Node};

const MB: u64 = 1 << 20;
const GB: u64 = 1 << 30;

/// A folder is split when its largest child holds this share of it...
const SPLIT_SHARE: f64 = 0.30;
/// ...or when the folder holds this share of the scan and its largest child this share of it.
const BIG_FOLDER_SHARE: f64 = 0.10;
const BIG_FOLDER_CHILD_SHARE: f64 = 0.10;
/// When splitting, children of at least this share of the folder (and `min`) get their own
/// entry; the rest stays in a "(rest)" entry for the folder. Entries that miss the final cap
/// are folded back into their nearest listed "(rest)" ancestor.
const CHILD_SHARE: f64 = 0.02;

/// Nothing smaller than this is pushed or listed: 0.4 % of the scan, clamped to 64 MB ..= 1 GB.
pub fn min_size(root_size: u64) -> u64 {
    ((root_size as f64 * 0.004) as u64).clamp(64 * MB, GB)
}

struct Item<'a> {
    size: u64,
    path: Vec<usize>,
    node: &'a Node,
}

impl PartialEq for Item<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Item<'_> {}
impl PartialOrd for Item<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Item<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // largest first; on ties the earlier (lexicographically smaller) path first
        self.size.cmp(&other.size).then_with(|| other.path.cmp(&self.path))
    }
}

fn display_path(root: &Node, root_path: &str, path: &[usize]) -> String {
    let mut s = root_path.to_string();
    let mut n = root;
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

pub fn compute(root: &Node, root_path: &str, max: usize) -> Vec<Hotspot> {
    let min = min_size(root.size);
    let root_size = root.size as f64;
    let mut out: Vec<Hotspot> = Vec::new();
    let mut emit = |path: &[usize], size: u64, residual: bool, kind: Kind| {
        out.push(Hotspot {
            path: path.to_vec(),
            display: display_path(root, root_path, path),
            size,
            residual,
            kind,
        });
    };

    let mut heap = BinaryHeap::new();
    if root.size >= min && root.kind != Kind::Rest {
        heap.push(Item { size: root.size, path: Vec::new(), node: root });
    }

    while let Some(Item { size, path, node }) = heap.pop() {
        match node.kind {
            Kind::File => emit(&path, size, false, Kind::File),
            Kind::Rest => {}
            Kind::Dir => {
                let is_root = path.is_empty();
                let n = size as f64;
                // A Rest aggregate is not a place, so it never counts as the "largest child".
                let big = node
                    .children
                    .iter()
                    .find(|c| c.kind != Kind::Rest)
                    .map_or(0, |c| c.size) as f64;
                let split = is_root
                    || big >= SPLIT_SHARE * n
                    || (n >= BIG_FOLDER_SHARE * root_size && big >= BIG_FOLDER_CHILD_SHARE * n);
                if !split {
                    emit(&path, size, false, Kind::Dir);
                    continue;
                }
                // The root has no "rest" entry, so it hands down everything that passes `min`.
                let threshold = if is_root { min } else { min.max((CHILD_SHARE * n).ceil() as u64) };
                let mut pushed = 0u64;
                let mut any = false;
                for (i, c) in node.children.iter().enumerate() {
                    if c.kind != Kind::Rest && c.size >= threshold {
                        let mut p = path.clone();
                        p.push(i);
                        heap.push(Item { size: c.size, path: p, node: c });
                        pushed += c.size;
                        any = true;
                    }
                }
                if !is_root {
                    let residual = size.saturating_sub(pushed);
                    if residual >= min {
                        // nothing handed down: the folder itself is the answer, not "its rest"
                        emit(&path, residual, any, Kind::Dir);
                    }
                }
            }
        }
    }

    let by_size = |a: &Hotspot, b: &Hotspot| b.size.cmp(&a.size).then_with(|| a.display.cmp(&b.display));
    out.sort_by(by_size);
    if out.len() > max {
        // Fold what misses the cap into its parent's "(rest)" when that is listed, so a residual
        // is exactly what you see when you open the folder minus the entries named inside it.
        let dropped = out.split_off(max);
        let rest_at: std::collections::HashMap<Vec<usize>, usize> = out
            .iter()
            .enumerate()
            .filter(|(_, h)| h.residual)
            .map(|(i, h)| (h.path.clone(), i))
            .collect();
        for d in dropped {
            if let Some(&i) = d.path.split_last().and_then(|(_, parent)| rest_at.get(parent)) {
                out[i].size += d.size;
            }
        }
        out.sort_by(by_size);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::focus_chain;

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    fn file(name: &str, size: u64) -> Node {
        Node {
            name: name.into(),
            size,
            files: 1,
            dirs: 0,
            kind: Kind::File,
            other: 0,
            denied: false,
            children: Vec::new(),
        }
    }

    fn rest(count: u64, size: u64) -> Node {
        Node {
            name: format!("{count} smaller files").into(),
            size,
            files: count,
            dirs: 0,
            kind: Kind::Rest,
            other: 0,
            denied: false,
            children: Vec::new(),
        }
    }

    fn dir(name: &str, mut children: Vec<Node>) -> Node {
        children.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
        Node {
            name: name.into(),
            size: children.iter().map(|c| c.size).sum(),
            files: children.iter().map(|c| c.files).sum(),
            dirs: children.iter().map(|c| c.dirs + c.is_dir() as u64).sum(),
            kind: Kind::Dir,
            other: 0,
            denied: false,
            children,
        }
    }

    /// A folder of `n` files of `each` bytes: big as a whole, but spread.
    fn spread(name: &str, n: usize, each: u64) -> Node {
        dir(name, (0..n).map(|i| file(&format!("f{i:03}.bin"), each)).collect())
    }

    /// C: (100 GiB) = Windows (70 GiB, spread over 70 folders) + kapa (30 GiB).
    /// kapa = rapa (20 GiB) + 20 folders of 512 MiB; rapa = kona (14 GiB) + six 1 GiB folders;
    /// kona = 112 files of 128 MiB.
    fn kapa_tree() -> Node {
        let kona = spread("kona", 112, 128 * MIB);
        let mut rapa_children = vec![kona];
        for i in 0..6 {
            rapa_children.push(spread(&format!("r{i}"), 8, 128 * MIB));
        }
        let rapa = dir("rapa", rapa_children);
        let mut kapa_children = vec![rapa];
        for i in 0..20 {
            kapa_children.push(spread(&format!("k{i:02}"), 4, 128 * MIB));
        }
        let kapa = dir("kapa", kapa_children);
        let windows = dir(
            "Windows",
            (0..70).map(|i| spread(&format!("w{i:02}"), 8, 128 * MIB)).collect(),
        );
        dir("C:", vec![windows, kapa])
    }

    fn names(hs: &[Hotspot]) -> Vec<(String, u64, bool)> {
        hs.iter().map(|h| (h.display.clone(), h.size, h.residual)).collect()
    }

    #[test]
    fn kapa_rapa_kona() {
        let root = kapa_tree();
        assert_eq!(root.size, 100 * GIB);
        let hs = compute(&root, "C:\\", 40);
        assert_eq!(
            names(&hs),
            vec![
                ("C:\\Windows".to_string(), 70 * GIB, false),
                ("C:\\kapa\\rapa\\kona".to_string(), 14 * GIB, false),
                ("C:\\kapa".to_string(), 10 * GIB, true),
                // rapa's six 1 GiB folders are 5 % each, above CHILD_SHARE: listed by name
                ("C:\\kapa\\rapa\\r0".to_string(), GIB, false),
                ("C:\\kapa\\rapa\\r1".to_string(), GIB, false),
                ("C:\\kapa\\rapa\\r2".to_string(), GIB, false),
                ("C:\\kapa\\rapa\\r3".to_string(), GIB, false),
                ("C:\\kapa\\rapa\\r4".to_string(), GIB, false),
                ("C:\\kapa\\rapa\\r5".to_string(), GIB, false),
            ]
        );
        // capped at 3: the r* folders miss the cap; their parent rapa has no listed rest, so
        // they drop and "rest of kapa" stays what kapa holds besides rapa
        let capped = compute(&root, "C:\\", 3);
        assert_eq!(
            names(&capped),
            vec![
                ("C:\\Windows".to_string(), 70 * GIB, false),
                ("C:\\kapa\\rapa\\kona".to_string(), 14 * GIB, false),
                ("C:\\kapa".to_string(), 10 * GIB, true),
            ]
        );
        // index paths resolve to the named nodes
        let kona = root.get(&hs[1].path).unwrap();
        assert_eq!(&*kona.name, "kona");
        assert_eq!(hs[1].kind, Kind::Dir);
        // non-overlapping: the entries add up to the whole drive
        assert_eq!(hs.iter().map(|h| h.size).sum::<u64>(), root.size);
    }

    #[test]
    fn focus_chain_on_kapa() {
        let root = kapa_tree();
        let kapa_idx = root.children.iter().position(|c| &*c.name == "kapa").unwrap();
        let kapa = &root.children[kapa_idx];
        let chain = focus_chain(kapa);
        assert_eq!(chain, vec![0, 0]);
        let end = kapa.get(&chain).unwrap();
        assert_eq!(&*end.name, "kona");
        assert_eq!(&*kapa.children[chain[0]].name, "rapa");
        // at the root, Windows holds 70 % and is spread inside: the chain is just Windows
        let root_chain = focus_chain(&root);
        assert_eq!(root_chain.len(), 1);
        assert_eq!(&*root.get(&root_chain).unwrap().name, "Windows");
        // a spread folder has no chain
        assert!(focus_chain(&root.children[0]).is_empty());
    }

    #[test]
    fn focus_chain_skips_rest_and_stops_at_file() {
        // the Rest aggregate sorts first but is never a target; the big file is
        let d = dir("d", vec![rest(500, 6 * GIB), file("big.iso", 5 * GIB)]);
        let top = dir("top", vec![d, file("small", GIB)]);
        assert_eq!(focus_chain(&top), vec![0]);
        assert_eq!(focus_chain(&top.children[0]), Vec::<usize>::new()); // 5 of 11 GiB < 50 %
        let d2 = dir("d2", vec![rest(500, GIB), file("big.iso", 5 * GIB)]);
        let chain = focus_chain(&d2);
        assert_eq!(chain, vec![0]);
        assert_eq!(d2.get(&chain).unwrap().kind, Kind::File);
    }

    #[test]
    fn spread_folder_is_emitted_not_split() {
        // data: 40 GiB over 40 folders of 1 GiB; a 60 GiB file next to it
        let data = dir("data", (0..40).map(|i| spread(&format!("d{i:02}"), 8, 128 * MIB)).collect());
        let root = dir("D:", vec![data, file("disk.vhdx", 60 * GIB)]);
        let hs = compute(&root, "D:\\", 40);
        assert_eq!(
            names(&hs),
            vec![
                ("D:\\disk.vhdx".to_string(), 60 * GIB, false),
                ("D:\\data".to_string(), 40 * GIB, false),
            ]
        );
        assert_eq!(hs[0].kind, Kind::File);
        assert_eq!(hs[1].kind, Kind::Dir);
    }

    #[test]
    fn root_is_always_split() {
        // 20 folders of 5 GiB: no child reaches 30 % or even 10 %, the root is still split
        let root = dir("E:", (0..20).map(|i| spread(&format!("p{i:02}"), 50, 100 * MIB)).collect());
        let hs = compute(&root, "E:\\", 40);
        assert_eq!(hs.len(), 20);
        assert!(hs.iter().all(|h| !h.path.is_empty() && !h.residual && h.path.len() == 1));
        assert!(hs.iter().all(|h| h.size == 5000 * MIB));
        assert_eq!(hs[0].display, "E:\\p00");
    }

    #[test]
    fn small_rest_is_dropped_and_cap_applies() {
        // a: 10 GiB = one 9.9 GiB file + 100 MiB of small stuff (below min): no residual
        let a = dir("a", vec![file("huge.bin", 10 * GIB - 100 * MIB), rest(1000, 100 * MIB)]);
        let mut kids = vec![a];
        for i in 0..50 {
            kids.push(file(&format!("m{i:02}.mkv"), 2 * GIB));
        }
        let root = dir("F:", kids);
        let hs = compute(&root, "F:\\", 40);
        assert_eq!(hs.len(), 40);
        assert_eq!(hs[0].display, "F:\\a\\huge.bin");
        assert!(hs.iter().all(|h| !h.residual));
        assert!(hs.windows(2).all(|w| w[0].size >= w[1].size));
        // a tiny scan lists nothing
        let tiny = dir("t", vec![file("x", 10 * MIB)]);
        assert!(compute(&tiny, "T:\\", 40).is_empty());
    }
}
