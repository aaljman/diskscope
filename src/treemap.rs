//! Squarified treemap of one folder, nested one level (grandchildren drawn inside big tiles).
//! A nested tile looks through pass-through folders (one child holding nearly everything) and
//! shows the contents of the first level where the space actually splits.

use eframe::egui::{
    self, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2,
    vec2,
};

use crate::model::{Kind, Node, format_count, format_size};
use crate::theme::{self, BG, TEXT, mix};

/// What a tile stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Child(usize),
    /// A tile inside child `.0`; `.1` is the index path below that child (the pass-through
    /// folders the nested tile looked through, then the inner tile itself).
    Grandchild(usize, Vec<usize>),
    /// Space the volume reports used but the scan could not see (drive roots only).
    Unaccounted,
    /// Children too small to draw, gathered into one tile.
    More,
}

pub struct Input<'a> {
    pub node: &'a Node,
    /// One colour per child of `node` (see `theme::child_colors`).
    pub colors: &'a [Color32],
    pub unaccounted: Option<u64>,
    pub selected: Option<usize>,
    /// Full path of `node`, for tooltips.
    pub folder_path: &'a str,
    /// Which measure `Node::size` holds (tooltips show both).
    pub metric: crate::model::Metric,
}

pub struct Output {
    pub response: egui::Response,
    pub hovered: Option<Target>,
    pub clicked: Option<Target>,
}

const GAP: f32 = 3.0;
const INNER_GAP: f32 = 2.0;
const HEADER: f32 = 22.0;
/// Tiles below this area (px²) are folded into the "more" tile.
const MIN_AREA: f32 = 150.0;
const MAX_TILES: usize = 400;
const MAX_INNER: usize = 80;
/// A folder whose largest child (a folder) holds at least this share is only a wrapper:
/// a nested tile looks through it.
const PASS_THROUGH: f64 = 0.85;

struct Entry {
    target: Target,
    size: u64,
}

struct Tile {
    target: Target,
    rect: Rect,
    /// The inner tiles, when the tile is nested.
    inner: Option<Inner>,
    size: u64,
    more_count: u64,
}

struct Inner {
    /// Pass-through levels below the child (empty when its own children are shown).
    via: Vec<usize>,
    /// Inner tiles: index into the children of the node at `via`, and its rect.
    tiles: Vec<(usize, Rect)>,
}

/// Follows wrapper folders down from `n`: while the largest child is a folder holding at least
/// `PASS_THROUGH` of its parent (and has something inside), step into it. Iterative, so a
/// chain thousands of folders deep costs nothing but the walk.
fn look_through(n: &Node) -> (Vec<usize>, &Node) {
    let mut via = Vec::new();
    let mut n = n;
    while let Some(big) = n.children.first() {
        let dominant = n.size > 0 && big.size as f64 >= PASS_THROUGH * n.size as f64;
        if !(dominant && big.is_dir() && !big.children.is_empty()) {
            break;
        }
        via.push(0);
        n = big;
    }
    (via, n)
}

/// Squarified layout (Bruls, Huizing, van Wijk). `values` must be sorted largest first.
/// Returns one rect per value (empty rects for zero values).
pub fn squarify(values: &[f64], rect: Rect) -> Vec<Rect> {
    let n = values.len();
    let mut out = vec![Rect::NOTHING; n];
    let total: f64 = values.iter().sum();
    if total <= 0.0 || rect.width() <= 0.0 || rect.height() <= 0.0 {
        return out;
    }
    let scale = (rect.width() as f64 * rect.height() as f64) / total;
    let areas: Vec<f64> = values.iter().map(|v| v * scale).collect();

    let (mut x, mut y) = (rect.min.x as f64, rect.min.y as f64);
    let (mut w, mut h) = (rect.width() as f64, rect.height() as f64);
    let mut i = 0;
    while i < n {
        if areas[i] <= 0.0 {
            i += 1;
            continue;
        }
        let side = w.min(h).max(1e-9);
        // grow the row while the worst aspect ratio improves
        let mut j = i;
        let mut sum = 0.0;
        let mut best = f64::INFINITY;
        while j < n && areas[j] > 0.0 {
            let s = sum + areas[j];
            let rmax = areas[i];
            let rmin = areas[j];
            let worst = ((side * side * rmax) / (s * s)).max((s * s) / (side * side * rmin));
            if worst > best {
                break;
            }
            best = worst;
            sum = s;
            j += 1;
        }
        let last = j == n || areas[j] <= 0.0;
        if w >= h {
            // column on the left
            let cw = if last { w } else { sum / h };
            let mut yy = y;
            for k in i..j {
                let hh = if k + 1 == j { y + h - yy } else { areas[k] / cw };
                out[k] = Rect::from_min_size(
                    pos2(x as f32, yy as f32),
                    vec2(cw as f32, hh as f32),
                );
                yy += hh;
            }
            x += cw;
            w -= cw;
        } else {
            // row on top
            let rh = if last { h } else { sum / w };
            let mut xx = x;
            for k in i..j {
                let ww = if k + 1 == j { x + w - xx } else { areas[k] / rh };
                out[k] = Rect::from_min_size(
                    pos2(xx as f32, y as f32),
                    vec2(ww as f32, rh as f32),
                );
                xx += ww;
            }
            y += rh;
            h -= rh;
        }
        i = j;
        if last {
            break;
        }
    }
    out
}

