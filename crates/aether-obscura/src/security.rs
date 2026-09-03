use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::Url;

use crate::ObscuraError;

/// Parsed URL after AETHER's scheme, credential, and literal-address checks.
#[derive(Clone, Debug)]
pub struct BrowserUrl {
    pub url: Url,
    pub origin: String,
    pub loopback: bool,
}

/// Validate the URL surface that can cross the Obscura navigation boundary.
///
/// DNS resolution, redirect handling, and effective destination checks remain an Obscura
/// contract. Obscura v0.2.1 performs those checks when its private-network flag is absent.
pub fn validate_browser_url(raw: &str) -> Result<BrowserUrl, ObscuraError> {
    let url = Url::parse(raw).map_err(|error| ObscuraError::InvalidUrl {
        message: format!("URL is not valid: {error}"),
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ObscuraError::NetworkPolicy {
            message: "only http and https URLs are supported".to_owned(),
        });
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ObscuraError::NetworkPolicy {
            message: "URLs with embedded credentials are not allowed".to_owned(),
        });
    }
    let Some(host) = url.host_str() else {
        return Err(ObscuraError::InvalidUrl { message: "URL has no host".to_owned() });
    };
    if host.is_empty() {
        return Err(ObscuraError::InvalidUrl { message: "URL has an empty host".to_owned() });
    }
    let loopback = is_loopback_host(host);
    let literal_host =
        host.strip_prefix('[').and_then(|value| value.strip_suffix(']')).unwrap_or(host);
    if let Ok(address) = literal_host.parse::<IpAddr>() {
        if !loopback && !is_public_address(address) {
            return Err(ObscuraError::NetworkPolicy {
                message: "private, link-local, metadata, multicast, or reserved destinations are not allowed".to_owned(),
            });
        }
    } else if host.starts_with('[') {
        return Err(ObscuraError::NetworkPolicy {
            message: "IPv6 literal addresses with zones or invalid syntax are not allowed"
                .to_owned(),
        });
    } else if host.eq_ignore_ascii_case("localhost") {
        // Obscura's v0.2.1 default policy still blocks this destination. Keep the URL parser
        // permissive so callers can return the more precise provider-capability diagnostic.
    }
    let origin = url.origin().ascii_serialization();
    Ok(BrowserUrl { url, origin, loopback })
}

/// Return a bounded, query-free origin suitable for a permission prompt.
pub fn sanitized_origin(raw: &str) -> Option<String> {
    validate_browser_url(raw).ok().map(|value| value.origin)
}

fn is_loopback_host(host: &str) -> bool {
    let literal_host =
        host.strip_prefix('[').and_then(|value| value.strip_suffix(']')).unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || literal_host.parse::<IpAddr>().is_ok_and(|address| match address {
            IpAddr::V4(address) => address.is_loopback(),
            IpAddr::V6(address) => address.is_loopback(),
        })
}

fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    !address.is_private()
        && !address.is_link_local()
        && !address.is_loopback()
        && !address.is_unspecified()
        && !address.is_multicast()
        && !address.is_broadcast()
        && !is_ipv4_metadata(address)
        && !is_ipv4_documentation(address)
        && !is_ipv4_reserved(address)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    !address.is_loopback()
        && !address.is_unspecified()
        && !address.is_multicast()
        && !address.is_unique_local()
        && !address.is_unicast_link_local()
        && !is_ipv6_documentation(address)
        && !is_ipv6_reserved(address)
}

fn is_ipv4_metadata(address: Ipv4Addr) -> bool {
    address == Ipv4Addr::new(169, 254, 169, 254)
}

fn is_ipv4_documentation(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
}

fn is_ipv4_reserved(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    octets[0] == 0 || (octets[0] == 100 && (64..=127).contains(&octets[1])) || octets[0] >= 224
}

fn is_ipv6_documentation(address: Ipv6Addr) -> bool {
    address.segments()[0] == 0x2001 && address.segments()[1] == 0x0db8
}

fn is_ipv6_reserved(address: Ipv6Addr) -> bool {
    let first = address.segments()[0];
    first == 0x0000 || (first & 0xff00) == 0xff00
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_policy_rejects_dangerous_schemes_and_credentials() {
        assert!(validate_browser_url("file:///tmp/a").is_err());
        assert!(validate_browser_url("javascript:alert(1)").is_err());
        assert!(validate_browser_url("https://user:pass@example.com").is_err());
    }

    #[test]
    fn url_policy_accepts_public_http_and_loopback_shape() {
        let public = validate_browser_url("https://example.com/docs?q=secret").unwrap();
        assert_eq!(public.origin, "https://example.com");
        assert!(!public.loopback);
        assert!(validate_browser_url("http://localhost:8080").unwrap().loopback);
        assert!(validate_browser_url("http://127.0.0.1:8080").unwrap().loopback);
        assert!(validate_browser_url("http://[::1]:8080").unwrap().loopback);
    }

    #[test]
    fn url_policy_rejects_literal_private_and_reserved_addresses() {
        for address in [
            "http://10.0.0.1",
            "http://172.16.0.1",
            "http://192.168.0.1",
            "http://169.254.169.254",
            "http://[fd00::1]",
            "http://[fe80::1]",
            "http://224.0.0.1",
        ] {
            assert!(validate_browser_url(address).is_err(), "{address}");
        }
    }
}
