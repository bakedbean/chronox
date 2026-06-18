//! ratatui rendering for the chronology UI. Maps `sessionx`'s neutral
//! `TokenKind`/`DiffLine` model to styled ratatui `Line`/`Span`, and renders
//! bar rows. Absorbed from chronox's `render.rs` — `sessionx` is
//! framework-agnostic, so this UI mapping lives in the consumer.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use sessionx::syntax::{
    CellKind, DiffCell, DiffLine, DiffMarker, LangSpec, Token, TokenKind, change_detail_diff,
};
use std::path::Path;

/// De-emphasised foreground grey for "inactive" header and nav text (folded
/// carets, timestamps, counts, unselected summaries). Bright enough to stay
/// legible on a dark terminal — `DarkGray` combined with `Modifier::DIM` was
/// too dark to read.
pub const INACTIVE_FG: Color = Color::Rgb(0x8a, 0x91, 0x9e);

fn style_for(kind: TokenKind) -> Style {
    match kind {
        TokenKind::Keyword => Style::default().fg(Color::Magenta),
        TokenKind::Str => Style::default().fg(Color::Yellow),
        TokenKind::Comment => Style::default().fg(Color::DarkGray),
        TokenKind::Number => Style::default().fg(Color::Cyan),
        TokenKind::Default => Style::default(),
    }
}

/// Count added vs removed lines for a change by running its bounded `detail`
/// through `change_detail_diff` and tallying markers. `base_line`/`lang` do not
/// affect counts, so we pass neutral values. Source A in the design (no I/O).
pub fn change_counts(detail: &sessionx::event::ChangeDetail) -> (u32, u32) {
    let mut add = 0;
    let mut del = 0;
    for dl in change_detail_diff(detail, 1, None) {
        match dl.marker {
            DiffMarker::Added => add += 1,
            DiffMarker::Removed => del += 1,
        }
    }
    (add, del)
}

/// Fixed-width magnitude bar: `add` green cells + `del` red cells in a
/// `width`-cell gauge, the remainder faint `▱`. Mirrors the design's `statBar`.
pub fn stat_bar(add: u32, del: u32, width: usize) -> Vec<Span<'static>> {
    let total = (add + del).max(1) as f64;
    let mut g = ((add as f64 / total) * width as f64).round() as usize;
    let mut r = ((del as f64 / total) * width as f64).round() as usize;
    if add > 0 && g == 0 {
        g = 1;
    }
    if del > 0 && r == 0 {
        r = 1;
    }
    while g + r > width {
        if r > g {
            r -= 1;
        } else {
            g -= 1;
        }
    }
    let empty = width - g - r;
    vec![
        Span::styled("▰".repeat(g), Style::default().fg(Color::Green)),
        Span::styled("▰".repeat(r), Style::default().fg(Color::Red)),
        Span::styled("▱".repeat(empty), Style::default().fg(Color::DarkGray)),
    ]
}