fn entries(input: &Input, area: f32) -> (Vec<Entry>, u64, u64) {
    let node = input.node;
    let unacc = input.unaccounted.unwrap_or(0);
    let total = node.size + unacc;
    let mut out = Vec::new();
    let mut unacc_done = unacc == 0;
    let mut more = 0u64;
    let mut more_count = 0u64;
    let min_size = if area > 0.0 {
        (total as f64 * MIN_AREA as f64 / area as f64) as u64
    } else {
        u64::MAX
    };
    for (i, c) in node.children.iter().enumerate() {
        if !unacc_done && unacc >= c.size {
            out.push(Entry { target: Target::Unaccounted, size: unacc });
            unacc_done = true;
        }
        if c.size == 0 {
            continue;
        }
        if out.len() >= MAX_TILES || c.size < min_size {
            more += c.size;
            more_count += 1;
            continue;
        }
        out.push(Entry { target: Target::Child(i), size: c.size });
    }
    if !unacc_done {
        out.push(Entry { target: Target::Unaccounted, size: unacc });
    }
    if more > 0 {
        out.push(Entry { target: Target::More, size: more });
    }
    // "more" and a late unaccounted can break the order; squarify wants it sorted
    out.sort_by_key(|e| std::cmp::Reverse(e.size));
    (out, total, more_count)
}

/// Lays out the children of `child` in `body`, where `body` stands for `total` bytes (the tile's
/// own size, which can be more than `child` when wrapper levels were looked through).
fn inner_layout(child: &Node, total: u64, body: Rect) -> Vec<(usize, Rect)> {
    let area = body.area();
    if total == 0 || area <= 0.0 {
        return Vec::new();
    }
    let min_size = (total as f64 * (MIN_AREA * 1.5) as f64 / area as f64) as u64;
    // (child index or None for the hidden remainder, size)
    let mut slots: Vec<(Option<usize>, u64)> = Vec::new();
    let mut shown = 0u64;
    for (i, g) in child.children.iter().enumerate() {
        // children are sorted largest first: the rest are smaller still
        if g.size == 0 || g.size < min_size || slots.len() >= MAX_INNER {
            break;
        }
        slots.push((Some(i), g.size));
        shown += g.size;
    }
    // the remainder (small children and the skipped levels' own files) keeps its share of the
    // body; the parent colour shows through there
    let rest = total.saturating_sub(shown);
    if rest > 0 {
        slots.push((None, rest));
    }
    slots.sort_by_key(|s| std::cmp::Reverse(s.1));
    let vals: Vec<f64> = slots.iter().map(|s| s.1 as f64).collect();
    squarify(&vals, body)
        .into_iter()
        .zip(slots)
        .filter_map(|(r, (i, _))| i.map(|i| (i, r)))
        .collect()
}

fn tile_radius(r: Rect, max: f32) -> CornerRadius {
    // small tiles stay squarish so a field of them reads as a treemap, not as dots
    let rad = (r.width().min(r.height()) / 6.0).clamp(1.0, max);
    CornerRadius::same(rad.round() as u8)
}

