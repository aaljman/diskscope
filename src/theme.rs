//! Colours, fonts, egui style and small text helpers shared by the UI.

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, FontTweak, Galley,
    Margin, Stroke,
    epaint::text::{LayoutJob, TextFormat, TextWrapping, VariationCoords},
};

// ---------------------------------------------------------------------------------------------
// Colours

/// Window background behind the browse view.
pub const BG: Color32 = Color32::from_rgb(17, 18, 22);
/// Side panels (drives, hotspots): a touch darker than the centre.
pub const SIDE: Color32 = Color32::from_rgb(13, 14, 17);
/// Cards and raised rows.
pub const SURFACE: Color32 = Color32::from_rgb(25, 27, 32);
pub const SURFACE_HOVER: Color32 = Color32::from_rgb(30, 32, 39);
/// Hairlines between areas.
pub const LINE: Color32 = Color32::from_rgb(32, 34, 41);
/// Bar tracks.
pub const TRACK: Color32 = Color32::from_rgb(34, 36, 43);

pub const TEXT: Color32 = Color32::from_rgb(229, 231, 236);
pub const TEXT_DIM: Color32 = Color32::from_rgb(150, 155, 166);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(98, 103, 115);

/// The one accent: selection, focus hints, progress.
pub const ACCENT: Color32 = Color32::from_rgb(134, 148, 255);
/// Quiet warning (nearly full drives, denied folders).
pub const WARN: Color32 = Color32::from_rgb(222, 165, 104);

/// Files in the treemap: neutral slate.
pub const FILE: Color32 = Color32::from_rgb(92, 102, 120);
/// What small folders fade toward.
pub const NEUTRAL: Color32 = Color32::from_rgb(98, 106, 122);
/// The "N smaller files" aggregate: dim grey.
pub const REST: Color32 = Color32::from_rgb(62, 66, 76);
/// Space the scan could not see.
pub const UNACCOUNTED: Color32 = Color32::from_rgb(52, 55, 64);

/// Soft categorical palette for folders, in rank order.
pub const PALETTE: [Color32; 10] = [
    Color32::from_rgb(98, 128, 210),  // blue
    Color32::from_rgb(70, 160, 152),  // teal
    Color32::from_rgb(204, 150, 88),  // amber
    Color32::from_rgb(178, 108, 158), // mauve
    Color32::from_rgb(116, 160, 104), // sage
    Color32::from_rgb(208, 114, 100), // coral
    Color32::from_rgb(136, 116, 208), // violet
    Color32::from_rgb(82, 152, 196),  // sky
    Color32::from_rgb(176, 166, 94),  // olive gold
    Color32::from_rgb(160, 126, 112), // clay
];

/// Linear mix in sRGB space, `t = 0` gives `a`.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(
        f(a.r(), b.r()),
        f(a.g(), b.g()),
        f(a.b(), b.b()),
        f(a.a(), b.a()),
    )
}

pub fn alpha(c: Color32, a: f32) -> Color32 {
    c.gamma_multiply(a)
}

/// Colour for each child of a folder: folders take palette colours in rank order, files are
/// slate, the small-files aggregate is grey. The treemap and the list share this.
pub fn child_colors(node: &crate::model::Node) -> Vec<Color32> {
    use crate::model::Kind;
    let mut k = 0;
    node.children
        .iter()
        .map(|c| match c.kind {
            Kind::Dir => {
                // the long tail of small folders fades toward a quiet slate: no confetti
                let mut col = PALETTE[k % PALETTE.len()];
                if k >= 8 {
                    col = mix(col, NEUTRAL, ((k - 7) as f32 / 8.0).min(0.7));
                }
                k += 1;
                col
            }
            Kind::File => FILE,
            Kind::Rest => REST,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Fonts

pub const F_MEDIUM: &str = "ui-medium";
pub const F_SEMIBOLD: &str = "ui-semibold";
pub const F_DISPLAY: &str = "ui-display";

pub fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(F_MEDIUM.into()))
}
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(F_SEMIBOLD.into()))
}
/// Large headings and numbers (display optical size).
pub fn display(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(F_DISPLAY.into()))
}

fn read_font(name: &str) -> Option<&'static [u8]> {
    let dir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let bytes = std::fs::read(format!(r"{dir}\Fonts\{name}")).ok()?;
    Some(Box::leak(bytes.into_boxed_slice()))
}

fn variable(bytes: &'static [u8], wght: f32, opsz: f32) -> FontData {
    FontData::from_static(bytes).tweak(FontTweak {
        coords: VariationCoords::new([(b"wght", wght), (b"opsz", opsz)]),
        ..Default::default()
    })
}

