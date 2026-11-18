//! Session picker filtering, selection and scope/sort handling.
//!
//! Split out of app.rs without behaviour changes.

use super::*;

impl App {
    pub(super) fn session_view(&self) -> Vec<usize> {
        let query = self.session_query.trim().to_ascii_lowercase();
        let current_cwd = self
            .ready
            .as_ref()
            .map(|ready| ready.cwd.as_str())
            .unwrap_or("");
        let normalize = |value: &str| {
            value
                .replace('\\', "/")
                .trim_end_matches('/')
                .to_ascii_lowercase()
        };
        let mut rows: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                if self.session_scope == SessionScope::Current
                    && (current_cwd.is_empty() || normalize(&row.cwd) != normalize(current_cwd))
                {
                    return None;
                }
                if !query.is_empty() {
                    let file = row
                        .path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("");
                    let haystack =
                        format!("{} {} {}", row.title, file, row.cwd).to_ascii_lowercase();
                    if !haystack.contains(&query) {
                        return None;
                    }
                }
                Some(index)
            })
            .collect();
        if self.session_sort == SessionSort::Name {
            rows.sort_by(|left, right| {
                let left = &self.sessions[*left];
                let right = &self.sessions[*right];
                left.title
                    .to_ascii_lowercase()
                    .cmp(&right.title.to_ascii_lowercase())
                    .then_with(|| left.path.cmp(&right.path))
            });
        }
        rows
    }

    pub(super) fn first_visible_session(&self) -> usize {
        self.session_view().first().copied().unwrap_or(0)
    }

    pub(super) fn move_session_selection(&self, current: usize, delta: isize) -> usize {
        let view = self.session_view();
        if view.is_empty() {
            return 0;
        }
        let Some(position) = view.iter().position(|actual| *actual == current) else {
            return view[0];
        };
        let next = (position as isize + delta).clamp(0, view.len() as isize - 1);
        view[next as usize]
    }

    pub(super) fn reset_session_picker(&mut self) {
        self.session_delete_pending = None;
        let index = self.first_visible_session();
        self.overlay = Some(Overlay::Sessions { index });
    }

    pub(super) fn open_session_picker(&mut self) {
        self.session_query.clear();
        self.session_scope = SessionScope::All;
        self.session_sort = SessionSort::Recent;
        self.reset_session_picker();
    }

    pub(super) fn clamp_session_picker(&mut self) {
        let Some(Overlay::Sessions { index }) = self.overlay.as_ref() else {
            return;
        };
        let index = *index;
        let view = self.session_view();
        if !view.contains(&index) {
            self.overlay = Some(Overlay::Sessions {
                index: view.first().copied().unwrap_or(0),
            });
            self.session_delete_pending = None;
        }
    }
}
