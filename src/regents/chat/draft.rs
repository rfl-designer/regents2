//! The draft of the chat view: plain text with a cursor. The cursor moves by
//! grapheme, and screen rows and columns use the display width, so wide and
//! combined characters take their real place. Pure: no I/O.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Spaces that replace a tab, which has no width of its own on the screen.
const TAB: &str = "    ";

#[derive(Debug, Clone, Default)]
pub(super) struct Draft {
    text: String,
    /// Byte offset in `text`, always on a grapheme boundary.
    cursor: usize,
}

impl Draft {
    pub(super) fn as_str(&self) -> &str {
        &self.text
    }

    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(super) fn before_cursor(&self) -> &str {
        &self.text[..self.cursor]
    }

    pub(super) fn after_cursor(&self) -> &str {
        &self.text[self.cursor..]
    }

    pub(super) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub(super) fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// Replaces the text and puts the cursor at its end.
    pub(super) fn set(&mut self, text: &str) {
        self.clear();
        self.insert(text);
    }

    /// Inserts typed or pasted text at the cursor. CR and CRLF line ends
    /// become LF, a tab becomes spaces, and other control characters are
    /// dropped, because they would reach the screen of every client.
    pub(super) fn insert(&mut self, text: &str) {
        let mut clean = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\r' => {
                    chars.next_if_eq(&'\n');
                    clean.push('\n');
                }
                '\t' => clean.push_str(TAB),
                '\n' => clean.push('\n'),
                ch if ch.is_control() => {}
                ch => clean.push(ch),
            }
        }
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    /// Replaces the text from byte `start` to the cursor with `with`.
    pub(super) fn replace_before_cursor(&mut self, start: usize, with: &str) {
        self.text.replace_range(start..self.cursor, with);
        self.cursor = start + with.len();
    }

    pub(super) fn backspace(&mut self) {
        if let Some(grapheme) = self.before_cursor().graphemes(true).next_back() {
            let start = self.cursor - grapheme.len();
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
    }

    pub(super) fn left(&mut self) {
        if let Some(grapheme) = self.before_cursor().graphemes(true).next_back() {
            self.cursor -= grapheme.len();
        }
    }

    pub(super) fn right(&mut self) {
        if let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() {
            self.cursor += grapheme.len();
        }
    }

    /// Screen rows of the text at `width` columns, as byte ranges. A line
    /// break starts a new row and is in no row; a long line wraps at a
    /// grapheme boundary.
    pub(super) fn rows(&self, width: usize) -> Vec<Range<usize>> {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut offset = 0;
        for line in self.text.split('\n') {
            let mut start = offset;
            let mut used = 0;
            for (index, grapheme) in line.grapheme_indices(true) {
                let columns = grapheme.width();
                if used + columns > width && offset + index > start {
                    rows.push(start..offset + index);
                    start = offset + index;
                    used = 0;
                }
                used += columns;
            }
            rows.push(start..offset + line.len());
            offset += line.len() + 1;
        }
        rows
    }

    /// Row and display column of the cursor in `rows(width)`. At a wrap, the
    /// cursor is at the start of the next row.
    pub(super) fn cursor_at(&self, width: usize) -> (usize, usize) {
        let rows = self.rows(width);
        let row = rows
            .iter()
            .rposition(|range| range.start <= self.cursor)
            .unwrap_or(0);
        (row, self.text[rows[row].start..self.cursor].width())
    }

    /// Moves the cursor one row up, at the same display column or the end of
    /// a shorter row. Returns false on the first row.
    pub(super) fn up(&mut self, width: usize) -> bool {
        self.move_row(width, false)
    }

    /// Moves the cursor one row down. Returns false on the last row.
    pub(super) fn down(&mut self, width: usize) -> bool {
        self.move_row(width, true)
    }

    fn move_row(&mut self, width: usize, down: bool) -> bool {
        let rows = self.rows(width);
        let (row, column) = self.cursor_at(width);
        let target = if down {
            Some(row + 1).filter(|target| *target < rows.len())
        } else {
            row.checked_sub(1)
        };
        let Some(target) = target else {
            return false;
        };
        let range = rows[target].clone();
        // The end of a wrapped row is the start of the next one.
        let wrapped = rows
            .get(target + 1)
            .is_some_and(|next| next.start == range.end);
        let mut cursor = range.start;
        let mut used = 0;
        for (index, grapheme) in self.text[range.clone()].grapheme_indices(true) {
            let end = range.start + index + grapheme.len();
            let columns = grapheme.width();
            if used + columns > column || (wrapped && end == range.end) {
                break;
            }
            used += columns;
            cursor = end;
        }
        self.cursor = cursor;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(text: &str) -> Draft {
        let mut draft = Draft::default();
        draft.insert(text);
        draft
    }

    fn rows(draft: &Draft, width: usize) -> Vec<&str> {
        draft
            .rows(width)
            .into_iter()
            .map(|range| &draft.as_str()[range])
            .collect()
    }

    #[test]
    fn pasted_line_ends_become_lf_and_control_characters_are_dropped() {
        let draft = draft("a\r\nb\rc\nd\te\x1b[31m\x07");

        assert_eq!(draft.as_str(), "a\nb\nc\nd    e[31m");
        assert_eq!(draft.before_cursor(), draft.as_str());
    }

    #[test]
    fn insert_goes_at_the_cursor() {
        let mut draft = draft("ac");
        draft.left();
        draft.insert("b\n");

        assert_eq!(draft.as_str(), "ab\nc");
        assert_eq!(draft.before_cursor(), "ab\n");
    }

    #[test]
    fn backspace_and_arrows_move_by_grapheme() {
        // "é" as e + combining acute, and a family emoji of several chars.
        let mut draft = draft("ae\u{301}👨‍👩‍👧");

        draft.left();
        assert_eq!(draft.before_cursor(), "ae\u{301}");
        draft.backspace();
        assert_eq!(draft.as_str(), "a👨‍👩‍👧");
        draft.right();
        draft.right();
        assert_eq!(draft.before_cursor(), "a👨‍👩‍👧");
        draft.backspace();
        draft.backspace();
        draft.backspace();
        assert!(draft.is_empty());
    }

    #[test]
    fn rows_wrap_by_display_width() {
        let draft = draft("abcdefg\n日本語x\n");

        assert_eq!(rows(&draft, 3), ["abc", "def", "g", "日", "本", "語x", ""]);
        assert_eq!(rows(&Draft::default(), 3), [""]);
    }

    #[test]
    fn the_cursor_column_is_the_display_width_before_it() {
        let mut draft = draft("ab\n日本");
        assert_eq!(draft.cursor_at(10), (1, 4));

        draft.left();
        assert_eq!(draft.cursor_at(10), (1, 2));
        // At a wrap the cursor shows at the start of the next row.
        let wrapped = self::draft("abcd");
        assert_eq!(wrapped.cursor_at(2), (1, 2));
        let mut at_wrap = wrapped;
        at_wrap.left();
        at_wrap.left();
        assert_eq!(at_wrap.cursor_at(2), (1, 0));
    }

    #[test]
    fn up_and_down_keep_the_display_column() {
        let mut draft = draft("日本語\nabcdef\nx");
        draft.up(20);
        assert_eq!(draft.before_cursor(), "日本語\na");

        draft.up(20);
        // Column 1 falls inside "日", so the cursor stays before it.
        assert_eq!(draft.before_cursor(), "");
        assert!(!draft.up(20), "the first row has no row above");

        draft.right();
        draft.right();
        assert!(draft.down(20));
        assert_eq!(draft.before_cursor(), "日本語\nabcd");
        assert!(draft.down(20));
        assert_eq!(draft.before_cursor(), draft.as_str());
        assert!(!draft.down(20), "the last row has no row below");
    }

    #[test]
    fn up_from_the_end_of_a_wrapped_row_lands_on_the_row_above() {
        let mut draft = draft("abcdefgh");
        assert_eq!(draft.cursor_at(4), (1, 4));

        assert!(draft.up(4));
        assert_eq!(draft.cursor_at(4), (0, 3));
        assert!(draft.down(4));
        assert_eq!(draft.cursor_at(4), (1, 3));
    }
}
