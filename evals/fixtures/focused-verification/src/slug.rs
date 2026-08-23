pub fn format_slug(value: &str) -> String {
    value.to_ascii_lowercase().replace(' ', "-")
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_slug() {
        assert_eq!(super::format_slug(" Hello World "), "hello-world");
    }

    #[test]
    fn unrelated_behavior() {
        assert_eq!(2 + 2, 4);
    }
}
