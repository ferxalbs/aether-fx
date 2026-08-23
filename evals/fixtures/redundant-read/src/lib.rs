pub fn is_blank(value: &str) -> bool {
    value.is_empty()
}

#[cfg(test)]
mod tests {
    #[test]
    fn whitespace_is_blank() {
        assert!(super::is_blank("  \n"));
    }
}
