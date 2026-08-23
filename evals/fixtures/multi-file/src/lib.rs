pub fn display_name(input: &str) -> String {
    input.trim().to_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn display_name_uses_slug_format() {
        assert_eq!(super::display_name("Hello World"), "hello-world");
    }
}
