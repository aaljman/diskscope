//! The window: drives sidebar, browse view (breadcrumb, header, treemap, list), hotspots panel,
//! scanning view, and the test flags (`--demo`, `--shot`, `--nav`, ...).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Color32, CornerRadius, Event, Frame, Key, Margin, PointerButton, Pos2, Rect,
    Sense, Stroke, StrokeKind, Ui, ViewportCommand, pos2, vec2,
};

use crate::model::{
    DriveInfo, DriveKind, Hotspot, Kind, Metric, Node, ScanJob, ScanResult, focus_chain, format_count,
    format_size,
};
use crate::theme::{self, ACCENT, BG, LINE, SIDE, SURFACE, TEXT, TEXT_DIM, TEXT_FAINT, mix};
use crate::treemap::{self, Target};
use crate::{drives, scan};

const MIN_W: f32 = 1000.0;
const MIN_H: f32 = 640.0;
/// Scans that finish faster than this never show the progress view.
const PROGRESS_DELAY: Duration = Duration::from_millis(300);
const ROW_H: f32 = 34.0;

// ---------------------------------------------------------------------------------------------
// Command line

#[derive(Default, Clone)]
struct Opts {
    /// Folder or drive to scan at start.
    path: Option<String>,
    /// Synthetic data, no disk access (`--demo`); `--demo-scanning` also shows a scan running.
    demo: bool,
    demo_scanning: bool,
    /// Screenshot the window to this PNG, then exit.
    shot: Option<PathBuf>,
    after: f32,
    /// Relative path under the scan root to open before the shot.
    nav: Option<String>,
    size: Option<(f32, f32)>,
    hover_hotspots: bool,
    /// Scroll the hotspots panel this far down (points), once.
    scroll_hotspots: Option<f32>,
    /// `--metric ondisk|size`: start with this measure (tests; the saved preference otherwise).
    metric: Option<Metric>,
    /// Pretend the pointer rests here (window points), for hover / tooltip shots.
    hover: Option<Pos2>,
    /// Inject a primary / secondary click here (window points), once the scan has a result.
    click: Option<Pos2>,
    rclick: Option<Pos2>,
    /// Key presses to inject, one per frame after 1 s: `down`, `up`, `enter`, `backspace`,
    /// `alt+left`, `alt+right`, `alt+up`, `esc`, `f5` (comma- or plus-free list: `down;down;enter`).
    keys: Vec<String>,
    no_hotspots: bool,
}

impl Opts {
    fn parse(args: &[String]) -> Self {
        let mut o = Opts { after: 1.5, ..Default::default() };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--demo" => o.demo = true,
                "--demo-scanning" => {
                    o.demo = true;
                    o.demo_scanning = true;
                }
                "--shot" => o.shot = it.next().map(PathBuf::from),
                "--after" => o.after = it.next().and_then(|s| s.parse().ok()).unwrap_or(1.5),
                "--nav" => o.nav = it.next().cloned(),
                "--size" => {
                    o.size = it.next().and_then(|s| {
                        let (w, h) = s.split_once(['x', 'X'])?;
                        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
                    })
                }
                "--hover" => o.hover = it.next().and_then(|s| parse_pos(s)),
                "--click" => o.click = it.next().and_then(|s| parse_pos(s)),
                "--rclick" => o.rclick = it.next().and_then(|s| parse_pos(s)),
                "--keys" => {
                    o.keys = it.next().map(|s| s.split(';').map(str::to_owned).collect()).unwrap_or_default()
                }
                "--hover-hotspots" => o.hover_hotspots = true,
                "--scroll-hotspots" => o.scroll_hotspots = it.next().and_then(|s| s.parse().ok()),
                "--metric" => {
                    o.metric = match it.next().map(|s| s.to_lowercase()).as_deref() {
                        Some("size" | "logical") => Some(Metric::Logical),
                        Some(_) => Some(Metric::OnDisk),
                        None => None,
                    }
                }
                "--no-hotspots" => o.no_hotspots = true,
                "--no-mouse" => {}
                s if !s.starts_with("--") && o.path.is_none() => o.path = Some(s.to_owned()),
                _ => {}
            }
        }
        o
    }
}

/// `120x340`, `120,340` or `120:340`.
fn parse_pos(s: &str) -> Option<Pos2> {
    let (x, y) = s.split_once([',', 'x', ':'])?;
    Some(pos2(x.trim().parse().ok()?, y.trim().parse().ok()?))
}

pub fn run(args: Vec<String>) {
    let opts = Opts::parse(&args);
    if let Some(shot) = &opts.shot {
        // test runs happen on a hidden desktop: leave a note if anything panics
        let note = shot.with_extension("panic.txt");
        let _ = std::fs::remove_file(&note);
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&note)
                .and_then(|mut f| {
                    use std::io::Write;
                    writeln!(f, "{info}")
                });
            default_hook(info);
        }));
    }
    let (w, h) = opts.size.unwrap_or((1440.0, 900.0));
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("diskscope")
            .with_inner_size([w, h])
            .with_min_inner_size([MIN_W.min(w), MIN_H.min(h)])
            .with_icon(Arc::new(app_icon())),
        centered: true,
        ..Default::default()
    };
    let _ = eframe::run_native(
        "diskscope",
        native,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            Ok(Box::new(App::new(opts)))
        }),
    );
}

/// The window / taskbar icon: the same art as the exe's embedded .ico (assets\make_icon.py).
fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon_256.png")).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Preference: which size to show. A one-line file in %APPDATA%\diskscope (test runs never
// read or write it).

fn settings_path() -> Option<PathBuf> {
    let dir = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(dir).join("diskscope").join("settings.txt"))
}

fn load_metric() -> Option<Metric> {
    let text = std::fs::read_to_string(settings_path()?).ok()?;
    text.lines().find_map(|l| match l.trim() {
        "metric=ondisk" => Some(Metric::OnDisk),
        "metric=size" => Some(Metric::Logical),
        _ => None,
    })
}

fn save_metric(m: Metric) {
    let Some(path) = settings_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let v = if m == Metric::OnDisk { "ondisk" } else { "size" };
    let _ = std::fs::write(path, format!("metric={v}\n"));
}

/// A place in a tree remembered by names (index paths change when the metric switches).
type NamedPlace = (Vec<String>, Option<String>);

fn place_names(root: &Node, path: &[usize], selected: Option<usize>) -> NamedPlace {
    let sel = selected.and_then(|k| root.get(path)?.children.get(k)).map(|c| c.name.to_string());
    (names_of(root, path), sel)
}

fn place_find(root: &Node, place: &NamedPlace) -> (Vec<usize>, Option<usize>) {
    let (path, _) = resolve_names(root, &place.0);
    let sel = place.1.as_ref().and_then(|name| root.get(&path)?.children.iter().position(|c| *c.name == **name));
    (path, sel)
}

// ---------------------------------------------------------------------------------------------
// State

#[derive(Default)]
struct Nav {
    path: Vec<usize>,
    selected: Option<usize>,
    back: Vec<(Vec<usize>, Option<usize>)>,
    forward: Vec<(Vec<usize>, Option<usize>)>,
    /// Scroll the list to the selected row on the next frame.
    reveal: bool,
}

impl Nav {
    fn go(&mut self, path: Vec<usize>, selected: Option<usize>) {
        if path != self.path {
            self.back.push((std::mem::take(&mut self.path), self.selected));
            self.forward.clear();
            self.path = path;
        }
        self.selected = selected;
        self.reveal = selected.is_some();
    }
    fn back(&mut self) {
        if let Some((p, s)) = self.back.pop() {
            self.forward.push((std::mem::replace(&mut self.path, p), self.selected));
            self.selected = s;
            self.reveal = s.is_some();
        }
    }
    fn forward(&mut self) {
        if let Some((p, s)) = self.forward.pop() {
            self.back.push((std::mem::replace(&mut self.path, p), self.selected));
            self.selected = s;
            self.reveal = s.is_some();
        }
    }
    fn up(&mut self) {
        if let Some(last) = self.path.last().copied() {
            let mut p = self.path.clone();
            p.pop();
            self.go(p, Some(last));
        }
    }
}

struct Scan {
    root_path: String,
    job: Option<ScanJob>,
    result: Option<ScanResult>,
    done_at: Option<Instant>,
    nav: Nav,
    /// Folder names to reopen when a rescan finishes.
    restore: Option<Vec<String>>,
    /// Progress counters read once per frame (files, folders, bytes, denied), so every place
    /// that shows them agrees.
    snap: [u64; 4],
}

impl Scan {
    fn new(root_path: String, job: ScanJob) -> Self {
        Self { root_path, job: Some(job), result: None, done_at: None, nav: Nav::default(), restore: None, snap: [0; 4] }
    }
    fn scanning(&self) -> bool {
        self.job.is_some()
    }
}

enum Action {
    SelectScan(usize),
    ScanPath(String),
    Rescan(usize),
    Cancel(usize),
    Go(Vec<usize>, Option<usize>),
    Select(Option<usize>),
    RevealDone,
    Back,
    Forward,
    Up,
    OpenExplorer(String),
    Reveal(String),
    Copy(String),
    RestartAdmin,
    ShowHotspots(bool),
    SetMetric(Metric),
}

#[derive(PartialEq)]
enum ShotState {
    Off,
    Waiting,
    Requested,
    Done,
}

struct App {
    opts: Opts,
    drives: Vec<DriveInfo>,
    elevated: bool,
    scans: Vec<Scan>,
    current: Option<usize>,
    folder_input: String,
    /// Shown under the folder field when a path can't be scanned.
    input_error: Option<String>,
    show_hotspots: bool,
    started: Instant,
    shot: ShotState,
    nav_pending: Option<String>,
    actions: Vec<Action>,
    last_hover_inject: Option<Instant>,
    clicked: bool,
    title: String,
    title_key: u64,
    fitted: bool,
    /// When the current scan first had a result (test input waits for it).
    settled_at: Option<Instant>,
    /// Per-frame ui time in ms, kept only for test shots.
    frame_times: Vec<f32>,
    /// The measure everything shows (global; applied to each scan when it is shown).
    metric: Metric,
    key_index: usize,
}

fn norm(p: &str) -> String {
    p.trim().trim_end_matches(['\\', '/']).to_lowercase()
}

impl App {
    fn new(opts: Opts) -> Self {
        let (drives, elevated) = if opts.demo {
            (demo::drives(), false)
        } else {
            (drives::list(), drives::is_elevated())
        };
        let mut app = App {
            show_hotspots: !opts.no_hotspots,
            shot: if opts.shot.is_some() { ShotState::Waiting } else { ShotState::Off },
            nav_pending: opts.nav.clone(),
            opts,
            drives,
            elevated,
            scans: Vec::new(),
            current: None,
            folder_input: String::new(),
            input_error: None,
            started: Instant::now(),
            actions: Vec::new(),
            last_hover_inject: None,
            clicked: false,
            title: "diskscope".to_owned(),
            title_key: 0,
            fitted: false,
            settled_at: None,
            frame_times: Vec::new(),
            metric: Metric::OnDisk,
            key_index: 0,
        };
        app.metric = match app.opts.metric {
            Some(m) => m,
            None if app.opts.shot.is_none() => load_metric().unwrap_or(Metric::OnDisk),
            None => Metric::OnDisk,
        };
        if app.opts.demo {
            let r = demo::result();
            app.scans.push(Scan {
                root_path: r.root_path.clone(),
                job: None,
                result: Some(r),
                done_at: Some(Instant::now() - Duration::from_secs(140)),
                nav: Nav::default(),
                restore: None,
                snap: [0; 4],
            });
            app.current = Some(0);
            if app.opts.demo_scanning {
                app.scans.push(Scan::new("D:\\".into(), demo::scanning_job("D:\\")));
                app.current = Some(1);
            }
            if let Some(p) = app.opts.path.clone() {
                app.start_scan(p);
            }
        } else if let Some(p) = app.opts.path.clone() {
            app.start_scan(p);
        }
        app
    }

    fn start_scan(&mut self, path: String) {
        let path = path.trim().trim_matches('"').to_owned();
        if path.is_empty() {
            return;
        }
        // `C:` means the drive root
        let path = if path.len() == 2 && path.ends_with(':') { format!("{path}\\") } else { path };
        if !self.opts.demo && !std::path::Path::new(&path).is_dir() {
            self.input_error = Some(format!("No folder at {path}"));
            self.folder_input = path;
            return;
        }
        self.input_error = None;
        if let Some(i) = self.scans.iter().position(|s| norm(&s.root_path) == norm(&path)) {
            self.current = Some(i);
            if self.scans[i].result.is_none() && self.scans[i].job.is_none() {
                self.rescan(i);
            }
            return;
        }
        let job = if self.opts.demo { demo::scanning_job(&path) } else { scan::start(path.clone()) };
        self.scans.push(Scan::new(path, job));
        self.current = Some(self.scans.len() - 1);
    }

