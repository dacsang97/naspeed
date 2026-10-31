/// Body served by hello-server; tests and `ns dev` edit this string.
pub fn greeting() -> &'static str {
    "hello v1"
}

#[cfg(test)]
mod tests {
    #[test]
    fn greeting_is_not_empty() {
        assert!(!super::greeting().is_empty());
    }
}
