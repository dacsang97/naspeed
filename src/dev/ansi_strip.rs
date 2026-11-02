//! Removes terminal control sequences before writing logs or copying text.

#[derive(Clone, Copy)]
enum State {
    Text,
    Escape,
    Csi,
    Osc,
    OscEscape,
    StringControl,
    StringControlEscape,
}

pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut state = State::Text;
    for ch in input.chars() {
        state = match state {
            State::Text => match ch {
                '\x1b' => State::Escape,
                c if c.is_control() && c != '\t' => State::Text,
                c => {
                    out.push(c);
                    State::Text
                }
            },
            State::Escape => match ch {
                '[' => State::Csi,
                ']' => State::Osc,
                'P' | '^' | '_' => State::StringControl,
                _ => State::Text,
            },
            State::Csi => {
                if ('@'..='~').contains(&ch) {
                    State::Text
                } else {
                    State::Csi
                }
            }
            State::Osc => match ch {
                '\x07' => State::Text,
                '\x1b' => State::OscEscape,
                _ => State::Osc,
            },
            State::OscEscape => match ch {
                '\\' | '\x07' => State::Text,
                '\x1b' => State::OscEscape,
                _ => State::Osc,
            },
            State::StringControl => {
                if ch == '\x1b' {
                    State::StringControlEscape
                } else {
                    State::StringControl
                }
            }
            State::StringControlEscape => match ch {
                '\\' => State::Text,
                '\x1b' => State::StringControlEscape,
                _ => State::StringControl,
            },
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_sgr_cursor_moves_and_osc_hyperlinks() {
        let input = "\x1b[31mred\x1b[0m\x1b[2K\x1b]8;;https://example.test\x07link\x1b]8;;\x07";
        assert_eq!(strip_ansi(input), "redlink");
    }

    #[test]
    fn removes_other_controls_but_keeps_tabs_and_unicode() {
        assert_eq!(strip_ansi("a\x00\tb\u{1f642}"), "a\tb\u{1f642}");
    }

    #[test]
    fn drops_a_lone_escape_without_dropping_text() {
        assert_eq!(strip_ansi("before\x1b"), "before");
    }
}