/// Pad a span list to `width` columns; when `selected`, fill the row with the
/// blue selection background. Char-based width (matches `clip_line_to_width`).
fn finish(mut spans: Vec<Span<'static>>, width: u16, selected: bool) -> Line<'static> {
    let width = width as usize;
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    if selected {
        let bg = Color::Rgb(0x24, 0x30, 0x49);
        for s in &mut spans {
            s.style = s.style.bg(bg);
        }
    }
    Line::from(spans)
}

/// A grouped file-header row: `<caret><path>[ new]<pad><gauge> +A[ -D]<pad><count>`.
#[allow(clippy::too_many_arguments)]
pub fn header_line(
    file_rel: &str,
    add: u32,
    del: u32,
    count: usize,
    is_new: bool,
    expanded: bool,
    active: bool,
    width: u16,
    selected: bool,
) -> Line<'static> {
    let dim = Style::default().fg(INACTIVE_FG);
    let (caret, caret_style) = if expanded {
        ("▾ ", Style::default().fg(Color::Cyan))
    } else {
        ("▸ ", dim)
    };
    let path_style = if active {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    // Right block: gauge + " " + "+A" [+ " -D"] + "  count".
    let mut right = stat_bar(add, del, 4);
    right.push(Span::raw(" "));
    right.push(Span::styled(
        format!("+{add}"),
        Style::default().fg(Color::Green),
    ));
    if del > 0 {
        right.push(Span::styled(
            format!(" -{del}"),
            Style::default().fg(Color::Red),
        ));
    }
    right.push(Span::styled(format!("  {count}"), dim));
    let right_len: usize = right.iter().map(|s| s.content.chars().count()).sum();

    let new_len = if is_new { 4 } else { 0 }; // " new"
    let caret_len = 2;
    let budget = (width as usize).saturating_sub(caret_len + new_len + right_len + 1);
    let path = abbreviate_path(file_rel, budget);

    let left_len = caret_len + path.chars().count() + new_len;
    let gap = (width as usize).saturating_sub(left_len + right_len).max(1);

    let mut spans = vec![
        Span::styled(caret, caret_style),
        Span::styled(path, path_style),
    ];
    if is_new {
        spans.push(Span::styled(" new", Style::default().fg(Color::Blue)));
    }
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    finish(spans, width, selected)
}

/// A nested edit row under the active file:
/// `  <connector> <HH:MM>  +a[ -d]  <summary>`.
pub fn edit_line(
    timestamp_ms: i64,
    add: u32,
    del: u32,
    summary: &str,
    last: bool,
    selected: bool,
    width: u16,
) -> Line<'static> {
    let faint = Style::default().fg(INACTIVE_FG);
    let dim = Style::default().fg(INACTIVE_FG);
    let connector = if last { "└ " } else { "├ " };
    let summary_style = if selected {
        Style::default().fg(Color::White)
    } else {
        dim
    };

    let mut spans = vec![
        Span::raw("  "),
        Span::styled(connector, faint),
        Span::styled(hhmm(timestamp_ms), dim),
        Span::raw("  "),
        Span::styled(format!("+{add}"), Style::default().fg(Color::Green)),
    ];
    if del > 0 {
        spans.push(Span::styled(
            format!(" -{del}"),
            Style::default().fg(Color::Red),
        ));
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled(summary.to_string(), summary_style));
    finish(spans, width, selected)
}

fn token_spans(code: &[Token]) -> Vec<Span<'static>> {
    code.iter()
        .map(|(t, k)| Span::styled(t.clone(), style_for(*k)))
        .collect()
}

fn diff_line_to_ratatui(dl: &DiffLine) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let (marker, marker_style) = match dl.marker {
        DiffMarker::Added => ("+ ", Style::default().fg(Color::Green)),
        DiffMarker::Removed => ("- ", Style::default().fg(Color::Red)),
    };
    let mut spans = vec![
        Span::styled(dl.gutter.clone(), dim),
        Span::styled(marker.to_string(), marker_style),
    ];
    spans.extend(token_spans(&dl.code));
    Line::from(spans)
}

/// Map one side-by-side cell to a styled line. `None` yields an empty line (a
/// blank column). Same gutter/marker/colour vocabulary as `diff_line_to_ratatui`.
pub fn side_cell_to_line(cell: Option<&DiffCell>) -> Line<'static> {
    let Some(c) = cell else {
        return Line::default();
    };
    let dim = Style::default().add_modifier(Modifier::DIM);
    let (marker, marker_style) = match c.kind {
        CellKind::Added => ("+ ", Style::default().fg(Color::Green)),
        CellKind::Removed => ("- ", Style::default().fg(Color::Red)),
        CellKind::Context => ("  ", Style::default()),
    };
    let mut spans = vec![
        Span::styled(c.gutter.clone(), dim),
        Span::styled(marker.to_string(), marker_style),
    ];
    spans.extend(token_spans(&c.code));
    Line::from(spans)
}

/// Build the modal's styled diff lines from a change. Same colours/gutter as the
/// in-`wsx` implementation it replaces.
pub fn change_detail_lines_styled(
    detail: &sessionx::event::ChangeDetail,
    base_line: u32,
    lang: Option<&LangSpec>,
) -> Vec<Line<'static>> {
    change_detail_diff(detail, base_line, lang)
        .iter()
        .map(diff_line_to_ratatui)
        .collect()
}

