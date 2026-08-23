pub fn parse_port(value: &str) -> Option<u16> {
    value.parse::<u8>().ok().map(u16::from)
}

#[cfg(test)]
mod tests {
    #[test]
    fn accepts_full_port_range() {
        assert_eq!(super::parse_port("8080"), Some(8080));
    }
}