fn layout(input: &Input, rect: Rect) -> (Vec<Tile>, u64) {
    let (entries, _total, more_count) = entries(input, rect.area());
    let vals: Vec<f64> = entries.iter().map(|e| e.size as f64).collect();
    // lay out in a rect grown by half a gap so the outer edges line up with `rect`
    let rects = squarify(&vals, rect.expand(GAP / 2.0));
    let mut tiles = Vec::with_capacity(entries.len());
    for (e, r) in entries.iter().zip(rects) {
        let r = r.shrink(GAP / 2.0);
        if r.width() < 3.0 || r.height() < 3.0 {
            continue;
        }
        let mut inner = None;
        if let Target::Child(i) = e.target {
            let c = &input.node.children[i];
            if c.is_dir() && !c.children.is_empty() && r.width() >= 84.0 && r.height() >= 58.0 {
                let body = Rect::from_min_max(
                    pos2(r.min.x + 3.0, r.min.y + HEADER),
                    pos2(r.max.x - 3.0, r.max.y - 3.0),
                );
                let (via, deep) = look_through(c);
                let tiles = inner_layout(deep, c.size, body.expand(INNER_GAP / 2.0))
                    .into_iter()
                    .map(|(g, gr)| (g, gr.shrink(INNER_GAP / 2.0)))
                    // slivers read as noise; the parent colour shows there instead
                    .filter(|(_, gr)| gr.width() >= 4.0 && gr.height() >= 4.0)
                    .collect();
                inner = Some(Inner { via, tiles });
            }
        }
        tiles.push(Tile {
            target: e.target.clone(),
            rect: r,
            inner,
            size: e.size,
            more_count: if e.target == Target::More { more_count } else { 0 },
        });
    }
    (tiles, more_count)
}

fn hit(tiles: &[Tile], p: Pos2) -> Option<Target> {
    let t = tiles.iter().find(|t| t.rect.contains(p))?;
    if let (Target::Child(i), Some(inner)) = (&t.target, &t.inner)
        && let Some((g, _)) = inner.tiles.iter().find(|(_, r)| r.contains(p))
    {
        let mut path = inner.via.clone();
        path.push(*g);
        return Some(Target::Grandchild(*i, path));
    }
    Some(t.target.clone())
}

/// Header of a nested tile: `name`, or `name › deep` / `name › … › deep` when it looks through
/// wrapper folders. When it doesn't fit, both names are cut so the deep one (where the space
/// is) stays readable; the separator never is.
fn header_galley(
    ctx: &egui::Context,
    name: &str,
    via: &[usize],
    deep: &Node,
    font: &egui::FontId,
    max_w: f32,
) -> std::sync::Arc<egui::Galley> {
    use egui::text::{LayoutJob, TextFormat};
    let sep = match via.len() {
        0 => return theme::line(ctx, name.to_owned(), font.clone(), TEXT, max_w),
        1 => "  \u{203a}  ",
        _ => "  \u{203a}  \u{2026}  \u{203a}  ",
    };
    let sep_w = theme::text_width(ctx, sep, font);
    let name_w = theme::text_width(ctx, name, font);
    let deep_w = theme::text_width(ctx, &deep.name, font);
    let avail = max_w - sep_w;
    let (name_t, deep_t) = if name_w + deep_w <= avail {
        (name.to_owned(), deep.name.to_string())
    } else if avail >= 70.0 {
        // the deep name gets what it needs up to 60 %, or more when the first name is short
        let dw = deep_w.min((avail * 0.6).max(avail - name_w));
        let nw = avail - dw;
        (theme::truncate_end(ctx, name, font, nw), theme::truncate_end(ctx, &deep.name, font, dw))
    } else {
        return theme::line(ctx, name.to_owned(), font.clone(), TEXT, max_w);
    };
    let mut job = LayoutJob::default();
    job.append(&name_t, 0.0, TextFormat::simple(font.clone(), TEXT));
    job.append(sep, 0.0, TextFormat::simple(font.clone(), theme::alpha(TEXT, 0.45)));
    job.append(&deep_t, 0.0, TextFormat::simple(font.clone(), TEXT));
    job.break_on_newline = false;
    ctx.fonts_mut(|f| f.layout_job(job))
}