    /// Brings scan `i` to the preferred metric. The tree re-sorts, so the open folder, the
    /// selection and the back/forward history are carried over by names.
    fn apply_metric(&mut self, i: usize) {
        let m = self.metric;
        let Some(s) = self.scans.get_mut(i) else { return };
        let Some(r) = s.result.as_mut() else { return };
        if r.metric == m {
            return;
        }
        let cur = place_names(&r.root, &s.nav.path, s.nav.selected);
        let back: Vec<NamedPlace> = s.nav.back.iter().map(|(p, k)| place_names(&r.root, p, *k)).collect();
        let fwd: Vec<NamedPlace> = s.nav.forward.iter().map(|(p, k)| place_names(&r.root, p, *k)).collect();
        r.set_metric(m);
        let (path, sel) = place_find(&r.root, &cur);
        s.nav.path = path;
        s.nav.selected = sel;
        s.nav.reveal = sel.is_some();
        s.nav.back = back.iter().map(|pl| place_find(&r.root, pl)).collect();
        s.nav.forward = fwd.iter().map(|pl| place_find(&r.root, pl)).collect();
    }

    fn rescan(&mut self, i: usize) {
        let demo = self.opts.demo;
        let s = &mut self.scans[i];
        if s.job.is_some() {
            return;
        }
        if let Some(r) = &s.result {
            s.restore = Some(names_of(&r.root, &s.nav.path));
        }
        s.job = Some(if demo { demo::scanning_job(&s.root_path) } else { scan::start(s.root_path.clone()) });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let mut any_running = false;
        let mut finished = false;
        let metric = self.metric;
        for s in &mut self.scans {
            let Some(job) = &s.job else { continue };
            {
                use std::sync::atomic::Ordering::Relaxed;
                let p = &job.progress;
                s.snap = [p.files.load(Relaxed), p.dirs.load(Relaxed), p.bytes.load(Relaxed), p.denied.load(Relaxed)];
            }
            if let Some(mut r) = job.take_result() {
                // new results come in the preferred measure (a no-op when they already are)
                r.set_metric(metric);
                let restore = s.restore.take();
                s.nav = Nav::default();
                if let Some(names) = restore {
                    let (path, _) = resolve_names(&r.root, &names);
                    s.nav.path = path;
                }
                s.result = Some(r);
                s.job = None;
                s.done_at = Some(Instant::now());
                finished = true;
            } else {
                any_running = true;
            }
        }
        if finished && !self.opts.demo {
            self.drives = drives::list();
        }
        // --nav: open a folder once the current scan has a result
        if let Some(nav) = self.nav_pending.clone()
            && let Some(s) = self.current.and_then(|i| self.scans.get_mut(i))
            && let Some(r) = &s.result
        {
            let names: Vec<String> =
                nav.split(['\\', '/']).filter(|x| !x.is_empty()).map(str::to_owned).collect();
            let (path, sel) = resolve_names(&r.root, &names);
            s.nav.go(path, sel);
            self.nav_pending = None;
        }
        if any_running {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn apply(&mut self, ctx: &egui::Context) {
        let actions = std::mem::take(&mut self.actions);
        for a in actions {
            match a {
                Action::SelectScan(i) => {
                    self.current = Some(i);
                    let s = &self.scans[i];
                    if s.result.is_none() && s.job.is_none() {
                        self.rescan(i);
                    }
                }
                Action::ScanPath(p) => self.start_scan(p),
                Action::Rescan(i) => self.rescan(i),
                Action::Cancel(i) => {
                    if let Some(j) = &self.scans[i].job {
                        j.progress.cancel();
                    }
                }
                Action::Go(p, sel) => {
                    if let Some(s) = self.cur_mut() {
                        s.nav.go(p, sel);
                    }
                }
                Action::Select(sel) => {
                    if let Some(s) = self.cur_mut() {
                        s.nav.selected = sel;
                    }
                }
                Action::RevealDone => {
                    if let Some(s) = self.cur_mut() {
                        s.nav.reveal = false;
                    }
                }
                Action::Back => {
                    if let Some(s) = self.cur_mut() {
                        s.nav.back();
                    }
                }
                Action::Forward => {
                    if let Some(s) = self.cur_mut() {
                        s.nav.forward();
                    }
                }
                Action::Up => {
                    if let Some(s) = self.cur_mut() {
                        s.nav.up();
                    }
                }
                Action::OpenExplorer(p) => drives::open_in_explorer(&p),
                Action::Reveal(p) => drives::reveal_in_explorer(&p),
                Action::Copy(p) => ctx.copy_text(p),
                Action::RestartAdmin => {
                    let mut args = Vec::new();
                    if let Some(s) = self.cur() {
                        args.push(s.root_path.clone());
                    }
                    if drives::try_restart_elevated(&args) {
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                }
                Action::ShowHotspots(v) => self.show_hotspots = v,
                Action::SetMetric(m) => {
                    if m != self.metric {
                        self.metric = m;
                        if self.opts.shot.is_none() {
                            save_metric(m);
                        }
                        if let Some(i) = self.current {
                            self.apply_metric(i);
                        }
                    }
                }
            }
        }
    }

    /// The default 1440x900 can be larger than a small or highly scaled screen: shrink once.
    fn fit_to_monitor(&mut self, ctx: &egui::Context) {
        if self.fitted || self.opts.size.is_some() {
            return;
        }
        let (mon, inner, outer) =
            ctx.input(|i| (i.viewport().monitor_size, i.viewport().inner_rect, i.viewport().outer_rect));
        let (Some(mon), Some(inner), Some(outer)) = (mon, inner, outer) else { return };
        self.fitted = true;
        let w = inner.width().min(mon.x * 0.92).max(MIN_W.min(mon.x));
        let h = inner.height().min(mon.y * 0.88).max(MIN_H.min(mon.y));
        if w + 1.0 < inner.width() || h + 1.0 < inner.height() {
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(vec2(w, h)));
            // keep it centred where the system put it
            let shrink = vec2(inner.width() - w, inner.height() - h);
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(outer.min + shrink / 2.0));
        }
    }

    /// `Sam — diskscope`: the window title follows the open folder.
    fn update_title(&mut self, ctx: &egui::Context) {
        // rebuild only when the place changes (the path can be thousands of levels deep)
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            self.current.hash(&mut h);
            if let Some(s) = self.cur() {
                s.nav.path.hash(&mut h);
                s.result.is_some().hash(&mut h);
                s.done_at.hash(&mut h);
            }
            h.finish()
        };
        if key == self.title_key {
            return;
        }
        self.title_key = key;
        let title = match self.cur() {
            Some(s) => match &s.result {
                Some(r) => {
                    let path = if r.root.get(&s.nav.path).is_some() { s.nav.path.as_slice() } else { &[] };
                    let full = crate::model::path_string(r, path);
                    format!("{} — diskscope", theme::shorten_chars(&full, 160))
                }
                None => format!("{} — diskscope", s.root_path),
            },
            None => "diskscope".to_owned(),
        };
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    fn cur(&self) -> Option<&Scan> {
        self.current.and_then(|i| self.scans.get(i))
    }
    fn cur_mut(&mut self) -> Option<&mut Scan> {
        self.current.and_then(|i| self.scans.get_mut(i))
    }

    fn keys(&mut self, ctx: &egui::Context) {
        if ctx.text_edit_focused() {
            return;
        }
        let (back, fwd, up, f5, down_k, up_k, enter, esc) = ctx.input(|i| {
            (
                (i.modifiers.alt && i.key_pressed(Key::ArrowLeft))
                    || i.key_pressed(Key::Backspace)
                    || i.pointer.button_pressed(PointerButton::Extra1),
                (i.modifiers.alt && i.key_pressed(Key::ArrowRight))
                    || i.pointer.button_pressed(PointerButton::Extra2),
                i.modifiers.alt && i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::F5),
                !i.modifiers.alt && i.key_pressed(Key::ArrowDown),
                !i.modifiers.alt && i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Escape),
            )
        });
        if back {
            self.actions.push(Action::Back);
        }
        if fwd {
            self.actions.push(Action::Forward);
        }
        if up {
            self.actions.push(Action::Up);
        }
        if f5 && let Some(i) = self.current {
            self.actions.push(Action::Rescan(i));
        }
        let Some(s) = self.cur() else { return };
        let Some(r) = &s.result else { return };
        let Some(node) = r.root.get(&s.nav.path) else { return };
        let n = node.children.len();
        if n == 0 {
            return;
        }
        if down_k || up_k {
            let next = match (s.nav.selected, down_k) {
                (None, _) => 0,
                (Some(i), true) => (i + 1).min(n - 1),
                (Some(i), false) => i.saturating_sub(1),
            };
            self.actions.push(Action::Select(Some(next)));
            if let Some(s) = self.cur_mut() {
                s.nav.reveal = true;
            }
        } else if enter
            && let Some(i) = s.nav.selected
            && node.children.get(i).is_some_and(|c| c.is_dir())
        {
            let mut p = s.nav.path.clone();
            p.push(i);
            self.actions.push(Action::Go(p, None));
        } else if esc {
            self.actions.push(Action::Select(None));
        }
    }

    fn screenshot(&mut self, ctx: &egui::Context) {
        if self.shot == ShotState::Off || self.shot == ShotState::Done {
            return;
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = image {
            if let Some(path) = &self.opts.shot {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let res = image::save_buffer(
                    path,
                    img.as_raw(),
                    img.size[0] as u32,
                    img.size[1] as u32,
                    image::ExtendedColorType::Rgba8,
                );
                if let Err(e) = res {
                    let _ = std::fs::write(path.with_extension("error.txt"), e.to_string());
                }
                // how long our ui code took per frame (after the first few, which load fonts)
                let t: Vec<f32> = self.frame_times.iter().skip(5).copied().collect();
                if !t.is_empty() {
                    let avg = t.iter().sum::<f32>() / t.len() as f32;
                    let max = t.iter().copied().fold(0.0, f32::max);
                    let note = format!("frames {}  ui avg {avg:.2} ms  max {max:.2} ms\n", t.len());
                    let _ = std::fs::write(path.with_extension("frames.txt"), note);
                }
            }
            self.shot = ShotState::Done;
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        let t = self.started.elapsed().as_secs_f32();
        // --nav waits for the scan, but not forever
        let ready = t >= self.opts.after && (self.nav_pending.is_none() || t >= self.opts.after + 600.0);
        if self.shot == ShotState::Waiting && ready {
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::UserData::default()));
            self.shot = ShotState::Requested;
        }
        ctx.request_repaint_after(Duration::from_millis(50));
    }
}

/// Folder names from the root along `path`.
fn names_of(root: &Node, path: &[usize]) -> Vec<String> {
    let mut out = Vec::new();
    let mut n = root;
    for &i in path {
        let Some(c) = n.children.get(i) else { break };
        out.push(c.name.to_string());
        n = c;
    }
    out
}

/// Finds a path by names (case-insensitive). A trailing file name selects the file in its folder.
fn resolve_names(root: &Node, names: &[String]) -> (Vec<usize>, Option<usize>) {
    let mut path = Vec::new();
    let mut n = root;
    for name in names {
        let Some(i) = n.children.iter().position(|c| c.name.eq_ignore_ascii_case(name)) else {
            break;
        };
        if !n.children[i].is_dir() {
            return (path, Some(i));
        }
        path.push(i);
        n = &n.children[i];
    }
    (path, None)
}

/// `MFT`, `Walk`.
fn method_name(m: &str) -> String {
    let mut c = m.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

fn join_path(base: &str, name: &str) -> String {
    if base.ends_with('\\') { format!("{base}{name}") } else { format!("{base}\\{name}") }
}

// ---------------------------------------------------------------------------------------------
// Small painting helpers

#[derive(Clone, Copy)]
enum Icon {
    Back,
    Forward,
    Up,
    PanelClose,
    Check,
}

fn paint_icon(p: &egui::Painter, c: Pos2, icon: Icon, color: Color32) {
    let s = Stroke::new(1.6, color);
    match icon {
        Icon::Back | Icon::Forward => {
            let d = if matches!(icon, Icon::Back) { -1.0 } else { 1.0 };
            p.line_segment([c + vec2(-3.0 * d, -5.0), c + vec2(2.0 * d, 0.0)], s);
            p.line_segment([c + vec2(2.0 * d, 0.0), c + vec2(-3.0 * d, 5.0)], s);
        }
        Icon::Up => {
            p.line_segment([c + vec2(0.0, -5.5), c + vec2(0.0, 5.5)], s);
            p.line_segment([c + vec2(-4.5, -1.0), c + vec2(0.0, -5.5)], s);
            p.line_segment([c + vec2(4.5, -1.0), c + vec2(0.0, -5.5)], s);
        }
        Icon::PanelClose => {
            // double chevron pointing right: "slide away"
            for dx in [-3.0, 2.0] {
                p.line_segment([c + vec2(dx - 2.0, -4.5), c + vec2(dx + 2.5, 0.0)], s);
                p.line_segment([c + vec2(dx + 2.5, 0.0), c + vec2(dx - 2.0, 4.5)], s);
            }
        }
        Icon::Check => {
            let s = Stroke::new(1.5, color);
            p.line_segment([c + vec2(-3.5, 0.2), c + vec2(-1.0, 2.8)], s);
            p.line_segment([c + vec2(-1.0, 2.8), c + vec2(4.0, -2.8)], s);
        }
    }
}

fn icon_button(ui: &mut Ui, icon: Icon, enabled: bool, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), if enabled { Sense::click() } else { Sense::hover() });
    let p = ui.painter();
    if enabled && resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(7), theme::SURFACE_HOVER);
    }
    let col = if !enabled {
        theme::alpha(TEXT_FAINT, 0.6)
    } else if resp.hovered() {
        TEXT
    } else {
        TEXT_DIM
    };
    paint_icon(p, rect.center(), icon, col);
    if enabled { resp.on_hover_text(tip) } else { resp }
}