/// Truncate a styled `Line` to `width` display columns (char-based), preserving
/// span styles; the boundary span is trimmed.
pub fn clip_line_to_width(line: &Line<'static>, width: usize) -> Line<'static> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in &line.spans {
        if used >= width {
            break;
        }
        let remaining = width - used;
        let cnt = span.content.chars().count();
        if cnt <= remaining {
            out.push(span.clone());
            used += cnt;
        } else {
            let truncated: String = span.content.chars().take(remaining).collect();
            out.push(Span::styled(truncated, span.style));
            break;
        }
    }
    Line::from(out)
}

// ── display helpers from chronology_bar.rs ───────────────────────────────────

/// Worktree-relative display path, falling back to the full path when the file
/// is not under the worktree.
pub fn relative_display(file: &Path, worktree: &Path) -> String {
    match file.strip_prefix(worktree) {
        Ok(rel) => rel.to_string_lossy().to_string(),
        Err(_) => file.to_string_lossy().to_string(),
    }
}

/// Front-truncate `s` to `max` columns with a leading `…` so the tail (the
/// filename) stays visible. Counts characters, not bytes.
fn ellipsize_start(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let tail: String = s.chars().skip(n - (max - 1)).collect();
    format!("…{tail}")
}

/// Fit a worktree-relative path into `max` columns. If it already fits, return
/// it unchanged. Otherwise abbreviate each ancestor directory (everything
/// before the parent directory) to its first character, keeping the parent
/// directory and filename intact (e.g. `docs/superpowers/specs/foo.md` →
/// `d/s/specs/foo.md`). If still too wide, front-truncate with `…`.
fn abbreviate_path(rel: &str, max: usize) -> String {
    if rel.chars().count() <= max {
        return rel.to_string();
    }
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() > 2 {
        let last = parts.len() - 1;
        let mut out = String::new();
        for (i, p) in parts.iter().enumerate() {
            if i > 0 {
                out.push('/');
            }
            // Ancestors (everything before the parent dir) collapse to their
            // first character; the parent dir and filename are kept whole.
            if i + 2 <= last {
                if let Some(c) = p.chars().next() {
                    out.push(c);
                }
            } else {
                out.push_str(p);
            }
        }
        if out.chars().count() <= max {
            return out;
        }
        return ellipsize_start(&out, max);
    }
    ellipsize_start(rel, max)
}

