use regex::{Regex, RegexBuilder};

#[derive(Clone, Debug)]
pub(super) struct Query {
    regex: Regex,
}

impl Query {
    pub(super) fn parse(text: &str) -> Result<Query, regex::Error> {
        let (pattern, case_source) = if let Some(pattern) = text.strip_prefix("re:") {
            (pattern.to_string(), pattern)
        } else {
            (regex::escape(text), text)
        };
        let case_insensitive = !case_source.chars().any(char::is_uppercase);
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(case_insensitive)
            .build()?;
        Ok(Query { regex })
    }

    pub(super) fn matches(&self, text: &str) -> bool {
        self.regex.is_match(text)
    }

    /// Byte ranges of the non-empty hits in `text`.
    pub(super) fn ranges(&self, text: &str) -> Vec<std::ops::Range<usize>> {
        self.regex
            .find_iter(text)
            .map(|hit| hit.range())
            .filter(|range| !range.is_empty())
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum LogLevel {
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub(super) fn detect(text: &str) -> LogLevel {
        let mut level = LogLevel::Info;
        for token in text.split(|ch: char| !ch.is_ascii_alphanumeric()) {
            if token.eq_ignore_ascii_case("error") || token.eq_ignore_ascii_case("panicked") {
                return LogLevel::Error;
            }
            if token.eq_ignore_ascii_case("warn") || token.eq_ignore_ascii_case("warning") {
                level = LogLevel::Warn;
            }
        }
        level
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum LevelFilter {
    #[default]
    All,
    WarnUp,
    ErrorOnly,
}

impl LevelFilter {
    pub(super) fn accepts(self, level: LogLevel) -> bool {
        match self {
            LevelFilter::All => true,
            LevelFilter::WarnUp => level != LogLevel::Info,
            LevelFilter::ErrorOnly => level == LogLevel::Error,
        }
    }

    pub(super) fn next(self) -> LevelFilter {
        match self {
            LevelFilter::All => LevelFilter::WarnUp,
            LevelFilter::WarnUp => LevelFilter::ErrorOnly,
            LevelFilter::ErrorOnly => LevelFilter::All,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            LevelFilter::All => "all levels",
            LevelFilter::WarnUp => "WARN+",
            LevelFilter::ErrorOnly => "ERROR",
        }
    }
}