/// The quiet "On disk | Size" switch, right-aligned in `rect`. Returns its width.
fn metric_toggle(ui: &mut Ui, rect: Rect, current: Metric, acts: &mut Vec<Action>) -> f32 {
    let ctx = ui.ctx().clone();
    let font = theme::regular(11.5);
    let opts = [(Metric::OnDisk, "On disk"), (Metric::Logical, "Size")];
    let pad = 9.0;
    let widths: Vec<f32> = opts.iter().map(|(_, t)| theme::text_width(&ctx, t, &font) + 2.0 * pad).collect();
    let total: f32 = widths.iter().sum::<f32>() + 4.0;
    let h = 20.0;
    let outer = Rect::from_min_size(pos2(rect.max.x - total, rect.center().y - h / 2.0), vec2(total, h));
    let p = ui.painter().clone();
    p.rect_filled(outer, CornerRadius::same(6), theme::alpha(SURFACE, 0.9));
    let mut x = outer.min.x + 2.0;
    for ((m, label), w) in opts.iter().zip(&widths) {
        let seg = Rect::from_min_size(pos2(x, outer.min.y + 2.0), vec2(*w, h - 4.0));
        let resp = ui.interact(seg, ui.id().with(("metric", *label)), Sense::click());
        let on = *m == current;
        if on {
            p.rect_filled(seg, CornerRadius::same(5), theme::SURFACE_HOVER);
        }
        let col = if on {
            TEXT
        } else if resp.hovered() {
            TEXT_DIM
        } else {
            TEXT_FAINT
        };
        let g = theme::line_free(&ctx, *label, font.clone(), col);
        p.galley(seg.center() - g.size() / 2.0, g, col);
        if !on && resp.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let tip = match m {
            Metric::OnDisk => "Space on disk: what deleting would free (compression, cloud files and slack counted as they really are)",
            Metric::Logical => "File size, as Explorer's \"Size\" column shows it",
        };
        if resp.on_hover_text(tip).clicked() && !on {
            acts.push(Action::SetMetric(*m));
        }
        x += w;
    }
    total
}

/// A text-only button: dim, brightening on hover.
fn link(ui: &mut Ui, text: &str, font: egui::FontId, color: Color32, hover: Color32) -> egui::Response {
    let g = theme::line_free(ui.ctx(), text.to_owned(), font, color);
    let (rect, resp) = ui.allocate_exact_size(g.size(), Sense::click());
    let col = if resp.hovered() { hover } else { color };
    ui.painter().galley_with_override_text_color(rect.min, g, col);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

fn bar(p: &egui::Painter, rect: Rect, frac: f32, fill: Color32, track: Color32) {
    let r = CornerRadius::same((rect.height() / 2.0).round() as u8);
    p.rect_filled(rect, r, track);
    let f = frac.clamp(0.0, 1.0);
    if f > 0.0 {
        let w = (rect.width() * f).max(rect.height());
        p.rect_filled(Rect::from_min_size(rect.min, vec2(w, rect.height())), r, fill);
    }
}

/// Soft fade over the last pixels of a scroll area that has more content below.
fn fade_bottom<R>(ui: &Ui, out: &egui::scroll_area::ScrollAreaOutput<R>, bg: Color32) {
    let r = out.inner_rect;
    if out.state.offset.y + r.height() >= out.content_size.y - 1.0 {
        return;
    }
    let h = 26.0f32.min(r.height() / 3.0);
    let top = r.max.y - h;
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(pos2(r.min.x, top), Color32::TRANSPARENT);
    mesh.colored_vertex(pos2(r.max.x, top), Color32::TRANSPARENT);
    mesh.colored_vertex(pos2(r.min.x, r.max.y), bg);
    mesh.colored_vertex(pos2(r.max.x, r.max.y), bg);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 2, 3);
    ui.painter().add(egui::Shape::mesh(mesh));
}

fn galley_right(p: &egui::Painter, right_center: Pos2, g: Arc<egui::Galley>, color: Color32) {
    let pos = pos2(right_center.x - g.size().x, right_center.y - g.size().y / 2.0);
    p.galley(pos, g, color);
}

fn galley_left(p: &egui::Painter, left_center: Pos2, g: Arc<egui::Galley>, color: Color32) {
    let pos = pos2(left_center.x, left_center.y - g.size().y / 2.0);
    p.galley(pos, g, color);
}

// ---------------------------------------------------------------------------------------------
// The frame

impl eframe::App for App {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        // test clicks: move there, press, release, once
        // test input waits until the current scan has a result to click on
        let settled = self.cur().is_none_or(|s| s.result.is_some() && s.job.is_none());
        if settled && self.settled_at.is_none() {
            self.settled_at = Some(Instant::now());
        }
        let since = self.settled_at.map_or(Duration::ZERO, |t| t.elapsed());
        if since > Duration::from_millis(500) && self.started.elapsed() > Duration::from_millis(800) && !self.clicked {
            for (pos, button) in [(self.opts.click, PointerButton::Primary), (self.opts.rclick, PointerButton::Secondary)] {
                if let Some(p) = pos {
                    let modifiers = egui::Modifiers::default();
                    raw.events.push(Event::PointerMoved(p));
                    raw.events.push(Event::PointerButton { pos: p, button, pressed: true, modifiers });
                    raw.events.push(Event::PointerButton { pos: p, button, pressed: false, modifiers });
                }
            }
            self.clicked = true;
        }
        if self.key_index > 0 {
            // modifiers held for the previous injected key are released one frame later
            raw.events.push(Event::ModifiersChanged(egui::Modifiers::default()));
        }
        if since > Duration::from_millis(700) && self.started.elapsed() > Duration::from_secs(1) && self.key_index < self.opts.keys.len() {
            let name = self.opts.keys[self.key_index].to_lowercase();
            self.key_index += 1;
            let (alt, key) = match name.strip_prefix("alt+") {
                Some(k) => (true, k.to_owned()),
                None => (false, name),
            };
            let key = match key.as_str() {
                "down" => Some(Key::ArrowDown),
                "up" => Some(Key::ArrowUp),
                "left" => Some(Key::ArrowLeft),
                "right" => Some(Key::ArrowRight),
                "enter" => Some(Key::Enter),
                "backspace" => Some(Key::Backspace),
                "esc" => Some(Key::Escape),
                "f5" => Some(Key::F5),
                _ => None,
            };
            if let Some(key) = key {
                let modifiers = egui::Modifiers { alt, ..Default::default() };
                raw.events.push(Event::ModifiersChanged(modifiers));
                for pressed in [true, false] {
                    raw.events.push(Event::Key { key, physical_key: None, pressed, repeat: false, modifiers });
                }
            }
        }
        if let Some(p) = self.opts.hover {
            let due = self.last_hover_inject.is_none_or(|t| t.elapsed() > Duration::from_secs(3));
            if due && self.started.elapsed() > Duration::from_millis(300) {
                raw.events.push(Event::PointerMoved(p));
                self.last_hover_inject = Some(Instant::now());
            }
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let frame_start = Instant::now();
        let ctx = ui.ctx().clone();
        self.fit_to_monitor(&ctx);
        self.poll(&ctx);
        if let Some(i) = self.current {
            self.apply_metric(i);
        }
        self.keys(&ctx);

        let width = ctx.content_rect().width();
        let side_w = if width >= 1280.0 { 248.0 } else { 220.0 };
        let hot_w = if width >= 1360.0 { 360.0 } else { 304.0 };

        egui::Panel::left("drives")
            .exact_size(side_w)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::NONE.fill(SIDE).inner_margin(Margin { left: 14, right: 14, top: 16, bottom: 14 }))
            .show(ui, |ui| self.sidebar(ui));

        let mut open = self.show_hotspots;
        let hot_rect = egui::Panel::right("hotspots")
            .exact_size(hot_w)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::NONE.fill(SIDE).inner_margin(Margin { left: 18, right: 14, top: 16, bottom: 10 }))
            .show_collapsible(ui, &mut open, |ui| self.hotspots(ui))
            .map(|r| r.response.rect);
        self.show_hotspots = open;

        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(BG).inner_margin(Margin { left: 26, right: 26, top: 14, bottom: 10 }))
            .show(ui, |ui| self.centre(ui));

        // hairlines between the panels
        let full = ctx.content_rect();
        // on the panels' own layer, so menus and tooltips stay above them
        let p = ui.painter();
        let x = full.min.x + side_w - 0.5;
        p.line_segment([pos2(x, full.min.y), pos2(x, full.max.y)], Stroke::new(1.0, LINE));
        if let Some(r) = hot_rect.filter(|r| r.width() > 1.0) {
            let x = r.min.x + 0.5;
            p.line_segment([pos2(x, full.min.y), pos2(x, full.max.y)], Stroke::new(1.0, LINE));
        }

        self.apply(&ctx);
        self.update_title(&ctx);
        if self.opts.shot.is_some() {
            self.frame_times.push(frame_start.elapsed().as_secs_f32() * 1000.0);
        }
        self.screenshot(&ctx);
    }
}

// ---------------------------------------------------------------------------------------------
// Sidebar

impl App {
    fn sidebar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        // wordmark: three little tiles and the name
        {
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
            let p = ui.painter();
            let o = pos2(rect.min.x + 2.0, rect.center().y - 8.0);
            let tiles = [
                (Rect::from_min_size(o, vec2(9.0, 16.0)), 1.0),
                (Rect::from_min_size(o + vec2(11.0, 0.0), vec2(5.0, 7.0)), 0.7),
                (Rect::from_min_size(o + vec2(11.0, 9.0), vec2(5.0, 7.0)), 0.45),
            ];
            for (r, a) in tiles {
                p.rect_filled(r, CornerRadius::same(2), theme::alpha(ACCENT, a));
            }
            let g = theme::line_free(&ctx, "diskscope", theme::semibold(15.0), TEXT);
            galley_left(p, pos2(o.x + 26.0, rect.center().y), g, TEXT);
        }
        ui.add_space(22.0);

        // everything between the wordmark and the admin note scrolls when there are many drives
        let denied = self.cur().and_then(|s| s.result.as_ref()).map_or(0, |r| r.denied_dirs);
        let show_note = !self.elevated && denied > 0;
        let list_h = ui.available_height() - if show_note { 74.0 } else { 0.0 };
        egui::ScrollArea::vertical()
            .id_salt("sidebar")
            .max_height(list_h.max(60.0))
            .auto_shrink([false, true])
            .show(ui, |ui| self.sidebar_lists(ui));