/// The node at `path` below `n`.
fn descend<'a>(n: &'a Node, path: &[usize]) -> &'a Node {
    n.get(path).unwrap_or(n)
}

/// Name (cut with an ellipsis) and, when it fits whole, the size, in the top-left of `r`.
/// Nothing is drawn where it would not be readable.
#[allow(clippy::too_many_arguments)]
fn tile_text(
    ctx: &egui::Context,
    painter: &egui::Painter,
    r: Rect,
    pad: egui::Vec2,
    name: &str,
    size: Option<String>,
    fonts: (&egui::FontId, &egui::FontId),
    colors: (Color32, Color32),
) {
    let w = r.width() - 2.0 * pad.x;
    if w < 18.0 {
        return;
    }
    let ng = theme::line(ctx, name.to_owned(), fonts.0.clone(), colors.0, w);
    let lh = ng.size().y;
    if r.height() < lh + 2.0 * pad.y - 2.0 || (ng.elided && ng.size().x < 30.0) {
        return;
    }
    painter.galley(r.min + pad, ng, colors.0);
    if let Some(s) = size {
        let sg = theme::line_free(ctx, s, fonts.1.clone(), colors.1);
        if sg.size().x <= w && r.height() >= lh + sg.size().y + 2.0 * pad.y - 2.0 {
            painter.galley(r.min + pad + vec2(0.0, lh), sg, colors.1);
        }
    }
}

fn hatch(painter: &egui::Painter, r: Rect, color: Color32) {
    let p = painter.with_clip_rect(r.shrink(1.0).intersect(painter.clip_rect()));
    let step = 7.0;
    let mut x = r.min.x - r.height();
    while x < r.max.x {
        p.line_segment(
            [pos2(x, r.max.y), pos2(x + r.height(), r.min.y)],
            Stroke::new(1.0, color),
        );
        x += step;
    }
}

