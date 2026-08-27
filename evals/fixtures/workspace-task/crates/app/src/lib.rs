pub fn display_name(input: &str) -> String {
    codec::normalize(input)
}

#[cfg(test)]
mod tests {
    #[test]
    fn display_name_uses_codec_contract() {
        assert_eq!(super::display_name("Hello World"), "hello-world");
    }
}