/// Segoe UI Variable from C:\Windows\Fonts (or the static Segoe UI files), with egui's own
/// fonts kept as fallbacks for missing glyphs.
fn fonts() -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    let fallback: Vec<String> = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();

    let mut faces: Vec<(&str, Option<FontData>)> = Vec::new();
    if let Some(var) = read_font("SegUIVar.ttf") {
        faces.push(("ui-regular", Some(variable(var, 400.0, 10.5))));
        faces.push((F_MEDIUM, Some(variable(var, 500.0, 10.5))));
        faces.push((F_SEMIBOLD, Some(variable(var, 600.0, 10.5))));
        faces.push((F_DISPLAY, Some(variable(var, 600.0, 28.0))));
    } else {
        let reg = read_font("segoeui.ttf").map(FontData::from_static);
        let sb = read_font("seguisb.ttf").map(FontData::from_static);
        faces.push(("ui-regular", reg.clone()));
        faces.push((F_MEDIUM, sb.clone().or(reg.clone())));
        faces.push((F_SEMIBOLD, sb.clone().or(reg.clone())));
        faces.push((F_DISPLAY, sb.or(reg)));
    }

    for (name, data) in faces {
        let mut list = Vec::new();
        if let Some(data) = data {
            defs.font_data.insert(name.to_owned(), Arc::new(data));
            list.push(name.to_owned());
        }
        list.extend(fallback.iter().cloned());
        let family = if name == "ui-regular" {
            FontFamily::Proportional
        } else {
            FontFamily::Name(name.into())
        };
        defs.families.insert(family, list);
    }
    defs
}

/// Fonts and the egui style for the whole app.
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        use egui::TextStyle;
        style.text_styles = [
            (TextStyle::Small, regular(11.5)),
            (TextStyle::Body, regular(13.5)),
            (TextStyle::Button, regular(13.0)),
            (TextStyle::Heading, display(20.0)),
            (TextStyle::Monospace, FontId::monospace(12.5)),
        ]
        .into();

        let s = &mut style.spacing;
        s.item_spacing = egui::vec2(8.0, 6.0);
        s.button_padding = egui::vec2(10.0, 5.0);
        s.interact_size.y = 26.0;
        s.menu_margin = Margin::same(6);
        s.window_margin = Margin::same(10);
        s.scroll.bar_width = 6.0;
        s.scroll.floating = true;
        s.scroll.floating_allocated_width = 0.0;
        s.scroll.bar_inner_margin = 2.0;
        s.scroll.bar_outer_margin = 2.0;
        s.tooltip_width = 420.0;

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.panel_fill = BG;
        v.window_fill = Color32::from_rgb(28, 30, 36);
        v.window_stroke = Stroke::new(1.0, Color32::from_rgb(44, 47, 56));
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(8);
        v.window_shadow = egui::Shadow {
            offset: [0, 6],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(110),
        };
        v.popup_shadow = v.window_shadow;
        v.extreme_bg_color = Color32::from_rgb(11, 12, 15);
        v.text_edit_bg_color = Some(Color32::from_rgb(20, 21, 26));
        v.faint_bg_color = SURFACE;
        v.hyperlink_color = ACCENT;
        v.selection.bg_fill = alpha(ACCENT, 0.35);
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.override_text_color = None;
        v.window_highlight_topmost = false;

        let r = CornerRadius::same(6);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = BG;
        w.noninteractive.weak_bg_fill = BG;
        w.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);
        w.noninteractive.corner_radius = r;

        w.inactive.bg_fill = SURFACE;
        w.inactive.weak_bg_fill = SURFACE;
        w.inactive.bg_stroke = Stroke::new(1.0, LINE);
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.corner_radius = r;

        w.hovered.bg_fill = SURFACE_HOVER;
        w.hovered.weak_bg_fill = SURFACE_HOVER;
        w.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(52, 56, 66));
        w.hovered.fg_stroke = Stroke::new(1.0, TEXT);
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;

        w.active.bg_fill = Color32::from_rgb(36, 39, 47);
        w.active.weak_bg_fill = Color32::from_rgb(36, 39, 47);
        w.active.bg_stroke = Stroke::new(1.0, alpha(ACCENT, 0.6));
        w.active.fg_stroke = Stroke::new(1.0, TEXT);
        w.active.corner_radius = r;
        w.active.expansion = 0.0;

        w.open = w.hovered;
    });
}

// ---------------------------------------------------------------------------------------------
// Text helpers