pub fn show(ui: &mut Ui, rect: Rect, input: &Input) -> Output {
    let id = ui.id().with("treemap");
    let response = ui.interact(rect, id, Sense::click());
    let painter = ui.painter_at(rect.expand(2.0));
    let ctx = ui.ctx().clone();
    let node = input.node;

    let (tiles, _) = layout(input, rect);
    if tiles.is_empty() {
        painter.rect_filled(rect, CornerRadius::same(10), theme::alpha(theme::SURFACE, 0.6));
        let (title, sub) = if node.denied {
            ("No access to this folder", "Windows did not let the scan read it. Restart as admin to see inside.")
        } else if node.children.is_empty() {
            ("This folder is empty", "")
        } else {
            ("Nothing here takes any space", "")
        };
        let g = theme::line(&ctx, title, theme::medium(14.0), theme::TEXT_DIM, rect.width() - 24.0);
        let c = rect.center();
        let gh = g.size().y;
        painter.galley(c - vec2(g.size().x / 2.0, gh + if sub.is_empty() { -gh / 2.0 } else { 2.0 }), g, theme::TEXT_DIM);
        if !sub.is_empty() {
            let sg = theme::line(&ctx, sub, theme::regular(12.5), theme::TEXT_FAINT, rect.width() - 24.0);
            painter.galley(c + vec2(-sg.size().x / 2.0, 4.0), sg, theme::TEXT_FAINT);
        }
        return Output { response, hovered: None, clicked: None };
    }

    let hovered = response.hover_pos().and_then(|p| hit(&tiles, p));
    let clicked = if response.clicked() { hovered.clone() } else { None };

    let name_font = theme::medium(12.5);
    let size_font = theme::regular(11.5);
    let small_font = theme::regular(11.0);
    let text_soft = theme::alpha(TEXT, 0.62);

    for t in &tiles {
        let r = t.rect;
        let is_hover_tile = match (&hovered, &t.target) {
            (Some(Target::Grandchild(a, _)), Target::Child(b)) => a == b,
            (Some(h), tt) => h == tt,
            _ => false,
        };
        match t.target {
            Target::Child(i) => {
                let c = &node.children[i];
                let base = input.colors.get(i).copied().unwrap_or(theme::FILE);
                if let Some(inner) = &t.inner {
                    // nested tile: dark tinted frame, header label, lighter grandchildren
                    let mut frame = mix(BG, base, 0.28);
                    if is_hover_tile && matches!(hovered, Some(Target::Child(_))) {
                        frame = mix(BG, base, 0.38);
                    }
                    painter.rect_filled(r, tile_radius(r, 7.0), frame);
                    let deep = descend(c, &inner.via);
                    for (g, gr) in &inner.tiles {
                        let gc = &deep.children[*g];
                        let k = match gc.kind {
                            Kind::Dir => 0.60,
                            Kind::File => 0.50,
                            Kind::Rest => 0.40,
                        };
                        let mut fill = mix(BG, base, k);
                        if gc.kind == Kind::File {
                            fill = mix(fill, theme::FILE, 0.25);
                        }
                        let over = matches!(&hovered, Some(Target::Grandchild(hi, hp)) if *hi == i && hp.last() == Some(g));
                        if over {
                            fill = mix(fill, Color32::WHITE, 0.14);
                        }
                        painter.rect_filled(*gr, tile_radius(*gr, 4.0), fill);
                        tile_text(
                            &ctx,
                            &painter,
                            *gr,
                            vec2(6.0, 4.0),
                            &gc.name,
                            Some(format_size(gc.size)),
                            (&small_font, &small_font),
                            (theme::alpha(TEXT, 0.86), theme::alpha(TEXT, 0.55)),
                        );
                    }
                    // header: name left, size right
                    let size_g = theme::line_free(&ctx, format_size(c.size), size_font.clone(), text_soft);
                    let avail = r.width() - 18.0;
                    let show_size = avail > size_g.size().x + 56.0;
                    let name_w = if show_size { avail - size_g.size().x - 10.0 } else { avail };
                    let ng = header_galley(&ctx, &c.name, &inner.via, deep, &name_font, name_w);
                    if !(ng.elided && ng.size().x < 30.0) {
                        let y = r.min.y + (HEADER - ng.size().y) / 2.0;
                        painter.galley(pos2(r.min.x + 9.0, y), ng, TEXT);
                    }
                    if show_size {
                        let sy = r.min.y + (HEADER - size_g.size().y) / 2.0;
                        painter.galley(pos2(r.max.x - 9.0 - size_g.size().x, sy), size_g, text_soft);
                    }
                } else {
                    let k = match c.kind {
                        Kind::Dir => 0.60,
                        Kind::File => 0.80,
                        Kind::Rest => 0.80,
                    };
                    let mut fill = mix(BG, base, k);
                    if is_hover_tile {
                        fill = mix(fill, Color32::WHITE, 0.12);
                    }
                    painter.rect_filled(r, tile_radius(r, 6.0), fill);
                    tile_text(
                        &ctx,
                        &painter,
                        r,
                        vec2(8.0, 5.0),
                        &c.name,
                        Some(format_size(c.size)),
                        (&name_font, &size_font),
                        (TEXT, text_soft),
                    );
                }
                if input.selected == Some(i) {
                    painter.rect_stroke(r, tile_radius(r, 7.0), Stroke::new(2.0, theme::ACCENT), StrokeKind::Inside);
                } else if is_hover_tile {
                    painter.rect_stroke(
                        r,
                        tile_radius(r, 7.0),
                        Stroke::new(1.0, theme::alpha(Color32::WHITE, 0.35)),
                        StrokeKind::Inside,
                    );
                }
            }
            Target::Unaccounted => {
                let fill = if is_hover_tile { mix(BG, theme::UNACCOUNTED, 0.9) } else { mix(BG, theme::UNACCOUNTED, 0.6) };
                painter.rect_filled(r, tile_radius(r, 6.0), fill);
                hatch(&painter, r, theme::alpha(Color32::WHITE, 0.045));
                painter.rect_stroke(r, tile_radius(r, 6.0), Stroke::new(1.0, theme::alpha(Color32::WHITE, 0.06)), StrokeKind::Inside);
                tile_text(
                    &ctx,
                    &painter,
                    r,
                    vec2(8.0, 5.0),
                    "Unaccounted",
                    Some(format_size(input.unaccounted.unwrap_or(0))),
                    (&name_font, &size_font),
                    (theme::TEXT_DIM, theme::TEXT_FAINT),
                );
            }
            Target::More => {
                let mut fill = mix(BG, theme::REST, 0.55);
                if is_hover_tile {
                    fill = mix(fill, Color32::WHITE, 0.08);
                }
                painter.rect_filled(r, tile_radius(r, 6.0), fill);
                tile_text(
                    &ctx,
                    &painter,
                    r,
                    vec2(8.0, 5.0),
                    &format!("{} more", format_count(t.more_count)),
                    Some(format_size(t.size)),
                    (&size_font, &size_font),
                    (theme::TEXT_DIM, theme::TEXT_FAINT),
                );
            }
            Target::Grandchild(..) => {}
        }
    }

    // tooltip
    if let Some(h) = &hovered {
        let total = node.size + input.unaccounted.unwrap_or(0);
        let sep = if input.folder_path.ends_with('\\') { "" } else { "\\" };
        let info: Option<Tip> = match h {
            Target::Child(i) => {
                let c = &node.children[*i];
                let path = format!("{}{sep}{}", input.folder_path, c.name);
                Some(Tip::of(c, path, total, node.name.to_string(), input.metric))
            }
            Target::Grandchild(i, rel) => {
                let c = &node.children[*i];
                let mut path = format!("{}{sep}{}", input.folder_path, c.name);
                let mut n = c;
                for &k in rel {
                    n = &n.children[k];
                    path.push('\\');
                    path.push_str(&n.name);
                }
                Some(Tip::of(n, path, c.size, c.name.to_string(), input.metric))
            }
            Target::Unaccounted => {
                let u = input.unaccounted.unwrap_or(0);
                Some(Tip {
                    name: "Unaccounted".into(),
                    path: "Used space the scan could not see: system files, folders without access".into(),
                    sizes: format!("{} on disk", format_size(u)),
                    share: format!("{} of {}", theme::percent(u, total), node.name),
                })
            }
            Target::More => None,
        };
        if let Some(tip) = info {
            // a path thousands of folders deep would make a tooltip the size of the screen
            let path = theme::shorten_chars(&tip.path, 240);
            response.clone().on_hover_ui_at_pointer(|ui| {
                ui.set_max_width(420.0);
                ui.label(egui::RichText::new(&tip.name).font(theme::semibold(13.5)).color(TEXT));
                ui.add(egui::Label::new(egui::RichText::new(path).font(theme::regular(12.0)).color(theme::TEXT_DIM)).wrap());
                ui.add_space(2.0);
                ui.label(egui::RichText::new(&tip.sizes).font(theme::regular(12.0)).color(TEXT));
                ui.label(egui::RichText::new(&tip.share).font(theme::regular(12.0)).color(theme::TEXT_DIM));
            });
        }
    }

    if matches!(hovered, Some(Target::Child(_) | Target::Grandchild(..))) {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    Output { response, hovered, clicked }
}

/// What the treemap tooltip says about a tile.
struct Tip {
    name: String,
    path: String,
    /// "12.4 GB on disk · 13.1 GB size"
    sizes: String,
    /// "38% of Users · 1,204 files · 88 folders"
    share: String,
}

impl Tip {
    fn of(n: &Node, path: String, of: u64, parent: String, metric: crate::model::Metric) -> Self {
        let mut share = format!("{} of {}", theme::percent(n.size, of), parent);
        let items = items_text(n);
        if !items.is_empty() {
            share.push_str("  \u{b7}  ");
            share.push_str(&items);
        }
        Tip { name: n.name.to_string(), path, sizes: theme::both_sizes(n.size, n.other, metric), share }
    }
}

/// `1,204 files · 88 folders`, `file`, `312 files`.
pub fn items_text(n: &Node) -> String {
    match n.kind {
        Kind::File => "file".into(),
        Kind::Rest => format!("{} files", format_count(n.files)),
        Kind::Dir => {
            if n.denied {
                return "no access".into();
            }
            let f = if n.files == 1 { "file" } else { "files" };
            if n.dirs == 0 {
                format!("{} {f}", format_count(n.files))
            } else {
                let d = if n.dirs == 1 { "folder" } else { "folders" };
                format!("{} {f} · {} {d}", format_count(n.files), format_count(n.dirs))
            }
        }
    }
}
