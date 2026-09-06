//! `shitty-vt` implementation of `VtEngine`: the shitty terminal core, a C++
//! VT engine, reached through its C facade and the `shitty-vt` crate.
//!
//! Opt-in (`--features shitty-engine`) because it links a library that has to
//! exist at build time, which `cargo install luvus` cannot assume.
//!
//! Every method of the trait now has something behind it. The two things the
//! facade used to resolve away - what a cell's colours were asked for, and
//! where a row wrapped - were found by building this and landed upstream as
//! pg83/shitty#112 and #114.

use std::sync::mpsc::Sender;

use ratatui::style::{Color, Modifier};
use shitty_vt::{Cell as VtCell, ColorSource, Rgb, Terminal};

use super::{CodexComposerRegion, Cursor, HistoryMetrics, RenderCell, VtEngine};
use crate::terminal::backend::{CaptureMode, CaptureResult};
use crate::terminal::pty::InputAction;

/// Cell size assumed before the terminal exists to report its own. Corrected
/// from [`shitty_vt::Memory::cell_size`] immediately after construction.
const ASSUMED_CELL_BYTES: usize = 16;

pub struct ShittyEngine {
    term: Terminal,
    resp_tx: Sender<InputAction>,
    cols: u16,
    rows: u16,
    history_budget_bytes: usize,
    output_generation: u64,
}

impl ShittyEngine {
    pub fn new(
        cols: u16,
        rows: u16,
        resp_tx: Sender<InputAction>,
        history_budget_bytes: usize,
    ) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut term = Terminal::new(
            cols,
            rows,
            save_lines_for_budget(history_budget_bytes, cols, ASSUMED_CELL_BYTES),
        );
        // The terminal knows its real cell size; re-apply the budget with it
        // rather than keeping an estimate the engine never needed.
        let cell_size = term.memory_usage().cell_size as usize;
        term.set_save_lines(save_lines_for_budget(history_budget_bytes, cols, cell_size));

