pub fn last_index(values: &[u8]) -> Option<usize> {
    (!values.is_empty()).then(|| values.len())
}

#[cfg(test)]
mod tests {
    use super::last_index;

    #[test]
    fn returns_last_valid_index() {
        assert_eq!(last_index(&[4, 8, 15]), Some(2));
        assert_eq!(last_index(&[]), None);
    }
}