pub fn hhmm(timestamp_ms: i64) -> String {
    // Wall-clock HH:MM in the machine's local timezone (DST-aware), derived from
    // epoch ms. `--:--` only if the timestamp falls outside chrono's range.
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(timestamp_ms)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};
    use sessionx::event::ChangeDetail;
    use std::path::Path;

    #[test]
    fn styled_lines_preserve_colours_and_gutter() {
        let detail = ChangeDetail::Edit {
            old: "old".into(),
            new: "let y = 1".into(),
        };
        let lines = change_detail_lines_styled(
            &detail,
            7,
            sessionx::syntax::lang_for_path(Path::new("a.rs")),
        );
        // removed line: dim 5-space gutter, red "- " marker
        assert_eq!(lines[0].spans[0].content.as_ref(), "     ");
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(lines[0].spans[1].content.as_ref(), "- ");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Red));
        // added line: gutter "   7 ", green "+ ", "let" highlighted magenta
        assert_eq!(lines[1].spans[0].content.as_ref(), "   7 ");
        assert_eq!(lines[1].spans[1].content.as_ref(), "+ ");
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Green));
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|s| s.content.as_ref() == "let" && s.style.fg == Some(Color::Magenta))
        );
    }

    #[test]
    fn no_lang_is_plain_code_span() {
        let detail = ChangeDetail::Write {
            head: "let y = 1".into(),
        };
        let lines = change_detail_lines_styled(&detail, 1, None);
        assert_eq!(lines[0].spans[2].content.as_ref(), "let y = 1");
        assert_eq!(lines[0].spans[2].style.fg, None);
    }

    #[test]
    fn clip_line_preserves_styles_and_truncates() {
        let detail = ChangeDetail::Write {
            head: "abcdefgh".into(),
        };
        let line = &change_detail_lines_styled(&detail, 1, None)[0]; // "   1 + abcdefgh"
        let clipped = clip_line_to_width(line, 7);
        let text: String = clipped.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "   1 + ");
        assert_eq!(clip_line_to_width(line, 0).spans.len(), 0);
        let wide = clip_line_to_width(line, 999);
        let wide_text: String = wide.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(wide_text, "   1 + abcdefgh");
    }

    #[test]
    fn relative_path_strips_worktree_prefix() {
        let p = relative_display(Path::new("/wt/src/main.rs"), Path::new("/wt"));
        assert_eq!(p, "src/main.rs");
    }

    #[test]
    fn relative_path_passthrough_when_not_prefixed() {
        let p = relative_display(Path::new("/other/x.rs"), Path::new("/wt"));
        assert_eq!(p, "/other/x.rs");
    }

    #[test]
    fn hhmm_renders_local_clock_in_hh_colon_mm() {
        // Timezone-agnostic: whatever the test machine's zone, the output is a
        // valid HH:MM string (no panic, no UTC-only assumption).
        let s = hhmm(1_700_000_000_000);
        assert_eq!(s.len(), 5, "HH:MM is five chars");
        assert_eq!(&s[2..3], ":", "colon separator at index 2");
        assert!(
            s[..2]
                .bytes()
                .chain(s[3..].bytes())
                .all(|b| b.is_ascii_digit()),
            "hours and minutes are digits: {s}"
        );
        let (h, m) = (
            s[..2].parse::<u32>().unwrap(),
            s[3..].parse::<u32>().unwrap(),
        );
        assert!(h < 24 && m < 60, "valid wall-clock values: {s}");
    }

    #[test]
    fn abbreviate_keeps_short_paths_whole() {
        assert_eq!(abbreviate_path("src/main.rs", 40), "src/main.rs");
    }

    #[test]
    fn abbreviate_collapses_ancestors_keeping_parent_and_file() {
        let out = abbreviate_path("src/ui/widgets/chronology_bar.rs", 30);
        assert_eq!(out, "s/u/widgets/chronology_bar.rs");
    }

    #[test]
    fn abbreviate_front_truncates_when_still_too_long() {
        let out = abbreviate_path("docs/superpowers/specs/2026-06-05-foo.md", 15);
        assert!(out.chars().count() <= 15, "fits within max");
        assert!(out.starts_with('…'), "front-truncated");
        assert!(out.ends_with("foo.md"), "filename tail preserved");
    }

    #[test]
    fn abbreviate_parent_and_file_only_front_truncates() {
        let out = abbreviate_path("widgets/chronology_bar.rs", 12);
        assert!(out.chars().count() <= 12);
        assert!(out.ends_with(".rs"));
    }

    #[test]
    fn stat_bar_splits_green_red_and_pads_empty() {
        // all adds -> 4 green, 0 red, 0 empty
        let b = stat_bar(10, 0, 4);
        assert_eq!(b[0].content.as_ref(), "▰▰▰▰");
        assert_eq!(b[0].style.fg, Some(Color::Green));
        assert_eq!(b[1].content.as_ref(), "");
        assert_eq!(b[2].content.as_ref(), "");

        // mixed -> at least one of each, total width 4
        let b = stat_bar(3, 1, 4);
        let g = b[0].content.chars().count();
        let r = b[1].content.chars().count();
        let e = b[2].content.chars().count();
        assert_eq!(g + r + e, 4);
        assert!(g >= 1 && r >= 1, "both sides represented when both nonzero");
        assert_eq!(b[1].style.fg, Some(Color::Red));
        assert_eq!(b[2].style.fg, Some(Color::DarkGray));

        // nothing -> all empty/faint
        let b = stat_bar(0, 0, 4);
        assert_eq!(b[2].content.chars().count(), 4);
    }

    #[test]
    fn change_counts_counts_added_and_removed() {
        use sessionx::event::ChangeDetail;
        assert_eq!(
            change_counts(&ChangeDetail::Edit {
                old: "a\nb".into(),
                new: "x".into()
            }),
            (1, 2),
            "1 added line, 2 removed lines"
        );
        assert_eq!(
            change_counts(&ChangeDetail::Write {
                head: "a\nb\nc".into()
            }),
            (3, 0)
        );
        assert_eq!(change_counts(&ChangeDetail::None), (0, 0));
    }

    #[test]
    fn header_line_has_caret_path_gauge_and_counts() {
        let line = header_line("src/app.rs", 16, 3, 2, false, true, true, 44, false);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("▾ src/app.rs"), "expanded caret + path");
        assert!(text.contains("+16"));
        assert!(text.contains("-3"));
        assert!(text.trim_end().ends_with("2"), "edit count right-aligned");
        assert_eq!(
            line.spans[0].style.fg,
            Some(Color::Cyan),
            "expanded caret cyan"
        );
    }

    #[test]
    fn folded_header_uses_folded_caret_and_no_del_when_zero() {
        let line = header_line("Cargo.toml", 1, 0, 1, false, false, false, 44, false);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("▸ Cargo.toml"), "folded caret");
        assert!(text.contains("+1"));
        assert!(!text.contains("-0"), "zero removals omitted");
    }

    #[test]
    fn new_file_header_shows_new_tag() {
        let line = header_line("src/theme.rs", 58, 0, 1, true, false, false, 44, false);
        let new = line.spans.iter().find(|s| s.content.as_ref() == " new");
        assert!(new.is_some(), "single Write shows ' new'");
        assert_eq!(new.unwrap().style.fg, Some(Color::Blue));
    }

    #[test]
    fn edit_line_connector_time_stats_and_summary() {
        let line = edit_line(0, 12, 3, "guard repin()", false, false, 44);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        // Timezone-agnostic: the time is rendered in local time, so derive the
        // expected HH:MM from hhmm rather than assuming UTC.
        assert!(
            text.starts_with(&format!("  ├ {}", hhmm(0))),
            "indent, branch connector, time"
        );
        assert!(text.contains("+12"));
        assert!(text.contains("-3"));
        assert!(text.contains("guard repin()"));
    }

    #[test]
    fn last_edit_uses_corner_connector_and_selection_brightens() {
        let line = edit_line(0, 4, 0, "cache rows", true, true, 44);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("└ "), "last edit uses corner connector");
        assert!(!text.contains("-0"), "zero removals omitted");
        // selected: every span carries the blue selection background.
        let bg = ratatui::style::Color::Rgb(0x24, 0x30, 0x49);
        assert!(
            line.spans.iter().all(|s| s.style.bg == Some(bg)),
            "selection bar fills the row"
        );
        // summary brightened to White when selected.
        let sum = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "cache rows")
            .unwrap();
        assert_eq!(sum.style.fg, Some(Color::White));
    }

    #[test]
    fn side_cell_styles_marker_gutter_and_none() {
        use sessionx::syntax::{CellKind, DiffCell, change_detail_side_by_side};
        let detail = ChangeDetail::Edit {
            old: "a".into(),
            new: "let y = 1".into(),
        };
        let rows = change_detail_side_by_side(
            &detail,
            4,
            sessionx::syntax::lang_for_path(Path::new("a.rs")),
        );
        // removed cell on the left: dim gutter, red "- " marker
        let left = side_cell_to_line(rows[0].left.as_ref());
        assert_eq!(left.spans[0].content.as_ref(), "     ");
        assert!(left.spans[0].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(left.spans[1].content.as_ref(), "- ");
        assert_eq!(left.spans[1].style.fg, Some(Color::Red));
        // added cell on the right: gutter "   4 ", green "+ ", "let" highlighted
        let right = side_cell_to_line(rows[0].right.as_ref());
        assert_eq!(right.spans[0].content.as_ref(), "   4 ");
        assert_eq!(right.spans[1].content.as_ref(), "+ ");
        assert_eq!(right.spans[1].style.fg, Some(Color::Green));
        assert!(
            right
                .spans
                .iter()
                .any(|s| s.content.as_ref() == "let" && s.style.fg == Some(Color::Magenta))
        );
        // a context cell uses a blank "  " marker with no colour
        let ctx = DiffCell {
            gutter: "   9 ".to_string(),
            kind: CellKind::Context,
            code: vec![],
        };
        let ctx_line = side_cell_to_line(Some(&ctx));
        assert_eq!(ctx_line.spans[1].content.as_ref(), "  ");
        assert_eq!(ctx_line.spans[1].style.fg, None);
        // None -> an empty line (blank column)
        assert!(side_cell_to_line(None).spans.is_empty());
    }
}