        ShittyEngine {
            term,
            resp_tx,
            cols,
            rows,
            history_budget_bytes,
            output_generation: 0,
        }
    }

    /// Hands the terminal's own replies (cursor reports, device attributes,
    /// OSC answers) back to the child, the way alacritty's event proxy does.
    fn drain_replies(&mut self) {
        let replies = self.term.take_replies();
        if !replies.is_empty() {
            let _ = self.resp_tx.send(InputAction::Bytes(replies));
        }
    }

    fn cell_size(&self) -> usize {
        (self.term.memory_usage().cell_size as usize).max(1)
    }

    fn apply_history_budget(&mut self) {
        let save_lines =
            save_lines_for_budget(self.history_budget_bytes, self.cols, self.cell_size());
        self.term.set_save_lines(save_lines);
    }

    /// Absolute row index of the top of the live screen in [`Terminal::row_cells`]
    /// coordinates, which run oldest-first over history then the visible grid.
    fn live_top(&self) -> u32 {
        self.term.total_rows().saturating_sub(self.rows as u32)
    }

    /// One row's text, wide-cell continuations skipped, clusters kept whole.
    ///
    /// `limit` stops at that column, which is how a row that wrapped gives
    /// up only the part belonging to it; 0 takes the whole row.
    fn row_text(&self, index: u32, keep_clusters: bool, limit: u16, out: &mut String) {
        out.clear();
        self.term.row_cells(index, |_, column, cell| {
            if limit != 0 && column >= limit {
                return;
            }
            if cell.grapheme.is_empty() {
                out.push(' ');
                return;
            }
            let mut points = cell.grapheme.iter().filter_map(|p| char::from_u32(*p));
            if let Some(first) = points.next() {
                out.push(first);
            }
            if keep_clusters {
                out.extend(points);
            }
        });
    }

    /// The newest `lines` logical lines, oldest first, each as the physical
    /// rows it was written across.
    ///
    /// A row with a non-zero wrap length continues into the next one, so a
    /// line starts wherever the row above it ended.
    fn logical_lines(&self, lines: usize) -> Vec<Vec<(u32, u16)>> {
        let count = self.term.total_rows();
        let mut logical: Vec<Vec<(u32, u16)>> = Vec::new();
        let mut current: Vec<(u32, u16)> = Vec::new();
        // Walking backwards, each row's own wrap length is the one read to
        // decide whether the row after it continued, so carry it along
        // rather than asking the terminal for it twice.
        let mut wrap = 0;
        for index in (0..count).rev() {
            current.push((index, wrap));
            wrap = if index == 0 {
                0
            } else {
                self.term.row_wrap_length(index - 1)
            };
            if wrap == 0 {
                current.reverse();
                logical.push(std::mem::take(&mut current));
                if logical.len() >= lines {
                    break;
                }
            }
        }
        logical.reverse();
        logical
    }

    /// Appends one row. `wrap` is the row's wrap length: non-zero means it
    /// continues onto the next, so its text stops there and keeps its
    /// blanks - the row is full by definition, and trimming would eat a
    /// space the application printed. A row that ends on its own is
    /// trimmed as before.
    fn append_plain_row(
        &self,
        index: u32,
        wrap: u16,
        output: &mut String,
        max_bytes: usize,
    ) -> bool {
        let mut row = String::with_capacity(self.cols as usize);
        self.row_text(index, true, wrap, &mut row);
        let text = if wrap == 0 {
            row.trim_end()
        } else {
            row.as_str()
        };
        append_utf8_bounded(output, text, max_bytes)
    }

    fn append_ansi_row(
        &self,
        index: u32,
        wrap: u16,
        output: &mut String,
        max_bytes: usize,
    ) -> bool {
        let mut styled: Vec<(String, Color, Color, Modifier)> = Vec::new();
        self.term.row_cells(index, |_, column, cell| {
            if wrap != 0 && column >= wrap {
                return;
            }
            styled.push((
                cluster_text(&cell),
                foreground(&cell),
                background(&cell),
                modifiers(&cell),
            ));
        });
        let blank = (Color::Reset, Color::Reset, Modifier::empty());
        // A wrapped row keeps every cell it owns; only a row that ends on
        // its own gives up its trailing blanks.
        let last = if wrap != 0 {
            styled.len()
        } else {
            styled
                .iter()
                .rposition(|(text, fg, bg, m)| !text.trim().is_empty() || (*fg, *bg, *m) != blank)
                .map_or(0, |index| index + 1)
        };

        let mut style = blank;
        // Always reserve room to reset a style we emit.
        let content_limit = max_bytes.saturating_sub(4);
        for (text, fg, bg, m) in styled.iter().take(last) {
            let next = (*fg, *bg, *m);
            let code = (next != style).then(|| sgr(next.0, next.1, next.2));
            let needed = code.as_ref().map_or(0, String::len) + text.len();
            if output.len().saturating_add(needed) > content_limit {
                if style != blank {
                    output.push_str("\x1b[0m");
                }
                return false;
            }
            if let Some(code) = code {
                output.push_str(&code);
                style = next;
            }
            output.push_str(text);
        }
        if style != blank {
            output.push_str("\x1b[0m");
        }
        true
    }
}

/// The cell's foreground as the application asked for it.
fn foreground(cell: &VtCell<'_>) -> Color {
    map_color(cell.foreground_source, cell.foreground)
}

fn background(cell: &VtCell<'_>) -> Color {
    map_color(cell.background_source, cell.background)
}

fn save_lines_for_budget(bytes: usize, cols: u16, cell_size: usize) -> u16 {
    let row_bytes = (cols.max(1) as usize).saturating_mul(cell_size.max(1));
    bytes
        .saturating_div(row_bytes.max(1))
        .clamp(1, u16::MAX as usize) as u16
}

