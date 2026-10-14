# How Diskscope works

Diskscope is a native Windows app in Rust, built on eframe/egui for the UI, rayon for the
parallel scan and windows-sys for the Win32 calls.

## Modules

| File | Contents |
|---|---|
| `src/main.rs` | Entry point. `--cli` goes to `cli.rs`, everything else opens the window. |
| `src/model.rs` | The tree (`Node`), scan results, hotspots, `focus_chain`, size formatting. |
| `src/scan.rs` | Starts a scan on its own thread and picks the method. Holds the parallel folder walk. |
| `src/scan/mft.rs` | The NTFS master file table reader. |
| `src/hotspots.rs` | The "Where your space is" list. |
| `src/drives.rs` | Drive list, elevation check, "Restart as admin", opening Explorer. |
| `src/cli.rs` | The `--cli` text report. |
| `src/app.rs` | The window: sidebar, breadcrumb, treemap, list, hotspot panel, test flags. |
| `src/treemap.rs` | Squarified treemap layout and drawing. |
| `src/theme.rs` | Colours, fonts and the egui style. |

## Scanning

`scan::start` returns a `ScanJob` at once and fills in its result on a background thread. The UI
polls the job's progress counters and repaints about ten times a second while a scan runs.

The scanner reads the MFT when the target is an NTFS drive root and the process is elevated.
Otherwise it walks the folders.

**The MFT reader** opens the raw volume (`\\.\C:`) and reads the boot sector for the geometry.
It finds the extents of the `$MFT` file in record 0, following its `$ATTRIBUTE_LIST` when the
table is very fragmented. Then it reads the whole table in 4 MB chunks through one overlapped
handle with 16 reads in flight, and parses each chunk on the thread pool while the next chunks
load. Every record gets its update-sequence fixups checked. Each in-use base record supplies a
name, a parent and a size, and extension records add their names and data streams to their base
record. The tree is built from the parent references, starting at record 5, the root folder.
The NTFS metadata files (`$MFT`, `$LogFile`, `$UsnJrnl` and the rest) and the folders' index
blocks appear as one node at the root of a drive scan, sized by what they occupy.

**The folder walk** runs on a rayon pool of 16 to 64 threads, more threads than cores because
the work waits on the disk. It opens each folder relative to its parent's handle (`NtOpenFile`)
and lists it with `NtQueryDirectoryFile` into a 64 KB buffer, so a typical folder costs one open
and one or two listing calls. Sizes and attributes come from the listing, with no system call
per file.

Both methods follow the same counting rules:

- They skip symlinks and junctions entirely, so nothing is counted twice and nothing loops.
- A file with several hard links counts once.
- Cloud placeholders (`FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` or `FILE_ATTRIBUTE_OFFLINE`) take
  0 bytes on disk.
- Each folder keeps its 64 largest files as separate nodes. The rest fold into one node named
  like `312 smaller files`.
- A folder the scan can't read is marked `denied`, has size 0 and adds to the "no access" count.
- Every node holds two sizes, the space on disk and the file length. The "On disk" / "Size"
  toggle swaps them, and sorting, the treemap and the hotspot list all follow.

## The hotspot list

The goal is a short list (40 entries at most) of specific places that hold the space, with no
overlap, largest first. `hotspots::compute` runs a max-heap by size:

1. Set a floor, `min = clamp(0.4% of the scan, 64 MB, 1 GB)`. Nothing smaller is pushed or listed.
2. Push the root. Then pop the largest node `n` until the heap is empty.
3. A file is listed as is. A "smaller files" node is skipped.
4. A folder is split when it is the root, when its largest child holds at least 30% of it, or
   when it holds at least 10% of the whole scan and its largest child holds at least 10% of it.
   Otherwise the folder itself is listed, because its space is spread over many things and the
   folder is the most specific answer.
5. Splitting pushes every child of at least `max(min, 2% of n)` (just `min` at the root). The
   space left over, `n` minus the pushed children, is listed as `rest of n` if it is at least
   `min`.
6. Sort by size and keep the top 40. An entry that misses the cut is added to its parent's
   `rest of` entry when that one is listed.

Example: `kapa` (30 GB) holds `rapa` (20 GB), which holds `kona` (14 GB) and six 1 GB folders.
The list contains `kona 14 GB`, the six 1 GB folders by name (each is 5% of `rapa`) and
`rest of kapa 10 GB`. The UI shows the `rest of` entries in their own section below the named
places. `hotspots.rs` has unit tests for this tree.

The child threshold is low (2%) on purpose. At 10%, a folder full of medium-sized subfolders
showed up as one large `rest of` entry that hid them.

## Chain hints

`model::focus_chain` follows the dominant child down from a folder. A child is dominant when it
holds at least half of its parent (`FOCUS_SHARE = 0.5`). The chain stops at a file, at a folder
with no dominant child, or before a "smaller files" node. The folder list shows a non-empty
chain as `→ rapa › kona · 14 GB`.

## Testing the UI without a visible window

The app has flags for automated screenshots:

| Flag | Effect |
|---|---|
| `--demo` | Load a built-in synthetic drive instead of scanning anything |
| `--demo-scanning` | Same, plus a scan that runs forever, for the progress view |
| `--shot FILE --after SECS` | Save a PNG of the window after SECS seconds, then exit |
| `--nav a\b\c` | Open this folder under the scan root before the shot |
| `--size WxH` | Window size |
| `--hover X,Y`, `--click X,Y`, `--rclick X,Y` | Fake the pointer at a window position |
| `--keys down;down;enter` | Inject key presses, one per frame |
| `--hover-hotspots`, `--scroll-hotspots N`, `--no-hotspots` | Hotspot panel states |
| `--metric ondisk\|size` | Start with this size measure |

`tools\run_hidden.ps1` runs the app on a separate Windows desktop that is never shown, so a
test run can't take your screen, focus or mouse:

```powershell
powershell -File tools\run_hidden.ps1 -Exe target\release\diskscope.exe `
    -Args '--demo','--shot','C:\tmp\overview.png','--after','2' -Seconds 30
```

The README screenshots come from these commands with `--demo`.
