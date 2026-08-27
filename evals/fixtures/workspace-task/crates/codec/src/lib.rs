pub fn normalize(input: &str) -> String {
    input.trim().to_ascii_lowercase().replace(' ', "_")
}

#[cfg(test)]
mod tests {
    #[test]
    fn normalize_uses_hyphens() {
        assert_eq!(super::normalize("Hello World"), "hello-world");
    }
}