/// A colour request as ratatui says it, so the host terminal keeps its theme.
///
/// Luvus draws inside someone else's terminal. A cell that asked for the
/// default has to stay `Color::Reset` and one that asked for ANSI red has to
/// stay `Color::Indexed(1)`, or every pane comes out painted in shitty's
/// palette rather than the user's. The resolved RGB beside the source is what
/// shitty would have drawn with its own configuration and is exactly what must
/// not be forwarded - except for `Direct`, which is a colour the application
/// named itself and has no palette to lose.
fn map_color(source: ColorSource, resolved: Rgb) -> Color {
    match source {
        ColorSource::DefaultForeground | ColorSource::DefaultBackground => Color::Reset,
        ColorSource::Indexed(entry) => Color::Indexed(entry),
        ColorSource::Direct => Color::Rgb(resolved.r, resolved.g, resolved.b),
    }
}

fn cluster_text(cell: &VtCell<'_>) -> String {
    if cell.grapheme.is_empty() {
        return String::from(" ");
    }
    cell.grapheme
        .iter()
        .filter_map(|p| char::from_u32(*p))
        .filter(|c| !c.is_control() || *c == '\t')
        .collect()
}

fn modifiers(cell: &VtCell<'_>) -> Modifier {
    let attributes = cell.attributes;
    let mut m = Modifier::empty();
    if attributes.bold() {
        m |= Modifier::BOLD;
    }
    if attributes.italic() {
        m |= Modifier::ITALIC;
    }
    if attributes.faint() {
        m |= Modifier::DIM;
    }
    if attributes.inverse() {
        m |= Modifier::REVERSED;
    }
    if attributes.conceal() {
        m |= Modifier::HIDDEN;
    }
    if attributes.strike() {
        m |= Modifier::CROSSED_OUT;
    }
    // Underline is a style, not a flag, in this model: any of the five shapes
    // is an underline as far as ratatui is concerned.
    if cell.underline_style != 0 {
        m |= Modifier::UNDERLINED;
    }
    m
}

fn append_utf8_bounded(output: &mut String, text: &str, max_bytes: usize) -> bool {
    if output.len().saturating_add(text.len()) <= max_bytes {
        output.push_str(text);
        return true;
    }
    let remaining = max_bytes.saturating_sub(output.len());
    let mut end = remaining.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    output.push_str(&text[..end]);
    false
}

fn sgr(fg: Color, bg: Color, m: Modifier) -> String {
    let mut s = String::from("\x1b[0");
    if m.contains(Modifier::BOLD) {
        s.push_str(";1");
    }
    if m.contains(Modifier::DIM) {
        s.push_str(";2");
    }
    if m.contains(Modifier::ITALIC) {
        s.push_str(";3");
    }
    if m.contains(Modifier::UNDERLINED) {
        s.push_str(";4");
    }
    if m.contains(Modifier::REVERSED) {
        s.push_str(";7");
    }
    push_color(&mut s, fg, 38);
    push_color(&mut s, bg, 48);
    s.push('m');
    s
}

fn push_color(s: &mut String, c: Color, base: u8) {
    match c {
        Color::Indexed(i) => s.push_str(&format!(";{base};5;{i}")),
        Color::Rgb(r, g, b) => s.push_str(&format!(";{base};2;{r};{g};{b}")),
        _ => {}
    }
}

impl VtEngine for ShittyEngine {
    fn advance(&mut self, bytes: &[u8]) {
        self.term.feed(bytes);
        self.drain_replies();
        self.output_generation = self.output_generation.wrapping_add(1);
    }

    fn finish_output_batch(&mut self) {
        // The core does its own allocation maintenance inside `feed`; the
        // frame boundary only needs the damage flag cleared.
        self.term.clear_damage();
    }

    fn output_generation(&self) -> u64 {
        self.output_generation
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        self.term.resize(self.cols, self.rows);
        self.apply_history_budget();
    }