        // admin note, pinned to the bottom
        if show_note {
            let rest = ui.available_rect_before_wrap();
            let rect = Rect::from_min_max(pos2(rest.min.x, rest.max.y - 62.0), rest.max);
            let p = ui.painter().clone();
            p.rect_filled(rect, CornerRadius::same(10), theme::alpha(theme::WARN, 0.06));
            let n = format_count(denied);
            let f = if denied == 1 { "folder needs" } else { "folders need" };
            let g = theme::line(&ctx, format!("{n} {f} admin"), theme::regular(12.0), TEXT_DIM, rect.width() - 24.0);
            p.galley(rect.min + vec2(12.0, 11.0), g, TEXT_DIM);
            let lg = theme::line_free(&ctx, "Restart as admin", theme::medium(12.0), theme::WARN);
            let lr = Rect::from_min_size(rect.min + vec2(12.0, 33.0), lg.size());
            let resp = ui.interact(lr.expand(3.0), ui.id().with("admin_note"), Sense::click());
            p.galley_with_override_text_color(lr.min, lg, if resp.hovered() { TEXT } else { theme::WARN });
            if resp.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() {
                self.actions.push(Action::RestartAdmin);
            }
        }
    }

    fn sidebar_lists(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let label = theme::section_label(&ctx, "Drives", TEXT_FAINT);
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), label.size().y), Sense::hover());
        ui.painter().galley(pos2(r.min.x + 2.0, r.min.y), label, TEXT_FAINT);
        ui.add_space(8.0);

        let drives = self.drives.clone();
        for d in &drives {
            let idx = self.scans.iter().position(|s| norm(&s.root_path) == norm(&d.root));
            self.drive_card(ui, d, idx);
            ui.add_space(6.0);
        }
        if drives.is_empty() {
            ui.label(egui::RichText::new("No drives found").color(TEXT_FAINT));
        }

        // folder scans (anything that is not a drive root)
        let folders: Vec<usize> = (0..self.scans.len())
            .filter(|&i| !drives.iter().any(|d| norm(&d.root) == norm(&self.scans[i].root_path)))
            .collect();
        ui.add_space(14.0);
        let label = theme::section_label(&ctx, "Folders", TEXT_FAINT);
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), label.size().y), Sense::hover());
        ui.painter().galley(pos2(r.min.x + 2.0, r.min.y), label, TEXT_FAINT);
        ui.add_space(8.0);
        for i in folders {
            self.folder_card(ui, i);
            ui.add_space(6.0);
        }

        let edit = egui::TextEdit::singleline(&mut self.folder_input)
            .hint_text(egui::RichText::new("Scan a folder…  paste a path").color(TEXT_FAINT))
            .font(theme::regular(13.0))
            .margin(Margin::symmetric(10, 7))
            .desired_width(ui.available_width());
        let resp = ui.add(edit);
        if resp.changed() {
            self.input_error = None;
        }
        if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            let p = std::mem::take(&mut self.folder_input);
            self.actions.push(Action::ScanPath(p));
        }
        if let Some(e) = &self.input_error {
            ui.add_space(2.0);
            let g = theme::line(&ctx, e.clone(), theme::regular(12.0), theme::WARN, ui.available_width() - 4.0);
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), g.size().y), Sense::hover());
            ui.painter().galley(pos2(r.min.x + 2.0, r.min.y), g, theme::WARN);
        }

        ui.add_space(4.0);
    }

    fn scan_status(&self, idx: Option<usize>, used: Option<u64>) -> (String, Color32, bool) {
        let Some(s) = idx.map(|i| &self.scans[i]) else {
            return ("Not scanned".into(), TEXT_FAINT, false);
        };
        if s.job.is_some() {
            let bytes = s.snap[2];
            let txt = match used {
                Some(u) if u > 0 => format!("Scanning {:.0}%", (bytes as f64 * 100.0 / u as f64).min(99.0)),
                _ => "Scanning…".into(),
            };
            return (txt, ACCENT, false);
        }
        if let Some(r) = &s.result {
            return (format!("{} · {}", method_name(r.method), theme::format_duration(r.elapsed)), TEXT_FAINT, true);
        }
        ("Not scanned".into(), TEXT_FAINT, false)
    }

    fn drive_card(&mut self, ui: &mut Ui, d: &DriveInfo, idx: Option<usize>) {
        let ctx = ui.ctx().clone();
        let selected = idx.is_some() && idx == self.current;
        let scanned = idx.is_some_and(|i| self.scans[i].result.is_some() && self.scans[i].job.is_none());
        let footer = selected && scanned;
        let h = if footer { 108.0 } else { 74.0 };
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
        let p = ui.painter().clone();
        let fill = if selected {
            SURFACE
        } else if resp.hovered() {
            theme::alpha(SURFACE, 0.6)
        } else {
            Color32::TRANSPARENT
        };
        p.rect_filled(rect, CornerRadius::same(10), fill);
        if selected {
            p.rect_stroke(rect, CornerRadius::same(10), Stroke::new(1.0, theme::alpha(ACCENT, 0.28)), StrokeKind::Inside);
        }
        let inner = rect.shrink2(vec2(12.0, 11.0));

        // row 1: letter, label, status
        let y1 = inner.min.y + 9.0;
        let letter = theme::line_free(&ctx, format!("{}:", d.letter), theme::semibold(14.0), TEXT);
        let lw = letter.size().x;
        galley_left(&p, pos2(inner.min.x, y1), letter, TEXT);
        let (status, scol, done) = self.scan_status(idx, Some(d.used()));
        let sg = theme::line_free(&ctx, status, theme::regular(11.5), scol);
        let sw = sg.size().x + if done { 14.0 } else { 0.0 };
        galley_right(&p, pos2(inner.max.x, y1), sg, scol);
        if done {
            paint_icon(&p, pos2(inner.max.x - sw + 4.0, y1), Icon::Check, theme::alpha(ACCENT, 0.9));
        }
        let label = if d.label.is_empty() {
            match d.kind {
                DriveKind::Fixed => "Local disk",
                DriveKind::Removable => "Removable",
                DriveKind::Network => "Network",
                DriveKind::Other => "Drive",
            }
            .to_owned()
        } else {
            d.label.clone()
        };
        let lab_w = inner.width() - lw - 8.0 - sw - 10.0;
        let lg = theme::line(&ctx, label, theme::regular(13.0), TEXT_DIM, lab_w);
        galley_left(&p, pos2(inner.min.x + lw + 7.0, y1), lg, TEXT_DIM);

        // row 2: usage bar
        let frac = if d.total > 0 { d.used() as f32 / d.total as f32 } else { 0.0 };
        let bar_col = if frac > 0.92 { theme::WARN } else { mix(TEXT_DIM, ACCENT, 0.35) };
        let br = Rect::from_min_size(pos2(inner.min.x, inner.min.y + 24.0), vec2(inner.width(), 4.0));
        bar(&p, br, frac, theme::alpha(bar_col, 0.85), theme::TRACK);

        // row 3: used of total, free
        let y3 = inner.min.y + 43.0;
        let ug = theme::line_free(&ctx, format!("{} of {}", format_size(d.used()), format_size(d.total)), theme::regular(11.5), TEXT_FAINT);
        let fg = theme::line_free(&ctx, format!("{} free", format_size(d.free)), theme::regular(11.5), TEXT_FAINT);
        if ug.size().x + fg.size().x + 12.0 <= inner.width() {
            galley_right(&p, pos2(inner.max.x, y3), fg, TEXT_FAINT);
        }
        galley_left(&p, pos2(inner.min.x, y3), ug, TEXT_FAINT);

        let mut footer_hit = false;
        if footer && let Some(i) = idx {
            let y = inner.min.y + 60.0;
            p.line_segment([pos2(inner.min.x, y), pos2(inner.max.x, y)], Stroke::new(1.0, LINE));
            let row = Rect::from_min_max(pos2(inner.min.x, y + 6.0), pos2(inner.max.x, inner.max.y + 4.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(row).layout(egui::Layout::left_to_right(egui::Align::Center)));
            let r = link(&mut child, "Rescan", theme::medium(12.0), TEXT_DIM, TEXT);
            if r.clicked() {
                self.actions.push(Action::Rescan(i));
                footer_hit = true;
            }
            if !self.elevated {
                let avail = child.available_width();
                let txt = theme::line(&ctx, "Faster as admin", theme::regular(11.5), TEXT_FAINT, avail - 6.0);
                let (rr, rresp) = child.allocate_exact_size(vec2(avail, txt.size().y), Sense::click());
                let col = if rresp.hovered() { ACCENT } else { TEXT_FAINT };
                galley_right(&child.painter().clone(), pos2(rr.max.x, rr.center().y), txt, col);
                if rresp.hovered() {
                    ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                let rresp = rresp.on_hover_text("Restart as administrator: diskscope then reads the NTFS master file table directly, so a whole drive scans in a few seconds (cold) and folders that need admin are counted too.");
                if rresp.clicked() {
                    self.actions.push(Action::RestartAdmin);
                    footer_hit = true;
                }
            }
        }

        if resp.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() && !footer_hit {
            match idx {
                Some(i) => self.actions.push(Action::SelectScan(i)),
                None => self.actions.push(Action::ScanPath(d.root.clone())),
            }
        }
    }

    fn folder_card(&mut self, ui: &mut Ui, i: usize) {
        let ctx = ui.ctx().clone();
        let selected = self.current == Some(i);
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::click());
        let p = ui.painter().clone();
        let fill = if selected {
            SURFACE
        } else if resp.hovered() {
            theme::alpha(SURFACE, 0.6)
        } else {
            Color32::TRANSPARENT
        };
        p.rect_filled(rect, CornerRadius::same(10), fill);
        if selected {
            p.rect_stroke(rect, CornerRadius::same(10), Stroke::new(1.0, theme::alpha(ACCENT, 0.28)), StrokeKind::Inside);
        }
        let inner = rect.shrink2(vec2(12.0, 8.0));
        let s = &self.scans[i];
        let name = s
            .result
            .as_ref()
            .map(|r| r.root.name.to_string())
            .unwrap_or_else(|| s.root_path.rsplit(['\\', '/']).find(|x| !x.is_empty()).unwrap_or(&s.root_path).to_owned());
        let (status, scol, done) = self.scan_status(Some(i), None);
        let metric = self.metric;
        let size = s.result.as_ref().map(|r| format_size(if r.metric == metric { r.root.size } else { r.root.other }));
        let sg = theme::line_free(&ctx, size.unwrap_or(status), theme::regular(11.5), if done { TEXT_DIM } else { scol });
        let sw = sg.size().x;
        let y1 = inner.min.y + 8.0;
        galley_right(&p, pos2(inner.max.x, y1), sg, scol);
        let ng = theme::line(&ctx, name, theme::medium(13.0), TEXT, inner.width() - sw - 10.0);
        galley_left(&p, pos2(inner.min.x, y1), ng, TEXT);
        let path = theme::middle_ellipsis(&ctx, &s.root_path, &theme::regular(11.5), inner.width());
        let pg = theme::line_free(&ctx, path, theme::regular(11.5), TEXT_FAINT);
        galley_left(&p, pos2(inner.min.x, inner.min.y + 25.0), pg, TEXT_FAINT);
        if resp.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            self.actions.push(Action::SelectScan(i));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Centre

impl App {
    fn centre(&mut self, ui: &mut Ui) {
        let Some(ci) = self.current else {
            self.empty_state(ui);
            return;
        };
        let scanning = self.scans[ci].scanning();
        if scanning {
            let started = self.scans[ci].job.as_ref().map(|j| j.progress.started);
            if started.is_some_and(|t| t.elapsed() < PROGRESS_DELAY) {
                // fast scans never flash the progress view; a rescan keeps the old view meanwhile
                ui.ctx().request_repaint_after(Duration::from_millis(50));
                if self.scans[ci].result.is_some() {
                    self.browse(ui, ci);
                }
                return;
            }
            self.progress_view(ui, ci);
            return;
        }
        if self.scans[ci].result.is_none() {
            self.empty_state(ui);
            return;
        }
        self.browse(ui, ci);
    }

    fn empty_state(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let rect = ui.available_rect_before_wrap();
        let p = ui.painter();
        let c = rect.center() - vec2(0.0, 30.0);
        let t = theme::line_free(&ctx, "Pick a drive to see where the space went", theme::display(20.0), TEXT);
        p.galley(c - vec2(t.size().x / 2.0, t.size().y), t, TEXT);
        let s = theme::line(&ctx, "Choose a drive on the left, or paste a folder path to scan just that folder.", theme::regular(13.0), TEXT_DIM, rect.width() - 40.0);
        p.galley(c + vec2(-s.size().x / 2.0, 10.0), s, TEXT_DIM);
    }

    fn progress_view(&mut self, ui: &mut Ui, ci: usize) {
        let ctx = ui.ctx().clone();
        let s = &self.scans[ci];
        let Some(job) = &s.job else { return };
        let pr = &job.progress;
        let [files, dirs, bytes, denied] = s.snap;
        let current = pr.current.try_lock().map(|c| c.clone()).unwrap_or_default();
        let elapsed = pr.started.elapsed();
        let cancelled = pr.cancelled();
        let used = self.drives.iter().find(|d| norm(&d.root) == norm(&s.root_path)).map(|d| d.used());

        let area = ui.available_rect_before_wrap();
        let w = 480.0f32.min(area.width() - 40.0);
        let h = 244.0;
        let card = Rect::from_center_size(area.center() - vec2(0.0, 24.0), vec2(w, h));
        let p = ui.painter().clone();
        p.rect_filled(card, CornerRadius::same(14), theme::alpha(SURFACE, 0.7));
        p.rect_stroke(card, CornerRadius::same(14), Stroke::new(1.0, LINE), StrokeKind::Inside);
        let inner = card.shrink(26.0);

        let title = if cancelled { "Stopping…".to_owned() } else { format!("Scanning {}", job.root_path) };
        let mut title_w = inner.width();
        if let Some(u) = used.filter(|&u| u > 0) {
            let pct = format!("{:.0}%", (bytes as f64 * 100.0 / u as f64).min(99.0));
            let pg = theme::line_free(&ctx, pct, theme::display(19.0), TEXT_DIM);
            title_w -= pg.size().x + 16.0;
            p.galley(pos2(inner.max.x - pg.size().x, inner.min.y), pg, TEXT_DIM);
        }
        let tg = theme::line(&ctx, title, theme::display(19.0), TEXT, title_w);
        p.galley(inner.min, tg, TEXT);
        let cur = theme::middle_ellipsis(&ctx, &current, &theme::regular(12.0), inner.width());
        let cg = theme::line_free(&ctx, if cur.is_empty() { " ".into() } else { cur }, theme::regular(12.0), TEXT_FAINT);
        p.galley(inner.min + vec2(0.0, 32.0), cg, TEXT_FAINT);

        // progress bar: real fraction for drives, a calm sweep otherwise
        let br = Rect::from_min_size(inner.min + vec2(0.0, 62.0), vec2(inner.width(), 4.0));
        match used {
            Some(u) if u > 0 => {
                let f = (bytes as f64 / u as f64).min(0.995) as f32;
                bar(&p, br, f, ACCENT, theme::TRACK);
            }
            _ => {
                p.rect_filled(br, CornerRadius::same(2), theme::TRACK);
                let t = ctx.input(|i| i.time) as f32;
                let seg = br.width() * 0.28;
                let x = br.min.x + ((t * 0.45).fract() * (br.width() + seg)) - seg;
                let r = Rect::from_min_max(pos2(x.max(br.min.x), br.min.y), pos2((x + seg).min(br.max.x), br.max.y));
                if r.width() > 0.0 {
                    p.rect_filled(r, CornerRadius::same(2), ACCENT);
                }
            }
        }

        // three numbers
        let col_w = inner.width() / 3.0;
        let stats = [
            (format_count(files), "files".to_owned()),
            (format_count(dirs), "folders".to_owned()),
            (format_size(bytes), used.filter(|&u| u > 0).map_or("found".to_owned(), |u| format!("of {}", format_size(u)))),
        ];
        for (k, (v, l)) in stats.iter().enumerate() {
            let x = inner.min.x + col_w * k as f32;
            let vg = theme::line(&ctx, v.clone(), theme::display(22.0), TEXT, col_w - 8.0);
            p.galley(pos2(x, inner.min.y + 86.0), vg, TEXT);
            let lg = theme::line_free(&ctx, l.clone(), theme::regular(12.0), TEXT_FAINT);
            p.galley(pos2(x, inner.min.y + 116.0), lg, TEXT_FAINT);
        }

        let mut foot = format!("Elapsed {}", theme::format_duration(elapsed));
        if denied > 0 {
            foot.push_str(&format!("  ·  {} without access", format_count(denied)));
        }
        let fg = theme::line(&ctx, foot, theme::regular(12.0), TEXT_DIM, inner.width() - 110.0);
        galley_left(&p, pos2(inner.min.x, inner.max.y - 14.0), fg, TEXT_DIM);

        let btn = Rect::from_min_size(pos2(inner.max.x - 92.0, inner.max.y - 30.0), vec2(92.0, 30.0));
        let resp = ui.interact(btn, ui.id().with("cancel"), Sense::click());
        let fill = if resp.hovered() { theme::SURFACE_HOVER } else { Color32::TRANSPARENT };
        p.rect(btn, CornerRadius::same(8), fill, Stroke::new(1.0, mix(LINE, TEXT_FAINT, 0.4)), StrokeKind::Inside);
        let bg = theme::line_free(&ctx, "Cancel", theme::medium(12.5), if resp.hovered() { TEXT } else { TEXT_DIM });
        p.galley(btn.center() - bg.size() / 2.0, bg, TEXT_DIM);
        if resp.clicked() && !cancelled {
            self.actions.push(Action::Cancel(ci));
        }

        // a quiet tip below the card
        if !self.elevated {
            let font = theme::regular(12.0);
            let a = theme::line_free(&ctx, "Scans are faster and complete as admin  ·  ", font.clone(), TEXT_FAINT);
            let b = theme::line_free(&ctx, "Restart as admin", theme::medium(12.0), ACCENT);
            let w = a.size().x + b.size().x;
            if w < area.width() - 20.0 {
                let y = card.max.y + 22.0;
                let x0 = card.center().x - w / 2.0;
                let br = Rect::from_min_size(pos2(x0 + a.size().x, y - b.size().y / 2.0), b.size());
                p.galley(pos2(x0, y - a.size().y / 2.0), a, TEXT_FAINT);
                let lr = ui.interact(br.expand(3.0), ui.id().with("tip_admin"), Sense::click());
                p.galley_with_override_text_color(br.min, b, if lr.hovered() { TEXT } else { ACCENT });
                if lr.hovered() {
                    ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if lr.clicked() {
                    self.actions.push(Action::RestartAdmin);
                }
            }
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn browse(&mut self, ui: &mut Ui, ci: usize) {
        let ctx = ui.ctx().clone();
        let mut acts: Vec<Action> = Vec::new();
        let show_hotspots = self.show_hotspots;
        let s = &self.scans[ci];
        let Some(r) = &s.result else { return };
        let mut path = s.nav.path.clone();
        if r.root.get(&path).is_none() {
            path.clear();
        }
        let Some(node) = r.root.get(&path) else { return };
        let at_root = path.is_empty();
        // unaccounted space is a disk measure: shown only with "On disk"
        let unacc = if at_root && r.metric == Metric::OnDisk { r.unaccounted().filter(|&u| u > 0) } else { None };
        let metric = r.metric;
        let folder_path = crate::model::path_string(r, &path);
        let colors = theme::child_colors(node);
        let total = node.size + unacc.unwrap_or(0);

        // --- top bar: back / forward / up, breadcrumb, hotspots toggle
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if icon_button(ui, Icon::Back, !s.nav.back.is_empty(), "Back (Alt+Left)").clicked() {
                acts.push(Action::Back);
            }
            if icon_button(ui, Icon::Forward, !s.nav.forward.is_empty(), "Forward (Alt+Right)").clicked() {
                acts.push(Action::Forward);
            }
            if icon_button(ui, Icon::Up, !at_root, "Up (Alt+Up)").clicked() {
                acts.push(Action::Up);
            }
            ui.add_space(12.0);
            let toggle_w = if show_hotspots { 0.0 } else { 92.0 };
            let avail = ui.available_width() - toggle_w;
            self.breadcrumb(ui, &r.root, &path, avail, &mut acts);
            if !show_hotspots {
                let rest = ui.available_width();
                ui.add_space((rest - 86.0).max(0.0));
                let (br, resp) = ui.allocate_exact_size(vec2(86.0, 28.0), Sense::click());
                let fill = if resp.hovered() { theme::SURFACE_HOVER } else { SURFACE };
                ui.painter().rect_filled(br, CornerRadius::same(7), fill);
                let g = theme::line_free(&ctx, "Hotspots", theme::medium(12.5), if resp.hovered() { TEXT } else { TEXT_DIM });
                ui.painter().galley(br.center() - g.size() / 2.0, g, TEXT_DIM);
                if resp.clicked() {
                    acts.push(Action::ShowHotspots(true));
                }
            }
        });
        ui.add_space(16.0);

        // --- header: name and size, then a quiet line of facts
        {
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::hover());
            let p = ui.painter();
            let sg = theme::line_free(&ctx, format_size(node.size), theme::display(26.0), TEXT);
            let sw = sg.size().x;
            let size_rect = Rect::from_min_max(pos2(rect.max.x - sw, rect.min.y), rect.max);
            galley_right(p, pos2(rect.max.x, rect.center().y), sg, TEXT);
            ui.interact(size_rect, ui.id().with("header_size"), Sense::hover())
                .on_hover_text(theme::both_sizes(node.size, node.other, metric));
            let ng = theme::line(&ctx, node.name.to_string(), theme::display(26.0), TEXT, rect.width() - sw - 30.0);
            galley_left(p, pos2(rect.min.x, rect.center().y), ng, TEXT);
        }
        ui.add_space(2.0);
        {
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
            let mut facts: Vec<String> = Vec::new();
            if at_root {
                if let Some((t, f)) = r.volume {
                    facts.push(format!("{} used of {}", format_size(t.saturating_sub(f)), format_size(t)));
                }
            } else if let Some((t, f)) = r.volume.filter(|_| metric == Metric::OnDisk) {
                let used = t.saturating_sub(f);
                facts.push(format!("{} of the used space on {}", theme::percent(node.size, used), r.root.name));
            } else {
                facts.push(format!("{} of {}", theme::percent(node.size, r.root.size), r.root.name));
            }
            facts.push(format!("{} files", format_count(node.files)));
            facts.push(format!("{} folders", format_count(node.dirs)));
            if node.denied {
                facts.push("no access".into());
            }
            let mut right = String::new();
            if let Some(t) = s.done_at {
                right = format!(
                    "Scanned {}  ·  {} · {}",
                    theme::format_ago(t.elapsed()),
                    method_name(r.method),
                    theme::format_duration(r.elapsed)
                );
            }
            // the measure toggle sits at the right end of the line, always
            let toggle_w = metric_toggle(ui, rect, metric, &mut acts);
            let rect = Rect::from_min_max(rect.min, pos2(rect.max.x - toggle_w - 14.0, rect.max.y));
            let p = ui.painter();
            // whole facts only: drop the scan note first, then trailing facts, never cut a number
            let font = theme::regular(12.5);
            let facts_w = |n: usize| theme::text_width(&ctx, &facts[..n].join("  ·  "), &font);
            let rg = theme::line_free(&ctx, right, theme::regular(12.0), TEXT_FAINT);
            let rw = rg.size().x;
            let show_right = facts_w(facts.len()) + rw + 24.0 <= rect.width();
            let room = if show_right { rect.width() - rw - 24.0 } else { rect.width() };
            let mut n = facts.len();
            while n > 1 && facts_w(n) > room {
                n -= 1;
            }
            let lg = theme::line(&ctx, facts[..n].join("  ·  "), font.clone(), TEXT_DIM, room);
            galley_left(p, pos2(rect.min.x, rect.center().y), lg, TEXT_DIM);
            if show_right {
                galley_right(p, pos2(rect.max.x, rect.center().y), rg, TEXT_FAINT);
            }
        }
        ui.add_space(14.0);

        // --- treemap
        let avail = ui.available_rect_before_wrap();
        let tm_h = (avail.height() * 0.47).clamp(160.0, 560.0);
        let tm_rect = Rect::from_min_size(avail.min, vec2(avail.width(), tm_h));
        ui.allocate_rect(tm_rect, Sense::hover());
        let out = treemap::show(
            ui,
            tm_rect,
            &treemap::Input {
                node,
                colors: &colors,
                unaccounted: unacc,
                selected: s.nav.selected,
                folder_path: &folder_path,
                metric,
            },
        );
        if let Some(t) = out.clicked {
            match t {
                Target::Child(i) => {
                    let mut p = path.clone();
                    if node.children[i].is_dir() {
                        p.push(i);
                        acts.push(Action::Go(p, None));
                    } else {
                        acts.push(Action::Select(Some(i)));
                    }
                }
                Target::Grandchild(i, rel) => {
                    let mut p = path.clone();
                    p.push(i);
                    p.extend_from_slice(&rel);
                    // a folder opens; a file opens its folder with the file selected
                    if r.root.get(&p).is_some_and(|n| n.is_dir()) {
                        acts.push(Action::Go(p, None));
                    } else {
                        let last = p.pop();
                        acts.push(Action::Go(p, last));
                    }
                }
                Target::Unaccounted | Target::More => {}
            }
        }
        // right-click on a tile: same menu as the list
        // (read the response before taking the data lock: Response methods lock the context too)
        let right_clicked = out.response.secondary_clicked();
        let menu_target = ctx.data_mut(|d| {
            let id = egui::Id::new("tm_menu_target");
            if right_clicked {
                d.insert_temp(id, out.hovered.clone());
            }
            d.get_temp::<Option<Target>>(id).flatten()
        });
        out.response.context_menu(|ui| {
            let p = match &menu_target {
                Some(Target::Child(i)) => join_path(&folder_path, &node.children[*i].name),
                Some(Target::Grandchild(i, rel)) => {
                    let mut full = path.clone();
                    full.push(*i);
                    full.extend_from_slice(rel);
                    crate::model::path_string(r, &full)
                }
                _ => folder_path.clone(),
            };
            context_menu_items(ui, &p, &mut acts);
        });
        ui.add_space(18.0);

        // --- list
        self.list(ui, ci, node, &path, &colors, unacc, total, &folder_path, &mut acts);
        self.actions.extend(acts);
    }

    fn breadcrumb(&self, ui: &mut Ui, root: &Node, path: &[usize], avail: f32, acts: &mut Vec<Action>) {
        let ctx = ui.ctx().clone();
        let font = theme::medium(13.0);
        let sep_w = 22.0;
        // nodes along the path (a pointer walk; cheap even thousands of levels deep)
        let mut nodes: Vec<&Node> = Vec::with_capacity(path.len() + 1);
        nodes.push(root);
        for &i in path {
            let Some(c) = nodes.last().and_then(|n| n.children.get(i)) else { break };
            nodes.push(c);
        }
        let n = nodes.len();
        // keep the first crumb and as many of the last ones as fit; elide the middle.
        // Only the crumbs that can be shown are measured.
        let width_of = |k: usize| theme::text_width(&ctx, &nodes[k].name, &font).min(220.0);
        let ell_w = theme::text_width(&ctx, "…", &font) + sep_w;
        let mut used = width_of(0);
        let mut keep_from = n; // first index of the kept tail
        while keep_from > 1 {
            let w = width_of(keep_from - 1) + sep_w;
            let more_hidden = keep_from - 1 > 1;
            let need = used + w + if more_hidden { ell_w } else { 0.0 };
            if keep_from < n && need > avail - 8.0 {
                break;
            }
            used += w;
            keep_from -= 1;
        }
        let names: Vec<String> = (0..n)
            .map(|k| if k == 0 || k >= keep_from { nodes[k].name.to_string() } else { String::new() })
            .collect();
        let widths: Vec<f32> = (0..n).map(|k| if k == 0 || k >= keep_from { width_of(k) } else { 0.0 }).collect();
        let (rect, _) = ui.allocate_exact_size(vec2(avail.max(0.0), 28.0), Sense::hover());
        let mut x = rect.min.x;
        let cy = rect.center().y;
        let mut draw = |ui: &mut Ui, x: &mut f32, k: usize, w: f32, last: bool| {
            let col = if last { TEXT } else { TEXT_DIM };
            let g = theme::line(&ctx, names[k].clone(), font.clone(), col, w.max(20.0));
            let r = Rect::from_min_size(pos2(*x - 4.0, cy - 12.0), vec2(g.size().x + 8.0, 24.0));
            let resp = ui.interact(r, ui.id().with(("crumb", k)), if last { Sense::hover() } else { Sense::click() });
            if !last && resp.hovered() {
                ui.painter().rect_filled(r, CornerRadius::same(6), theme::SURFACE_HOVER);
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let gw = g.size().x;
            galley_left(ui.painter(), pos2(*x, cy), g, if !last && resp.hovered() { TEXT } else { col });
            if resp.clicked() {
                acts.push(Action::Go(path[..k].to_vec(), path.get(k).copied()));
            }
            *x += gw;
        };
        let sep = |ui: &mut Ui, x: &mut f32| {
            let g = theme::line_free(&ctx, "›", theme::regular(14.0), TEXT_FAINT);
            let gw = g.size().x;
            galley_left(ui.painter(), pos2(*x + (sep_w - gw) / 2.0, cy - 1.0), g, TEXT_FAINT);
            *x += sep_w;
        };
        draw(ui, &mut x, 0, widths[0], n == 1);
        if keep_from > 1 {
            sep(ui, &mut x);
            let g = theme::line_free(&ctx, "…", font.clone(), TEXT_FAINT);
            let gw = g.size().x;
            galley_left(ui.painter(), pos2(x, cy), g, TEXT_FAINT);
            x += gw;
        }
        for (k, &kw) in widths.iter().enumerate().skip(keep_from) {
            sep(ui, &mut x);
            let last = k == n - 1;
            let w = if last { rect.max.x - x } else { kw };
            draw(ui, &mut x, k, w, last);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn list(
        &self,
        ui: &mut Ui,
        ci: usize,
        node: &Node,
        path: &[usize],
        colors: &[Color32],
        unacc: Option<u64>,
        total: u64,
        folder_path: &str,
        acts: &mut Vec<Action>,
    ) {
        let ctx = ui.ctx().clone();
        let s = &self.scans[ci];
        let metric = s.result.as_ref().map_or(Metric::OnDisk, |r| r.metric);
        let width = ui.available_width();
        // columns, right to left; drop some when narrow
        let show_items = width >= 620.0;
        let show_bar = width >= 470.0;
        let items_w = if show_items { 108.0 } else { 0.0 };
        let pct_w = 52.0;
        let size_w = 78.0;
        let bar_w = if show_bar { (width * 0.14).clamp(70.0, 140.0) } else { 0.0 };

        // where the Unaccounted row sits among the children (sorted by size)
        let unacc_pos = unacc.map(|u| node.children.iter().position(|c| c.size <= u).unwrap_or(node.children.len()));
        let rows = node.children.len() + usize::from(unacc.is_some());
        let largest = node.children.first().map_or(0, |c| c.size).max(unacc.unwrap_or(0)).max(1);

        // header
        {
            let (rect, _) = ui.allocate_exact_size(vec2(width, 18.0), Sense::hover());
            let p = ui.painter();
            let f = |t: &str| theme::section_label(&ctx, t, TEXT_FAINT);
            let mut x = rect.max.x - 12.0;
            if show_items {
                galley_right(p, pos2(x, rect.center().y), f("Items"), TEXT_FAINT);
                x -= items_w;
            }
            galley_right(p, pos2(x, rect.center().y), f("Share"), TEXT_FAINT);
            x -= pct_w;
            galley_right(p, pos2(x, rect.center().y), f("Size"), TEXT_FAINT);
            let label = if !node.children.is_empty() {
                "Name"
            } else if node.denied {
                "No access"
            } else {
                "Empty folder"
            };
            galley_left(p, pos2(rect.min.x + 12.0, rect.center().y), f(label), TEXT_FAINT);
        }
        ui.add_space(4.0);
        let line_y = ui.cursor().min.y;
        ui.painter().line_segment([pos2(ui.cursor().min.x, line_y), pos2(ui.cursor().min.x + width, line_y)], Stroke::new(1.0, LINE));
        ui.add_space(4.0);

        let spacing = 2.0;
        let mut area = egui::ScrollArea::vertical()
            .id_salt(("list", ci, path.to_vec()))
            .auto_shrink([false, false]);
        if s.nav.reveal
            && let Some(sel) = s.nav.selected
        {
            let row = match unacc_pos {
                Some(up) if sel >= up => sel + 1,
                _ => sel,
            };
            let h = ui.available_height();
            let off = (row as f32 * (ROW_H + spacing) - h / 2.0 + ROW_H).max(0.0);
            area = area.vertical_scroll_offset(off);
        }
        let out = area.show_rows(ui, ROW_H, rows, |ui, range| {
            ui.spacing_mut().item_spacing.y = spacing;
            for row in range {
                let item = match unacc_pos {
                    Some(up) if row == up => None,
                    Some(up) if row > up => Some(row - 1),
                    _ => Some(row),
                };
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
                let p = ui.painter().clone();
                let cy = rect.center().y;
                let Some(i) = item else {
                    // Unaccounted row: informational, not clickable
                    let u = unacc.unwrap_or(0);
                    let dot = pos2(rect.min.x + 16.0, cy);
                    p.circle_stroke(dot, 3.5, Stroke::new(1.2, TEXT_FAINT));
                    let mut x = rect.max.x - 12.0;
                    if show_items {
                        x -= items_w;
                    }
                    galley_right(&p, pos2(x, cy), theme::line_free(&ctx, theme::percent(u, total), theme::regular(12.5), TEXT_FAINT), TEXT_FAINT);
                    x -= pct_w;
                    galley_right(&p, pos2(x, cy), theme::line_free(&ctx, format_size(u), theme::regular(13.0), TEXT_DIM), TEXT_DIM);
                    x -= size_w;
                    if show_bar {
                        let br = Rect::from_center_size(pos2(x - bar_w / 2.0, cy), vec2(bar_w - 16.0, 3.0));
                        bar(&p, br, u as f32 / largest as f32, theme::alpha(TEXT_FAINT, 0.6), theme::alpha(theme::TRACK, 0.6));
                        x -= bar_w;
                    }
                    let ng = theme::line(&ctx, "Unaccounted  (system files, no access)", theme::regular(13.5), TEXT_FAINT, x - rect.min.x - 36.0);
                    galley_left(&p, pos2(rect.min.x + 32.0, cy), ng, TEXT_FAINT);
                    resp.on_hover_text("Used space on the drive the scan could not see: system files, folders without access, hard-link differences.");
                    continue;
                };
                let c = &node.children[i];
                let selected = s.nav.selected == Some(i);
                if selected {
                    p.rect_filled(rect, CornerRadius::same(8), theme::alpha(ACCENT, 0.13));
                } else if resp.hovered() {
                    p.rect_filled(rect, CornerRadius::same(8), theme::SURFACE_HOVER);
                }
                let col = colors.get(i).copied().unwrap_or(theme::FILE);
                let dot = pos2(rect.min.x + 16.0, cy);
                match c.kind {
                    Kind::Dir => {
                        p.circle_filled(dot, 4.0, col);
                    }
                    Kind::File => {
                        p.rect_filled(Rect::from_center_size(dot, vec2(7.0, 7.0)), CornerRadius::same(2), col);
                    }
                    Kind::Rest => {
                        p.circle_stroke(dot, 3.5, Stroke::new(1.2, col));
                    }
                }
                let mut x = rect.max.x - 12.0;
                if show_items {
                    let it = match c.kind {
                        Kind::Dir if c.denied => "no access".to_owned(),
                        Kind::Dir => {
                            let n = c.files + c.dirs;
                            format!("{} {}", format_count(n), if n == 1 { "item" } else { "items" })
                        }
                        Kind::File => String::new(),
                        Kind::Rest => format!("{} files", format_count(c.files)),
                    };
                    galley_right(&p, pos2(x, cy), theme::line(&ctx, it, theme::regular(12.5), TEXT_FAINT, items_w - 12.0), TEXT_FAINT);
                    x -= items_w;
                }
                galley_right(&p, pos2(x, cy), theme::line_free(&ctx, theme::percent(c.size, total), theme::regular(12.5), TEXT_DIM), TEXT_DIM);
                x -= pct_w;
                galley_right(&p, pos2(x, cy), theme::line_free(&ctx, format_size(c.size), theme::medium(13.0), TEXT), TEXT);
                x -= size_w;
                if show_bar {
                    let br = Rect::from_center_size(pos2(x - bar_w / 2.0, cy), vec2(bar_w - 16.0, 3.0));
                    bar(&p, br, c.size as f32 / largest as f32, theme::alpha(col, 0.9), theme::TRACK);
                    x -= bar_w;
                }

                // name, then the focus hint
                let name_x = rect.min.x + 32.0;
                let name_max = x - name_x - 12.0;
                let chain = if c.is_dir() { focus_chain(c) } else { Vec::new() };
                let name_col = if c.kind == Kind::Rest { TEXT_DIM } else { TEXT };
                // the hint names where the space really is; the target and its size always show
                // (names: up to three in full, else the first and the last, for "a › … › z")
                let mut hint: Option<(Vec<String>, u64)> = None;
                if !chain.is_empty() {
                    let mut names = Vec::new();
                    let mut n = c;
                    for (step, &k) in chain.iter().enumerate() {
                        n = &n.children[k];
                        if chain.len() <= 3 || step == 0 || step + 1 == chain.len() {
                            names.push(n.name.to_string());
                        }
                    }
                    // only worth pointing at when the target is a real share of this folder
                    if n.size as f64 >= 0.01 * total as f64 {
                        hint = Some((names, n.size));
                    }
                }
                let name_font = theme::regular(13.5);
                let hint_font = theme::regular(12.5);
                let name_full_w = theme::text_width(&ctx, &c.name, &name_font);
                let room = name_max - 14.0;
                let mut name_w = name_max;
                let mut hint_text: Option<(String, f32)> = None;
                if let Some((names, size)) = &hint {
                    let last = names.last().cloned().unwrap_or_default();
                    let mut variants = Vec::new();
                    if chain.len() <= 3 {
                        variants.push(names.join(" › "));
                    }
                    if chain.len() > 2 {
                        variants.push(format!("{} › … › {last}", names[0]));
                    }
                    if chain.len() > 1 {
                        variants.push(format!("… › {last}"));
                    }
                    let suffix = format!(" · {}", format_size(*size));
                    let fits = |v: &str| theme::text_width(&ctx, &format!("→ {v}{suffix}"), &hint_font);
                    let name_min = name_full_w.min(room * 0.45);
                    if let Some(v) = variants.iter().find(|v| fits(v) <= room - name_full_w) {
                        name_w = name_full_w;
                        hint_text = Some((format!("→ {v}{suffix}"), fits(v)));
                    } else if let Some(v) = variants.iter().find(|v| fits(v) <= room - name_min) {
                        let hw = fits(v);
                        name_w = room - hw;
                        hint_text = Some((format!("→ {v}{suffix}"), hw));
                    } else {
                        // cut the target's name itself, keep the arrow and the size
                        let fixed = theme::text_width(&ctx, &format!("→ {suffix}"), &hint_font);
                        let avail = room - name_min - fixed;
                        if avail >= 40.0 {
                            let cut = theme::truncate_end(&ctx, &last, &hint_font, avail);
                            let txt = format!("→ {cut}{suffix}");
                            let w = theme::text_width(&ctx, &txt, &hint_font);
                            name_w = name_min;
                            hint_text = Some((txt, w));
                        } else {
                            name_w = name_max;
                        }
                    }
                }
                let ng = theme::line(&ctx, c.name.to_string(), name_font, name_col, name_w + 0.5);
                let ngw = ng.size().x;
                let name_cut = ng.elided;
                galley_left(&p, pos2(name_x, cy), ng, name_col);
                if let (Some((h, hw)), Some(chain)) = (hint_text, hint.as_ref().map(|_| chain.clone())) {
                    let hg = theme::line(&ctx, h, hint_font, ACCENT, hw + 1.0);
                    let hr = Rect::from_min_size(pos2(name_x + ngw + 14.0 - 6.0, cy - 11.0), vec2(hg.size().x + 12.0, 22.0));
                    let hresp = ui.interact(hr, ui.id().with(("hint", i)), Sense::click());
                    let hcol = if hresp.hovered() { TEXT } else { theme::alpha(ACCENT, 0.9) };
                    if hresp.hovered() {
                        p.rect_filled(hr, CornerRadius::same(6), theme::alpha(ACCENT, 0.16));
                        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    galley_left(&p, pos2(hr.min.x + 6.0, cy), hg, hcol);
                    let hresp = hresp.on_hover_text("Most of this folder's space is down there. Click to jump straight to it.");
                    if hresp.clicked() {
                        let mut np = path.to_vec();
                        np.push(i);
                        np.extend_from_slice(&chain);
                        // a file at the end of the chain: open its folder and select it
                        let target = r_node(node, i, &chain);
                        if target.is_some_and(|t| !t.is_dir()) {
                            let last = np.pop();
                            acts.push(Action::Go(np, last));
                        } else {
                            acts.push(Action::Go(np, None));
                        }
                    }
                }

                if resp.hovered() && c.is_dir() {
                    ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if resp.clicked() {
                    if c.is_dir() {
                        let mut np = path.to_vec();
                        np.push(i);
                        acts.push(Action::Go(np, None));
                    } else {
                        acts.push(Action::Select(Some(i)));
                    }
                }
                let resp = resp.on_hover_ui(|ui| {
                    if name_cut {
                        ui.label(egui::RichText::new(c.name.to_string()).font(theme::semibold(13.0)).color(TEXT));
                    }
                    ui.label(egui::RichText::new(theme::both_sizes(c.size, c.other, metric)).font(theme::regular(12.0)).color(TEXT_DIM));
                });
                if c.kind != Kind::Rest {
                    // the path is built only while the menu is open
                    resp.context_menu(|ui| context_menu_items(ui, &join_path(folder_path, &c.name), acts));
                }
            }
        });
        fade_bottom(ui, &out, BG);
        if s.nav.reveal {
            // one frame is enough to scroll there
            acts.push(Action::RevealDone);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Hotspots panel

    fn hotspots(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let mut acts = Vec::new();
        {
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
            let p = ui.painter().clone();
            let g = theme::line_free(&ctx, "Where your space is", theme::semibold(15.0), TEXT);
            galley_left(&p, pos2(rect.min.x, rect.center().y), g, TEXT);
            let br = Rect::from_center_size(pos2(rect.max.x - 14.0, rect.center().y), vec2(28.0, 28.0));
            let resp = ui.interact(br, ui.id().with("hs_close"), Sense::click());
            if resp.hovered() {
                p.rect_filled(br, CornerRadius::same(7), theme::SURFACE_HOVER);
            }
            paint_icon(&p, br.center(), Icon::PanelClose, if resp.hovered() { TEXT } else { TEXT_FAINT });
            if resp.on_hover_text("Hide").clicked() {
                acts.push(Action::ShowHotspots(false));
            }
        }
        let sub = theme::line(&ctx, "The specific places holding the most, largest first", theme::regular(12.0), TEXT_FAINT, ui.available_width());
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), sub.size().y), Sense::hover());
        ui.painter().galley(r.min, sub, TEXT_FAINT);
        ui.add_space(14.0);

        let Some(ci) = self.current else {
            ui.label(egui::RichText::new("Scan a drive to see this.").font(theme::regular(12.5)).color(TEXT_FAINT));
            self.actions.extend(acts);
            return;
        };
        let s = &self.scans[ci];
        let Some(res) = &s.result else {
            let msg = if s.scanning() { "Appears when the scan finishes." } else { "Scan a drive to see this." };
            ui.label(egui::RichText::new(msg).font(theme::regular(12.5)).color(TEXT_FAINT));
            self.actions.extend(acts);
            return;
        };
        if res.hotspots.is_empty() {
            ui.label(egui::RichText::new("Nothing big enough to point at.").font(theme::regular(12.5)).color(TEXT_FAINT));
            self.actions.extend(acts);
            return;
        }
        // named places first (always actionable), then the leftovers of folders whose big
        // parts were listed, in a quieter section; bars share one scale (the top entry)
        let top = res.hotspots.iter().map(|h| h.size).max().unwrap_or(1).max(1);
        let named: Vec<&Hotspot> = res.hotspots.iter().filter(|h| !h.residual).collect();
        let rest: Vec<&Hotspot> = res.hotspots.iter().filter(|h| h.residual).collect();
        let force_hover = self.opts.hover_hotspots;
        // tooltip: the full path, and both measures for a named place
        let tip = |h: &Hotspot| {
            let mut t = theme::shorten_chars(&h.display, 240);
            if !h.residual
                && let Some(n) = res.root.get(&h.path)
            {
                t.push('\n');
                t.push_str(&theme::both_sizes(n.size, n.other, res.metric));
            }
            t
        };
        let is_active = |h: &Hotspot| {
            s.nav.path == h.path
                || (h.kind == Kind::File
                    && h.path.split_last().is_some_and(|(l, rest)| s.nav.path == rest && s.nav.selected == Some(*l)))
        };
        let mut area = egui::ScrollArea::vertical().id_salt(("hotspots", ci)).auto_shrink([false, false]);
        if let Some(y) = self.opts.scroll_hotspots.take() {
            area = area.vertical_scroll_offset(y);
        }
        let out = area.show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (k, h) in named.iter().enumerate() {
                let row = HotRow { h, top, active: is_active(h), force_hover: force_hover && k == 1, quiet: false, tip: tip(h) };
                hotspot_row(ui, row, &mut acts);
            }
            if !rest.is_empty() {
                ui.add_space(if named.is_empty() { 0.0 } else { 16.0 });
                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width() - 4.0, 30.0), Sense::hover());
                let p = ui.painter();
                if !named.is_empty() {
                    p.line_segment([pos2(r.min.x + 10.0, r.min.y), pos2(r.max.x - 10.0, r.min.y)], Stroke::new(1.0, LINE));
                }
                let g = theme::section_label(&ctx, "Spread across smaller items", TEXT_FAINT);
                p.galley(pos2(r.min.x + 10.0, r.min.y + 12.0), g, TEXT_FAINT);
                ui.add_space(2.0);
                for h in &rest {
                    let row = HotRow { h, top, active: is_active(h), force_hover: false, quiet: true, tip: tip(h) };
                    hotspot_row(ui, row, &mut acts);
                }
            }
            ui.add_space(8.0);
        });
        fade_bottom(ui, &out, SIDE);
        self.actions.extend(acts);
    }
}

/// One hotspot. Named places: name, parent path, size, bar. Leftovers (`quiet`): one dimmer
/// line ("rest of Sam · C:\Users"), size, and a fainter bar on the same scale.
struct HotRow<'a> {
    h: &'a Hotspot,
    /// Size of the largest entry: bars are relative to it.
    top: u64,
    active: bool,
    force_hover: bool,
    quiet: bool,
    tip: String,
}

fn hotspot_row(ui: &mut Ui, row: HotRow, acts: &mut Vec<Action>) {
    let HotRow { h, top, active, force_hover, quiet, tip } = row;
    let ctx = ui.ctx().clone();
    let row_h = if quiet { 40.0 } else { 58.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width() - 4.0, row_h), Sense::click());
    let p = ui.painter().clone();
    let hovered = resp.hovered() || force_hover;
    if active {
        p.rect_filled(rect, CornerRadius::same(9), theme::alpha(ACCENT, 0.12));
    } else if hovered {
        p.rect_filled(rect, CornerRadius::same(9), SURFACE);
    }
    let inner = rect.shrink2(vec2(10.0, if quiet { 7.0 } else { 9.0 }));
    let (parent, leaf) = match h.display.rfind('\\') {
        Some(i) if i + 1 < h.display.len() => (&h.display[..i], &h.display[i + 1..]),
        _ => ("", h.display.as_str()),
    };
    let parent = if parent.len() == 2 && parent.ends_with(':') { format!("{parent}\\") } else { parent.to_owned() };
    let y1 = inner.min.y + 8.0;
    if quiet {
        // rest of Sam · C:\Users                     107 GB
        let sg = theme::line_free(&ctx, format_size(h.size), theme::regular(12.5), TEXT_DIM);
        let sw = sg.size().x;
        galley_right(&p, pos2(inner.max.x, y1), sg, TEXT_DIM);
        let room = inner.width() - sw - 12.0;
        let pre = theme::line_free(&ctx, "rest of ", theme::regular(12.5), TEXT_FAINT);
        let mut x = inner.min.x + pre.size().x;
        galley_left(&p, pos2(inner.min.x, y1), pre, TEXT_FAINT);
        let name_room = (inner.min.x + room - x).max(0.0);
        let ng = theme::line(&ctx, leaf.to_owned(), theme::medium(12.5), TEXT_DIM, name_room);
        let nw = ng.size().x;
        galley_left(&p, pos2(x, y1), ng, TEXT_DIM);
        x += nw;
        let path_room = inner.min.x + room - x - 2.0;
        if path_room > 70.0 && !parent.is_empty() {
            let pf = theme::regular(11.5);
            let pt = theme::middle_ellipsis(&ctx, &format!("  \u{b7}  {parent}"), &pf, path_room);
            let pg = theme::line_free(&ctx, pt, pf, TEXT_FAINT);
            galley_left(&p, pos2(x, y1), pg, TEXT_FAINT);
        }
    } else {
        let sg = theme::line_free(&ctx, format_size(h.size), theme::medium(13.0), TEXT);
        let sw = sg.size().x;
        galley_right(&p, pos2(inner.max.x, y1), sg, TEXT);
        let ng = theme::line(&ctx, leaf.to_owned(), theme::medium(13.5), TEXT, inner.width() - sw - 12.0);
        galley_left(&p, pos2(inner.min.x, y1), ng, TEXT);
        let pf = theme::regular(11.5);
        let pt = theme::middle_ellipsis(&ctx, &parent, &pf, inner.width());
        let pg = theme::line_free(&ctx, pt, pf, TEXT_FAINT);
        galley_left(&p, pos2(inner.min.x, inner.min.y + 26.0), pg, TEXT_FAINT);
    }
    // bar relative to the top entry overall
    let br = Rect::from_min_size(pos2(inner.min.x, inner.max.y - 2.0), vec2(inner.width(), if quiet { 2.0 } else { 2.5 }));
    let col = if h.kind == Kind::File {
        theme::FILE
    } else if quiet {
        TEXT_FAINT
    } else {
        mix(TEXT_FAINT, ACCENT, 0.75)
    };
    let a = match (hovered || active, quiet) {
        (true, _) => 0.95,
        (false, false) => 0.55,
        (false, true) => 0.4,
    };
    bar(&p, br, h.size as f32 / top as f32, theme::alpha(col, a), theme::alpha(theme::TRACK, 0.6));

    if resp.hovered() {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let resp = resp.on_hover_text(tip);
    if resp.clicked() {
        if h.kind == Kind::File {
            let mut pth = h.path.clone();
            let last = pth.pop();
            acts.push(Action::Go(pth, last));
        } else {
            acts.push(Action::Go(h.path.clone(), None));
        }
    }
    resp.context_menu(|ui| context_menu_items(ui, &h.display, acts));
}

fn r_node<'a>(node: &'a Node, i: usize, chain: &[usize]) -> Option<&'a Node> {
    let mut n = node.children.get(i)?;
    for &k in chain {
        n = n.children.get(k)?;
    }
    Some(n)
}