/// Lays out `text` on one line, cut with an ellipsis at `max_width`. Returns an empty galley for
/// a width too small to show anything useful.
pub fn line(
    ctx: &egui::Context,
    text: impl Into<String>,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text.into(), TextFormat::simple(font, color));
    job.wrap = TextWrapping {
        max_width: max_width.max(0.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    job.break_on_newline = false;
    ctx.fonts_mut(|f| f.layout_job(job))
}

/// Unbounded single line.
pub fn line_free(
    ctx: &egui::Context,
    text: impl Into<String>,
    font: FontId,
    color: Color32,
) -> Arc<Galley> {
    ctx.fonts_mut(|f| f.layout_no_wrap(text.into(), font, color))
}

pub fn text_width(ctx: &egui::Context, text: &str, font: &FontId) -> f32 {
    line_free(ctx, text.to_owned(), font.clone(), Color32::WHITE)
        .size()
        .x
}

/// Cuts `text` at the end with an ellipsis so it fits `max_width` (as a String, for composing).
pub fn truncate_end(ctx: &egui::Context, text: &str, font: &FontId, max_width: f32) -> String {
    if text_width(ctx, text, font) <= max_width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let build = |k: usize| -> String { chars[..k].iter().collect::<String>() + "…" };
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if text_width(ctx, &build(mid), font) <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    build(lo)
}

/// Cuts `text` to at most `max_chars` characters with an ellipsis in the middle, without
/// measuring: for strings that can be enormous, like paths thousands of folders deep.
pub fn shorten_chars(text: &str, max_chars: usize) -> String {
    let n = text.chars().count();
    if n <= max_chars {
        return text.to_owned();
    }
    let keep = max_chars.saturating_sub(1);
    let head = keep * 2 / 5;
    let tail = keep - head;
    let mut s: String = text.chars().take(head).collect();
    s.push('\u{2026}');
    s.extend(text.chars().skip(n - tail));
    s
}

/// Shortens a path in the middle (`C:\Users\…\Documents\projects`) so it fits `max_width`.
pub fn middle_ellipsis(ctx: &egui::Context, text: &str, font: &FontId, max_width: f32) -> String {
    // no glyph is narrower than ~2.5 px at UI sizes, so anything longer than this can't fit:
    // cut by count first and never lay out a huge string
    let text = &shorten_chars(text, (max_width / 2.5) as usize + 2);
    if text_width(ctx, text, font) <= max_width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    // keep `k` chars in total, split 40 % head / 60 % tail (the tail names the place)
    let build = |k: usize| -> String {
        let head = k * 2 / 5;
        let tail = k - head;
        let mut s: String = chars[..head].iter().collect();
        s.push('…');
        s.extend(chars[n - tail..].iter());
        s
    };
    let (mut lo, mut hi) = (0usize, n.saturating_sub(1));
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if text_width(ctx, &build(mid), font) <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 { String::new() } else { build(lo) }
}

/// Small caps-like section label: uppercase with a little letter spacing.
pub fn section_label(ctx: &egui::Context, text: &str, color: Color32) -> Arc<Galley> {
    let mut fmt = TextFormat::simple(semibold(10.5), color);
    fmt.extra_letter_spacing = 0.9;
    let job = LayoutJob::single_section(text.to_uppercase(), fmt);
    ctx.fonts_mut(|f| f.layout_job(job))
}

/// Both measures, the active one first: `12.4 GB on disk · 13.1 GB size`.
pub fn both_sizes(size: u64, other: u64, metric: crate::model::Metric) -> String {
    use crate::model::{Metric, format_size};
    match metric {
        Metric::OnDisk => format!("{} on disk  \u{b7}  {} size", format_size(size), format_size(other)),
        Metric::Logical => format!("{} size  \u{b7}  {} on disk", format_size(size), format_size(other)),
    }
}

/// Elapsed time as `0.4 s`, `41 s`, `3 min 12 s`.
pub fn format_duration(d: std::time::Duration) -> String {
    let s = d.as_secs_f64();
    if s < 10.0 {
        format!("{s:.1} s")
    } else if s < 60.0 {
        format!("{s:.0} s")
    } else {
        let m = (s / 60.0).floor();
        format!("{m:.0} min {:.0} s", s - m * 60.0)
    }
}

/// `3 min ago`, `just now`.
pub fn format_ago(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 45 {
        "just now".into()
    } else if s < 3600 {
        format!("{} min ago", (s + 30) / 60)
    } else if s < 86400 {
        format!("{} h ago", (s + 1800) / 3600)
    } else {
        format!("{} d ago", s / 86400)
    }
}

pub fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "–".into();
    }
    let p = part as f64 * 100.0 / whole as f64;
    if p >= 10.0 {
        format!("{p:.0}%")
    } else if p >= 0.1 {
        format!("{p:.1}%")
    } else if part > 0 {
        "<0.1%".into()
    } else {
        "0%".into()
    }
}