    fn cursor(&self) -> Cursor {
        let cursor = self.term.cursor();
        Cursor {
            x: cursor.column,
            y: cursor.row,
            // Scrolled into history: the live cursor is not in view, so hide
            // it rather than draw it over an old line.
            visible: cursor.visible && self.term.scroll_offset() == 0,
        }
    }

    fn codex_composer_region(&self) -> Option<CodexComposerRegion> {
        if self.term.scroll_offset() != 0 {
            return None;
        }
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        if rows < 3 || cols < 4 {
            return None;
        }
        let cursor = self.term.cursor().row as usize;
        if cursor >= rows {
            return None;
        }

        let mut buffer = String::with_capacity(cols);
        let top_row = self.live_top();
        let text: Vec<String> = (0..rows)
            .map(|row| {
                self.row_text(top_row + row as u32, false, 0, &mut buffer);
                buffer.clone()
            })
            .collect();
        let row_is_blank = |row: usize| text[row].trim().is_empty();
        let row_has_prompt = |row: usize| {
            text[row]
                .chars()
                .take(3)
                .any(|character| character == '\u{203a}')
        };

        let prompt = (cursor.saturating_sub(8)..=cursor)
            .rev()
            .find(|&row| row_has_prompt(row))?;
        let top = prompt.checked_sub(1)?;
        if !row_is_blank(top) || (prompt..=cursor).any(row_is_blank) {
            return None;
        }
        let bottom_limit = (cursor + 8).min(rows - 1);
        let bottom = ((cursor + 1)..=bottom_limit).find(|&row| row_is_blank(row))?;
        Some(CodexComposerRegion {
            top: top as u16,
            bottom: bottom as u16,
        })
    }

    fn for_each_cell(&self, f: &mut dyn FnMut(u16, u16, &str, RenderCell)) {
        // The overwhelmingly common cell is one character with no combining
        // marks; keep it off the heap the way the alacritty engine does.
        let mut stack = [0u8; 4];
        let mut combined = String::new();
        self.term.for_each_cell(|row, column, cell| {
            let symbol: &str = match cell.grapheme {
                [] => " ",
                [single] => match char::from_u32(*single) {
                    Some(character) => character.encode_utf8(&mut stack),
                    None => " ",
                },
                points => {
                    combined.clear();
                    combined.extend(points.iter().filter_map(|p| char::from_u32(*p)));
                    &combined
                }
            };
            f(
                row,
                column,
                symbol,
                RenderCell {
                    fg: foreground(&cell),
                    bg: background(&cell),
                    mods: modifiers(&cell),
                },
            );
        });
    }

    fn detection_text(&self, n: u16) -> String {
        // Read the live screen by absolute row index rather than the visible
        // grid: agent state must describe what the agent is doing now, not
        // whatever the user has scrolled to.
        let rows = self.rows as usize;
        let start = rows.saturating_sub(n as usize);
        let top = self.live_top();
        let mut out = String::new();
        let mut buffer = String::with_capacity(self.cols as usize);
        for row in start..rows {
            self.row_text(top + row as u32, true, 0, &mut buffer);
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(buffer.trim_end());
        }
        out
    }

    fn visible_rows(&self) -> Vec<String> {
        let mut lines = vec![String::new(); self.rows as usize];
        self.term.for_each_cell(|row, _, cell| {
            let Some(line) = lines.get_mut(row as usize) else {
                return;
            };
            match cell.grapheme.first().and_then(|p| char::from_u32(*p)) {
                Some(character) => line.push(character),
                None => line.push(' '),
            }
        });
        lines
    }