fn context_menu_items(ui: &mut Ui, path: &str, acts: &mut Vec<Action>) {
    ui.set_min_width(170.0);
    if ui.button("Open in Explorer").clicked() {
        acts.push(Action::OpenExplorer(path.to_owned()));
        ui.close();
    }
    if ui.button("Show in Explorer").clicked() {
        acts.push(Action::Reveal(path.to_owned()));
        ui.close();
    }
    if ui.button("Copy path").clicked() {
        acts.push(Action::Copy(path.to_owned()));
        ui.close();
    }
}

// ---------------------------------------------------------------------------------------------
// Demo data (`--demo`): a synthetic C: drive, no disk access.

mod demo {
    use std::sync::atomic::Ordering::Relaxed;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::model::{DriveInfo, DriveKind, Kind, Node, ScanJob, ScanProgress, ScanResult};

    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn f(&mut self) -> f64 {
            (self.next() % 1_000_000) as f64 / 1_000_000.0
        }
    }

    /// A file of `logical` bytes. On disk it takes whole 4 KB clusters; about a third of the logs
    /// and databases are NTFS-compressed and take far less.
    fn file(name: &str, logical: u64) -> Node {
        let h = name.bytes().fold(7u64, |a, b| a.wrapping_mul(31).wrapping_add(b as u64));
        let compressible = name.ends_with(".log") || name.ends_with(".db") || name.ends_with(".json");
        let on_disk = if compressible && h % 3 == 0 {
            (logical as f64 * 0.31) as u64
        } else {
            logical.div_ceil(4096) * 4096
        };
        sized_file(name, logical, on_disk)
    }

    fn sized_file(name: &str, logical: u64, on_disk: u64) -> Node {
        Node {
            name: name.into(),
            size: on_disk,
            other: logical,
            files: 1,
            dirs: 0,
            kind: Kind::File,
            denied: false,
            children: Vec::new(),
        }
    }

    /// `count` small files of `logical` bytes in all; each wastes half a cluster on disk.
    fn rest(count: u64, logical: u64) -> Node {
        Node {
            name: format!("{count} smaller files").into(),
            size: logical + count * 2048,
            files: count,
            dirs: 0,
            kind: Kind::Rest,
            other: logical,
            denied: false,
            children: Vec::new(),
        }
    }

    fn dir(name: &str, mut children: Vec<Node>) -> Node {
        children.sort_by_key(|c| std::cmp::Reverse(c.size));
        Node {
            name: name.into(),
            size: children.iter().map(|c| c.size).sum(),
            files: children.iter().map(|c| c.files).sum(),
            dirs: children.iter().map(|c| if c.is_dir() { 1 + c.dirs } else { 0 }).sum(),
            kind: Kind::Dir,
            other: children.iter().map(|c| c.other).sum(),
            denied: false,
            children,
        }
    }

    /// Cloud placeholders (OneDrive "online-only"): full size, nothing on disk.
    fn cloud(mut n: Node) -> Node {
        let mut stack: Vec<&mut Node> = vec![&mut n];
        while let Some(x) = stack.pop() {
            x.size = 0;
            stack.extend(x.children.iter_mut());
        }
        n
    }

    fn denied(name: &str) -> Node {
        let mut d = dir(name, Vec::new());
        d.denied = true;
        d
    }

    const WORDS: [&str; 24] = [
        "cache", "data", "assets", "models", "logs", "packages", "build", "media", "backup",
        "shaders", "textures", "plugins", "runtime", "locales", "samples", "archive", "temp",
        "projects", "exports", "renders", "snapshots", "index", "blobs", "sessions",
    ];
    const EXTS: [&str; 8] = ["bin", "dat", "pak", "mp4", "zip", "iso", "db", "log"];

    /// A folder of about `total` bytes spread over `n` subfolders and some files, `depth` deep.
    fn filler(name: &str, total: u64, n: usize, depth: u32, rng: &mut Rng) -> Node {
        let mut kids = Vec::new();
        if depth == 0 || n == 0 {
            let nf = 3 + (rng.next() % 6) as usize;
            let mut left = total;
            for k in 0..nf {
                let s = if k + 1 == nf { left } else { (left as f64 * (0.25 + rng.f() * 0.4)) as u64 };
                left -= s;
                let ext = EXTS[(rng.next() % EXTS.len() as u64) as usize];
                kids.push(file(&format!("{}_{k:02}.{ext}", WORDS[(rng.next() % 24) as usize]), s));
            }
            return dir(name, kids);
        }
        // a skewed split: a few big, a long tail
        let weights: Vec<f64> = (0..n).map(|k| (0.3 + rng.f()) / (1.0 + k as f64).powf(1.1)).collect();
        let wsum: f64 = weights.iter().sum();
        let files_share = 0.08 + rng.f() * 0.1;
        let dir_total = (total as f64 * (1.0 - files_share)) as u64;
        for (k, w) in weights.iter().enumerate() {
            let s = (dir_total as f64 * w / wsum) as u64;
            let word = WORDS[(k + (rng.next() % 3) as usize) % WORDS.len()];
            let nm = if k < WORDS.len() { word.to_string() } else { format!("{word}-{k}") };
            kids.push(filler(&nm, s, (n / 2).max(2), depth - 1, rng));
        }
        let ftotal = total - dir_total;
        kids.push(file(&format!("{}.{}", WORDS[(rng.next() % 24) as usize], EXTS[(rng.next() % 8) as usize]), ftotal / 2));
        kids.push(rest(40 + rng.next() % 900, ftotal - ftotal / 2));
        // names must be unique within a folder
        let mut seen = std::collections::HashSet::new();
        for c in kids.iter_mut() {
            let mut nm = c.name.to_string();
            let mut k = 2;
            while !seen.insert(nm.clone()) {
                nm = format!("{}-{k}", c.name);
                k += 1;
            }
            c.name = nm.into();
        }
        dir(name, kids)
    }

    fn tree() -> Node {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let r = &mut rng;

        // kapa (30 GB) -> rapa (20 GB) -> kona (14 GB): the chain the hint should follow
        let mut kona_kids = vec![
            filler("footage", 5 * GB + 300 * MB, 6, 1, r),
            filler("proxies", 3 * GB + 800 * MB, 8, 1, r),
            filler("renders", 2 * GB + 100 * MB, 3, 1, r),
            filler("audio", GB + 600 * MB, 5, 1, r),
            file("project-v3.tar", GB),
        ];
        kona_kids.push(rest(212, 200 * MB));
        let kona = dir("kona", kona_kids);
        let mut rapa_kids = vec![kona];
        for k in 0..6 {
            rapa_kids.push(filler(&format!("shard-{k}"), GB - (k as u64) * 40 * MB, 3, 1, r));
        }
        let rapa = dir("rapa", rapa_kids);
        let mut kapa_kids = vec![rapa];
        for k in 0..19 {
            kapa_kids.push(filler(&format!("{}-{k:02}", WORDS[k % 24]), 480 * MB + (k as u64 % 5) * 40 * MB, 3, 1, r));
        }
        kapa_kids.push(rest(64, 380 * MB));
        let kapa = dir("kapa", kapa_kids);

        let local = dir(
            "Local",
            vec![
                kapa,
                filler("Packages", 9 * GB, 14, 2, r),
                filler("Temp", 7 * GB + 400 * MB, 30, 1, r),
                filler("Microsoft", 5 * GB, 10, 2, r),
                filler("Google", 3 * GB + 200 * MB, 4, 2, r),
                filler("npm-cache", 2 * GB + 600 * MB, 12, 1, r),
                filler("pip", GB + 900 * MB, 4, 1, r),
                filler("NVIDIA", GB + 100 * MB, 3, 1, r),
                // a sparse WSL disk: big file, much less on disk
                dir("Docker", vec![dir("wsl", vec![sized_file("ext4.vhdx", 64 * GB, 21 * GB)])]),
                rest(380, 120 * MB),
            ],
        );
        let appdata = dir(
            "AppData",
            vec![local, filler("Roaming", 12 * GB, 16, 2, r), filler("LocalLow", 900 * MB, 4, 1, r)],
        );

        // a folder with very many small children
        let mut mods = Vec::new();
        for k in 0..900u64 {
            let s = 40 * KB + (r.next() % (3 * MB)) / (1 + k / 40);
            let word = WORDS[(k % 24) as usize];
            mods.push(dir(&format!("{word}-{k}"), vec![file("index.js", s / 2), rest(3 + k % 30, s - s / 2)]));
        }
        mods.push(dir("typescript", vec![file("lib.d.ts", 22 * MB), rest(120, 48 * MB)]));
        mods.push(dir("@swc", vec![file("core.win32-x64-msvc.node", 46 * MB)]));
        let node_modules = dir("node_modules", mods);
        let webapp = dir(
            "webapp",
            vec![node_modules, filler(".next", 640 * MB, 4, 1, r), filler("src", 30 * MB, 6, 1, r), file("package-lock.json", 900 * KB)],
        );
        let dev = dir(
            "Dev",
            vec![webapp, filler("rust-target", 14 * GB, 6, 2, r), filler("android-sdk", 9 * GB, 8, 2, r), filler("scratch", 600 * MB, 12, 1, r)],
        );

        let sam = dir(
            "Sam",
            vec![
                appdata,
                filler("Downloads", 38 * GB, 22, 1, r),
                filler("Videos", 31 * GB, 9, 1, r),
                filler("Documents", 24 * GB, 14, 2, r),
                dir(".cache", vec![filler("gradle", 22 * GB, 6, 2, r), filler("yarn", 3 * GB, 3, 1, r)]),
                filler("Pictures", 11 * GB, 12, 1, r),
                cloud(filler("OneDrive", 26 * GB, 9, 1, r)),
                filler("Desktop", 2 * GB + 300 * MB, 18, 1, r),
                filler("Music", 4 * GB, 30, 1, r),
                dev,
                file("NTUSER.DAT", 18 * MB),
                rest(42, 6 * MB),
            ],
        );
        let users = dir(
            "Users",
            vec![sam, filler("Public", 2 * GB, 6, 1, r), filler("Default", 900 * MB, 5, 1, r), denied("Administrator")],
        );

        let windows = dir(
            "Windows",
            vec![
                filler("WinSxS", 14 * GB, 40, 1, r),
                filler("System32", 9 * GB, 40, 1, r),
                filler("Installer", 6 * GB, 60, 0, r),
                filler("SoftwareDistribution", 3 * GB, 6, 1, r),
                filler("SysWOW64", 2 * GB + 500 * MB, 30, 1, r),
                filler("assembly", GB + 400 * MB, 6, 1, r),
                filler("Fonts", 700 * MB, 3, 0, r),
                denied("CSC"),
                rest(320, 90 * MB),
            ],
        );
        let pf = dir(
            "Program Files",
            vec![
                filler("NVIDIA Corporation", 6 * GB, 8, 2, r),
                filler("Microsoft Office", 5 * GB, 6, 2, r),
                filler("Adobe", 8 * GB, 5, 2, r),
                filler("Blender Foundation", GB + 200 * MB, 3, 2, r),
                filler("WindowsApps", 9 * GB, 30, 1, r),
                filler("Docker", 4 * GB, 5, 1, r),
                filler("Common Files", GB, 12, 1, r),
            ],
        );
        let games = dir(
            "Games",
            vec![
                filler("Iron Harbor", 71 * GB, 6, 2, r),
                filler("Starfall Odyssey", 128 * GB, 5, 2, r),
                filler("Glasswing", 49 * GB, 4, 2, r),
                filler("Tiny Tactics", 11 * GB, 4, 1, r),
                filler("Northwind Chronicles Definitive Edition - a very long folder name to check truncation", 8 * GB, 3, 1, r),
            ],
        );
        dir(
            "C:",
            vec![
                users,
                windows,
                pf,
                games,
                filler("Program Files (x86)", 12 * GB, 12, 2, r),
                filler("ProgramData", 16 * GB, 14, 2, r),
                file("pagefile.sys", 16 * GB),
                file("hiberfil.sys", 12 * GB + 700 * MB),
                file("swapfile.sys", 256 * MB),
                filler("$Recycle.Bin", 4 * GB, 3, 1, r),
                denied("System Volume Information"),
                filler("Recovery", 900 * MB, 2, 1, r),
            ],
        )
    }

    pub fn drives() -> Vec<DriveInfo> {
        vec![
            DriveInfo { root: "C:\\".into(), letter: 'C', label: "Windows".into(), total: 1863 * GB, free: 1863 * GB - 700 * GB, kind: DriveKind::Fixed },
            DriveInfo { root: "D:\\".into(), letter: 'D', label: "Data".into(), total: 3726 * GB, free: 1210 * GB, kind: DriveKind::Fixed },
            DriveInfo { root: "E:\\".into(), letter: 'E', label: String::new(), total: 476 * GB, free: 21 * GB, kind: DriveKind::Removable },
        ]
    }

    pub fn result() -> ScanResult {
        let root = tree();
        let root_path = "C:\\".to_string();
        let hotspots = crate::hotspots::compute(&root, &root_path, 40);
        ScanResult {
            root_path,
            root,
            hotspots,
            elapsed: Duration::from_millis(3240),
            denied_dirs: 3,
            volume: Some((1863 * GB, 1863 * GB - 700 * GB)),
            method: "MFT",
            metric: crate::model::Metric::OnDisk,
        }
    }

    /// A scan that keeps counting and never finishes (until cancelled), for the progress view.
    pub fn scanning_job(root_path: &str) -> ScanJob {
        let progress = Arc::new(ScanProgress::new());
        progress.files.store(612_408, Relaxed);
        progress.dirs.store(81_377, Relaxed);
        progress.bytes.store(1_190 * GB, Relaxed);
        progress.denied.store(12, Relaxed);
        let result: Arc<Mutex<Option<ScanResult>>> = Arc::new(Mutex::new(None));
        let (p, res, root) = (progress.clone(), result.clone(), root_path.to_string());
        std::thread::spawn(move || {
            let mut k = 0u64;
            loop {
                std::thread::sleep(Duration::from_millis(30));
                k += 1;
                p.files.fetch_add(311 + k % 97, Relaxed);
                p.dirs.fetch_add(23 + k % 7, Relaxed);
                p.bytes.fetch_add(180 * MB + (k % 13) * 7 * MB, Relaxed);
                if let Ok(mut c) = p.current.try_lock() {
                    *c = format!("{root}GameLibrary\\Starfall Odyssey\\Data\\Localization\\English\\part_{k:05}");
                }
                if p.cancelled() {
                    let node = dir(root.trim_end_matches('\\'), vec![file("partial.bin", 5 * GB)]);
                    if let Ok(mut r) = res.lock() {
                        *r = Some(ScanResult {
                            root_path: root.clone(),
                            root: node,
                            hotspots: Vec::new(),
                            elapsed: p.started.elapsed(),
                            denied_dirs: 0,
                            volume: None,
                            method: "walk",
                            metric: crate::model::Metric::OnDisk,
                        });
                    }
                    return;
                }
            }
        });
        ScanJob { root_path: root_path.to_string(), progress, result }
    }
}
