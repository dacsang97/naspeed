#[derive(Default)]
pub(super) struct Selection {
    anchor: Option<u64>,
    cursor: Option<u64>,
}

impl Selection {
    pub(super) fn active(&self) -> bool {
        self.anchor.is_some() && self.cursor.is_some()
    }

    pub(super) fn anchor(&self) -> Option<u64> {
        self.anchor
    }

    pub(super) fn cursor(&self) -> Option<u64> {
        self.cursor
    }

    pub(super) fn start(&mut self, id: u64) {
        self.anchor = Some(id);
        self.cursor = Some(id);
    }

    pub(super) fn extend_to(&mut self, id: u64) {
        if self.anchor.is_none() {
            self.anchor = Some(id);
        }
        self.cursor = Some(id);
    }

    pub(super) fn clear(&mut self) {
        self.anchor = None;
        self.cursor = None;
    }
}

pub(super) fn is_continuation(text: &str) -> bool {
    text.chars().next().is_some_and(char::is_whitespace)
        || text.starts_with("at ")
        || text.starts_with("-->")
        || text.starts_with('|')
        || text.starts_with("Caused by")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_rust_and_indented_continuations() {
        for line in [
            "    at app::main",
            "at app::main",
            " --> src/lib.rs:4:3",
            "| let value = 1;",
            "Caused by: missing file",
        ] {
            assert!(is_continuation(line), "{line:?}");
        }
        assert!(!is_continuation("thread 'main' panicked"));
        assert!(!is_continuation("error[E0308]: mismatched types"));
    }
}