    fn backend_capture(
        &self,
        mode: CaptureMode,
        lines: usize,
        ansi: bool,
        max_bytes: usize,
    ) -> CaptureResult {
        let lines = lines.max(1);
        let rows = self.rows as usize;
        let top = self.live_top();
        let mut output = String::new();
        let mut returned = 0;
        let mut truncated = false;

        match mode {
            CaptureMode::Detection | CaptureMode::Visible => {
                let plain = ansi && mode == CaptureMode::Visible;
                let start = rows.saturating_sub(lines);
                for row in start..rows {
                    if returned > 0 && !append_utf8_bounded(&mut output, "\n", max_bytes) {
                        truncated = true;
                        break;
                    }
                    let index = top + row as u32;
                    let complete = if plain {
                        self.append_ansi_row(index, 0, &mut output, max_bytes)
                    } else {
                        self.append_plain_row(index, 0, &mut output, max_bytes)
                    };
                    returned += 1;
                    if !complete {
                        truncated = true;
                        break;
                    }
                }
            }
            CaptureMode::RecentUnwrapped => {
                for rows in self.logical_lines(lines) {
                    if returned > 0 && !append_utf8_bounded(&mut output, "\n", max_bytes) {
                        truncated = true;
                        break;
                    }
                    // One logical line, however many rows the terminal
                    // happened to split it across.
                    let mut complete = true;
                    for (index, wrap) in rows {
                        complete = if ansi {
                            self.append_ansi_row(index, wrap, &mut output, max_bytes)
                        } else {
                            self.append_plain_row(index, wrap, &mut output, max_bytes)
                        };
                        if !complete {
                            break;
                        }
                    }
                    returned += 1;
                    if !complete {
                        truncated = true;
                        break;
                    }
                }
            }
        }
        CaptureResult {
            text: output,
            lines: returned,
            truncated,
        }
    }

    fn title(&self) -> Option<String> {
        self.term.title()
    }

    fn set_history_budget(&mut self, bytes: usize) {
        self.history_budget_bytes = bytes;
        self.apply_history_budget();
    }

    fn scroll(&mut self, delta: i32) {
        self.term.scroll(delta);
    }

    fn scroll_to_top(&mut self) {
        let history = self.term.history_rows();
        self.term.scroll_to(history);
    }

    fn scroll_to_bottom(&mut self) {
        self.term.scroll_to(0);
    }

    fn scroll_offset(&self) -> usize {
        self.term.scroll_offset() as usize
    }

    fn history_len(&self) -> usize {
        self.term.history_rows() as usize
    }

    fn history_metrics(&self) -> HistoryMetrics {
        let memory = self.term.memory_usage();
        let retained_rows = self.history_len();
        let cell_size = (memory.cell_size as usize).max(1);
        HistoryMetrics {
            offset: self.scroll_offset(),
            retained_rows,
            budget_bytes: self.history_budget_bytes,
            retained_bytes: retained_rows
                .saturating_mul(self.cols as usize)
                .saturating_mul(cell_size),
            estimated_grid_bytes: memory.cell_bytes as usize,
            cache_bytes: None,
            compacted_rows: None,
            allocated_cells: Some(
                (memory.allocated_rows as usize).saturating_mul(memory.columns as usize),
            ),
            // Cells only: clusters, hyperlinks and sixel patches live in a
            // store this figure does not count.
            exact_bytes: false,
        }
    }

    fn retained_row_count(&self) -> usize {
        self.term.total_rows() as usize
    }

    fn retained_row_text(&self, index: usize) -> Option<String> {
        let index = u32::try_from(index).ok()?;
        if index >= self.term.total_rows() {
            return None;
        }
        let mut output = String::with_capacity(self.cols as usize);
        self.row_text(index, true, 0, &mut output);
        let trimmed = output.trim_end().len();
        output.truncate(trimmed);
        Some(output)
    }

    fn for_each_retained_row(&self, f: &mut dyn FnMut(usize, &str)) {
        let mut output = String::with_capacity(self.cols as usize);
        for index in 0..self.term.total_rows() {
            self.row_text(index, true, 0, &mut output);
            let trimmed = output.trim_end().len();
            output.truncate(trimmed);
            f(index as usize, &output);
        }
    }

    fn scroll_to(&mut self, offset: usize) {
        self.term.scroll_to(offset.min(u32::MAX as usize) as u32);
    }

