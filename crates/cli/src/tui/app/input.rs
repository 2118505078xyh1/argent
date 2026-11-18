//! Input editing: cursor movement, undo/redo, word and line editing.
//!
//! These methods live in their own file so `app.rs` stays focused on state
//! and event handling. Behaviour is unchanged.

use super::*;

impl App {
    pub(super) fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.push_undo();
        for ch in text.chars() {
            self.input.insert(self.cursor, ch);
            self.cursor += 1;
        }
        self.dirty = true;
    }

    /// 粘贴外部文本（终端 bracketed paste 或右键剪贴板）。
    ///
    /// CRLF 统一成 LF；换行保留，输入框按多行显示。
    pub(super) fn insert_pasted(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.insert_text(&normalized);
    }

    pub(super) fn backspace(&mut self) {
        if self.cursor > 0 {
            self.push_undo();
            self.cursor -= 1;
            self.input.remove(self.cursor);
            self.dirty = true;
        }
    }

    pub(super) fn push_undo(&mut self) {
        const LIMIT: usize = 100;
        self.undo_stack.push((self.input.clone(), self.cursor));
        if self.undo_stack.len() > LIMIT {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    pub(super) fn undo_edit(&mut self) {
        if let Some((input, cursor)) = self.undo_stack.pop() {
            self.redo_stack.push((self.input.clone(), self.cursor));
            self.input = input;
            self.cursor = cursor.min(self.input.len());
            self.dirty = true;
        }
    }

    pub(super) fn redo_edit(&mut self) {
        if let Some((input, cursor)) = self.redo_stack.pop() {
            self.undo_stack.push((self.input.clone(), self.cursor));
            self.input = input;
            self.cursor = cursor.min(self.input.len());
            self.dirty = true;
        }
    }

    pub(super) fn move_word_left(&mut self) {
        let mut cursor = self.cursor;
        while cursor > 0 && self.input[cursor - 1].is_whitespace() {
            cursor -= 1;
        }
        while cursor > 0 && !self.input[cursor - 1].is_whitespace() {
            cursor -= 1;
        }
        self.cursor = cursor;
    }

    pub(super) fn move_word_right(&mut self) {
        let mut cursor = self.cursor;
        while cursor < self.input.len() && self.input[cursor].is_whitespace() {
            cursor += 1;
        }
        while cursor < self.input.len() && !self.input[cursor].is_whitespace() {
            cursor += 1;
        }
        self.cursor = cursor;
    }

    pub(super) fn kill_to_end(&mut self) {
        if self.cursor < self.input.len() {
            self.push_undo();
            self.input.truncate(self.cursor);
            self.dirty = true;
        }
    }

    pub(super) fn kill_to_start(&mut self) {
        if self.cursor > 0 {
            self.push_undo();
            self.input.drain(..self.cursor);
            self.cursor = 0;
            self.dirty = true;
        }
    }

    pub(super) fn delete_word_before(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let mut start = self.cursor;
        while start > 0 && self.input[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !self.input[start - 1].is_whitespace() {
            start -= 1;
        }
        self.input.drain(start..self.cursor);
        self.cursor = start;
        self.dirty = true;
    }

    /// 光标所在行（0 基）与行内字符列。换行符算上一行的行尾。
    pub(super) fn cursor_line_position(&self) -> (usize, usize) {
        let mut row = 0usize;
        let mut col = 0usize;
        for (index, ch) in self.input.iter().enumerate() {
            if index >= self.cursor {
                break;
            }
            if *ch == '\n' {
                row += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (row, col)
    }

    /// 第 `row` 行的字符区间 `[start, end)`，不含换行符。
    fn line_bounds(&self, row: usize) -> (usize, usize) {
        let mut current = 0usize;
        let mut start = 0usize;
        for (index, ch) in self.input.iter().enumerate() {
            if current == row {
                break;
            }
            if *ch == '\n' {
                current += 1;
                start = index + 1;
            }
        }
        let mut end = self.input.len();
        for (index, ch) in self.input.iter().enumerate().skip(start) {
            if *ch == '\n' {
                end = index;
                break;
            }
        }
        (start, end)
    }

    /// ↑/↓ 在多行输入里移动光标，尽量保持列。
    ///
    /// 已经在首行（↑）或末行（↓）时返回 `false`，让调用方继续走输入历史。
    pub(super) fn move_cursor_line(&mut self, down: bool) -> bool {
        let (row, col) = self.cursor_line_position();
        let total = self.input.iter().filter(|ch| **ch == '\n').count() + 1;
        let target = if down {
            if row + 1 >= total {
                return false;
            }
            row + 1
        } else {
            if row == 0 {
                return false;
            }
            row - 1
        };
        let (start, end) = self.line_bounds(target);
        self.cursor = (start + col).min(end);
        true
    }
}