    fn alt_screen(&self) -> bool {
        self.term.modes().alt_screen()
    }

    fn mouse_report(&self) -> bool {
        let modes = self.term.modes();
        modes.mouse_click() || modes.mouse_drag() || modes.mouse_motion()
    }

    fn alternate_scroll(&self) -> bool {
        self.term.modes().alternate_scroll()
    }

    fn application_cursor(&self) -> bool {
        self.term.modes().app_cursor_keys()
    }

    fn mouse_drag(&self) -> bool {
        let modes = self.term.modes();
        modes.mouse_drag() || modes.mouse_motion()
    }

    fn mouse_motion(&self) -> bool {
        self.term.modes().mouse_motion()
    }

    fn sgr_mouse(&self) -> bool {
        self.term.modes().mouse_sgr()
    }

    fn bracketed_paste(&self) -> bool {
        self.term.modes().bracketed_paste()
    }

    fn snapshot_ansi(&self) -> String {
        let rows = self.rows as usize;
        if rows == 0 || self.cols == 0 {
            return String::new();
        }
        let mut body: Vec<String> = Vec::with_capacity(rows);
        let top = self.live_top();
        for row in 0..rows {
            let mut line = String::new();
            self.append_ansi_row(top + row as u32, 0, &mut line, usize::MAX);
            body.push(line);
        }
        let Some(last) = body.iter().rposition(|line| !line.is_empty()) else {
            return String::from("\x1b[2J\x1b[H");
        };
        let mut out = String::from("\x1b[2J\x1b[H");
        for (index, line) in body.iter().take(last + 1).enumerate() {
            out.push_str(line);
            if index < last {
                out.push_str("\r\n");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn engine(cols: u16, rows: u16) -> ShittyEngine {
        let (tx, rx) = mpsc::channel();
        // Keep the receiver alive for the engine's lifetime: replies are sent,
        // not dropped, and a closed channel would hide a send failure.
        std::mem::forget(rx);
        ShittyEngine::new(cols, rows, tx, 256 * 1024)
    }

    /// The rendered cell at `column` of the top row.
    fn render_cell(engine: &ShittyEngine, column: u16) -> RenderCell {
        let mut found = None;
        engine.for_each_cell(&mut |row, at, _, cell| {
            if row == 0 && at == column {
                found = Some(cell);
            }
        });
        found.expect("the cell just written should be visited")
    }

    fn feed_lines(engine: &mut ShittyEngine, count: usize) {
        for index in 0..count {
            engine.advance(format!("line{index}\r\n").as_bytes());
        }
    }

    #[test]
    fn plain_text_inherits_the_host_theme() {
        // Nothing asked for a colour, so nothing may be sent: `Color::Reset`
        // leaves the host terminal painting its own text. Reporting shitty's
        // resolved white here is what painted every unstyled character white
        // in a real session while the whole suite stayed green.
        let mut engine = engine(20, 3);
        engine.advance(b"hi");

        let cell = render_cell(&engine, 0);
        assert_eq!(cell.fg, Color::Reset);
        assert_eq!(cell.bg, Color::Reset);
    }

    #[test]
    fn an_ansi_request_stays_a_palette_index() {
        // The host's palette, not shitty's: a user whose red is not
        // 0xaa0000 gets their own.
        let mut engine = engine(20, 3);
        engine.advance(b"\x1b[31;44mr");

        let cell = render_cell(&engine, 0);
        assert_eq!(cell.fg, Color::Indexed(1));
        assert_eq!(cell.bg, Color::Indexed(4));
    }

    #[test]
    fn a_true_color_request_keeps_its_value() {
        // Named outright, so there is no palette to lose and the value is
        // forwarded as it arrived.
        let mut engine = engine(20, 3);
        engine.advance(b"\x1b[38;2;1;2;3mx");

        assert_eq!(render_cell(&engine, 0).fg, Color::Rgb(1, 2, 3));
    }

    #[test]
    fn a_redefined_palette_entry_stays_an_index() {
        // OSC 4 moves entry 1 in shitty's palette. What the application
        // asked for did not move, and the host has a palette of its own to
        // resolve it against, so the index is what travels.
        let mut engine = engine(20, 3);
        engine.advance(b"\x1b]4;1;rgb:00/00/ff\x07\x1b[31mr");

        assert_eq!(render_cell(&engine, 0).fg, Color::Indexed(1));
    }

    #[test]
    fn text_lands_on_the_grid_with_the_cursor_after_it() {
        let mut engine = engine(20, 3);
        engine.advance(b"hi");
        assert_eq!(engine.visible_rows()[0].trim_end(), "hi");
        let cursor = engine.cursor();
        assert_eq!((cursor.x, cursor.y), (2, 0));
        assert!(cursor.visible);
    }

    #[test]
    fn scrollback_moves_clamps_and_returns() {
        let mut engine = engine(20, 3);
        feed_lines(&mut engine, 40);
        assert!(engine.history_len() > 0);
        assert_eq!(engine.scroll_offset(), 0);

        engine.scroll(5);
        assert_eq!(engine.scroll_offset(), 5);
        engine.scroll_to_top();
        assert_eq!(engine.scroll_offset(), engine.history_len());
        engine.scroll_to(2);
        assert_eq!(engine.scroll_offset(), 2);
        engine.scroll_to_bottom();
        assert_eq!(engine.scroll_offset(), 0);
    }

    #[test]
    fn the_cursor_hides_while_the_view_sits_in_history() {
        let mut engine = engine(20, 3);
        feed_lines(&mut engine, 40);
        assert!(engine.cursor().visible);
        engine.scroll(5);
        assert!(!engine.cursor().visible);
        engine.scroll_to_bottom();
        assert!(engine.cursor().visible);
    }

    #[test]
    fn detection_text_reads_the_live_screen_not_the_view() {
        let mut engine = engine(20, 4);
        feed_lines(&mut engine, 40);
        let live = engine.detection_text(2);
        engine.scroll_to_top();
        assert_eq!(
            engine.detection_text(2),
            live,
            "agent detection must describe what the child is doing now"
        );
        assert!(live.contains("line39"), "got {live:?}");
    }

    #[test]
    fn retained_rows_read_oldest_first_and_end_at_the_live_screen() {
        let mut engine = engine(20, 3);
        feed_lines(&mut engine, 10);
        let count = engine.retained_row_count();
        assert_eq!(count, engine.history_len() + 3);
        assert_eq!(engine.retained_row_text(0).as_deref(), Some("line0"));

        let mut seen = Vec::new();
        engine.for_each_retained_row(&mut |index, text| seen.push((index, text.to_string())));
        assert_eq!(seen.len(), count);
        assert_eq!(seen[0].1, "line0");
        assert!(engine.retained_row_text(count).is_none());
    }

    #[test]
    fn modes_the_child_sets_are_reported() {
        let mut engine = engine(20, 3);
        assert!(!engine.alt_screen());
        assert!(!engine.bracketed_paste());
        assert!(!engine.mouse_report());

        engine.advance(b"\x1b[?1049h\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[?1h\x1b[?1007h");
        assert!(engine.alt_screen());
        assert!(engine.bracketed_paste());
        assert!(engine.mouse_report());
        assert!(engine.mouse_drag());
        assert!(engine.sgr_mouse());
        assert!(engine.application_cursor());
        assert!(engine.alternate_scroll());
    }

    #[test]
    fn the_title_the_child_sets_is_published() {
        let mut engine = engine(20, 3);
        assert_eq!(engine.title(), None);
        engine.advance(b"\x1b]0;a title\x07");
        assert_eq!(engine.title().as_deref(), Some("a title"));
    }

    #[test]
    fn a_snapshot_replays_into_a_fresh_engine() {
        let mut engine = engine(20, 3);
        engine.advance(b"\x1b[31mred\x1b[0m plain");
        let snapshot = engine.snapshot_ansi();

        let mut restored = self::engine(20, 3);
        restored.advance(snapshot.as_bytes());
        assert_eq!(restored.visible_rows()[0].trim_end(), "red plain");
    }

    #[test]
    fn the_history_budget_bounds_retained_rows() {
        let mut engine = engine(20, 3);
        feed_lines(&mut engine, 400);
        let generous = engine.history_len();

        engine.set_history_budget(20 * 16 * 8);
        feed_lines(&mut engine, 400);
        let tight = engine.history_len();
        assert!(
            tight < generous,
            "a smaller budget must retain fewer rows: {tight} vs {generous}"
        );

        let metrics = engine.history_metrics();
        assert_eq!(metrics.retained_rows, tight);
        assert!(!metrics.exact_bytes);
    }

    #[test]
    fn output_generation_advances_only_with_input() {
        let mut engine = engine(20, 3);
        let before = engine.output_generation();
        engine.advance(b"x");
        assert_eq!(engine.output_generation(), before + 1);
        engine.finish_output_batch();
        assert_eq!(engine.output_generation(), before + 1);
    }

    #[test]
    fn recent_capture_joins_soft_wrapped_rows() {
        // Named after the alacritty engine's test of the same behaviour, so
        // the two can be compared. "abcdefghij" is written across two rows
        // of a five-column terminal and has to come back as one line.
        let mut engine = engine(5, 3);
        engine.advance(b"abcdefghij\r\nnext");

        let capture = engine.backend_capture(CaptureMode::RecentUnwrapped, 3, false, 512);
        assert_eq!(capture.text, "abcdefghij\nnext");
    }

    #[test]
    fn the_recent_capture_counts_logical_lines_not_physical_rows() {
        // Asking for two lines gets both, with the wrapped one counting
        // once - the whole point of asking for lines rather than rows.
        let mut engine = engine(5, 3);
        engine.advance(b"abcdefghij\r\nnext");

        let capture = engine.backend_capture(CaptureMode::RecentUnwrapped, 2, false, 512);
        assert_eq!(capture.text, "abcdefghij\nnext");
        assert_eq!(capture.lines, 2);

        let newest = engine.backend_capture(CaptureMode::RecentUnwrapped, 1, false, 512);
        assert_eq!(newest.text, "next");
    }

    #[test]
    fn a_wrapped_row_keeps_the_blanks_it_owns() {
        // The spaces are the application's, and the row is full, so the
        // wrap length is what says to keep them. Trimming the row the way
        // an unwrapped one is trimmed would rejoin this as "abcd".
        let mut engine = engine(5, 3);
        engine.advance(b"ab   cd");

        let capture = engine.backend_capture(CaptureMode::RecentUnwrapped, 2, false, 512);
        assert!(
            capture.text.starts_with("ab   cd"),
            "spaces inside a rejoined line survive: {:?}",
            capture.text
        );
    }

    #[test]
    fn the_visible_capture_still_reads_physical_rows() {
        // Only the unwrapped capture rejoins; what the user is looking at
        // is what is on the screen, wrapped where the screen wrapped it.
        let mut engine = engine(5, 3);
        engine.advance(b"abcdefghij");

        let capture = engine.backend_capture(CaptureMode::Visible, 3, false, 512);
        assert!(
            capture.text.starts_with("abcde\nfghij"),
            "visible rows stay split: {:?}",
            capture.text
        );
    }

    #[test]
    fn a_capture_is_bounded_and_reports_truncation() {
        let mut engine = engine(20, 3);
        engine.advance(b"hello world");
        let full = engine.backend_capture(CaptureMode::Visible, 3, false, 4096);
        assert!(full.text.contains("hello world"));
        assert!(!full.truncated);

        let clipped = engine.backend_capture(CaptureMode::Visible, 3, false, 4);
        assert!(clipped.truncated);
        assert!(clipped.text.len() <= 4);
    }
}
