//! SSRF guard for the link-unfurl worker. SECURITY-CRITICAL.
//!
//! The unfurl worker fetches URLs that arbitrary (authenticated) users typed
//! into chat, from inside our network. Without these checks a message like
//! `http://169.254.169.254/computeMetadata/v1/...` turns the server into a
//! proxy for cloud metadata, internal admin panels, and localhost services.
//!
//! Guard layers (each hop of a redirect chain passes through ALL of them):
//!
//! 1. [`vet_url`] — scheme must be http/https, no credentials in the URL,
//!    host must be present.
//! 2. [`vet_target`] — the host's IP addresses must all be publicly
//!    routable. IP-literal hosts are classified directly; domains are
//!    resolved and EVERY returned address must pass ([`forbidden_ip`]) —
//!    one bad A/AAAA record poisons the whole set, which defeats
//!    multi-record rebinding tricks.
//! 3. The caller pins the connection to exactly the vetted addresses
//!    (`reqwest`'s `resolve_to_addrs`), so the request cannot re-resolve to
//!    something else between check and connect (DNS-rebinding TOCTOU).
//!
//! Forbidden ranges (v4): 0.0.0.0/8, 127.0.0.0/8 (loopback), 10.0.0.0/8,
//! 172.16.0.0/12, 192.168.0.0/16 (private), 169.254.0.0/16 (link-local — the
//! cloud metadata service 169.254.169.254 lives here), 100.64.0.0/10 (CGNAT,
//! used for cloud-internal services), 192.0.0.0/24 (IETF), the three
//! TEST-NETs, 198.18.0.0/15 (benchmarking), and 224.0.0.0/3
//! (multicast/reserved/broadcast).
//!
//! Forbidden ranges (v6): `::`, `::1`, fc00::/7 (ULA), fe80::/10
//! (link-local), fec0::/10 (deprecated site-local), ff00::/8 (multicast),
//! 2001:db8::/32 (documentation), 2001::/23 (IETF protocol assignments —
//! covers Teredo and ORCHID, which embed attacker-chosen IPv4), ::/96
//! (deprecated IPv4-compatible), 64:ff9b::/96 + 64:ff9b:1::/48 (NAT64 —
//! embeds an IPv4 the translator would connect to). IPv4-mapped addresses
//! (`::ffff:a.b.c.d`) are classified by their embedded IPv4 — the classic
//! bypass is `http://[::ffff:169.254.169.254]/`. 6to4 (2002::/16) is likewise
//! classified by the IPv4 embedded in its prefix.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use url::{Host, Url};

/// Static (pre-DNS) URL vetting: scheme, credentials, host presence.
///
/// Port is deliberately NOT restricted: internal services are unreachable via
/// the IP classification regardless of port, and public sites legitimately
/// serve on :8080/:8443 etc.
pub fn vet_url(url: &Url) -> Result<(), String> {
    match url.scheme() {
        // `Url::parse` lowercases the scheme, so this also rejects "HTTPS" variants of anything else.
        "http" | "https" => {}
        other => return Err(format!("scheme '{other}' is not http(s)")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        // Credentialed URLs are a phishing/confusion vector (http://trusted.com@evil/)
        // and we must never send user-typed credentials anywhere.
        return Err("URL carries credentials".to_string());
    }
    if url.host().is_none() {
        return Err("URL has no host".to_string());
    }
    Ok(())
}

/// The name of the forbidden range this address falls in, or `None` if it is
/// publicly routable. The returned label is for logs and tests only — never
/// stored or shown to clients.
pub fn forbidden_ip(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => forbidden_ipv4(v4),
        IpAddr::V6(v6) => forbidden_ipv6(v6),
    }
}

fn forbidden_ipv4(ip: Ipv4Addr) -> Option<&'static str> {
    let o = ip.octets();
    if o[0] == 0 {
        Some("0.0.0.0/8 (this-network)")
    } else if o[0] == 127 {
        Some("127.0.0.0/8 (loopback)")
    } else if o[0] == 10 {
        Some("10.0.0.0/8 (private)")
    } else if o[0] == 172 && (16..=31).contains(&o[1]) {
        Some("172.16.0.0/12 (private)")
    } else if o[0] == 192 && o[1] == 168 {
        Some("192.168.0.0/16 (private)")
    } else if o[0] == 169 && o[1] == 254 {
        Some("169.254.0.0/16 (link-local / cloud metadata)")
    } else if o[0] == 100 && (64..=127).contains(&o[1]) {
        Some("100.64.0.0/10 (CGNAT / cloud-internal)")
    } else if o[0] == 192 && o[1] == 0 && o[2] == 0 {
        Some("192.0.0.0/24 (IETF protocol assignments)")
    } else if o[0] == 192 && o[1] == 0 && o[2] == 2 {
        Some("192.0.2.0/24 (TEST-NET-1)")
    } else if o[0] == 198 && o[1] == 51 && o[2] == 100 {
        Some("198.51.100.0/24 (TEST-NET-2)")
    } else if o[0] == 203 && o[1] == 0 && o[2] == 113 {
        Some("203.0.113.0/24 (TEST-NET-3)")
    } else if o[0] == 198 && (o[1] & 0xfe) == 18 {
        Some("198.18.0.0/15 (benchmarking)")
    } else if o[0] >= 224 {
        // 224.0.0.0/4 multicast + 240.0.0.0/4 reserved + 255.255.255.255.
        Some("224.0.0.0/3 (multicast/reserved/broadcast)")
    } else {
        None
    }
}

fn forbidden_ipv6(ip: Ipv6Addr) -> Option<&'static str> {
    if ip.is_unspecified() {
        return Some(":: (unspecified)");
    }
    if ip.is_loopback() {
        return Some("::1 (loopback)");
    }
    // IPv4-mapped (::ffff:a.b.c.d): the connection actually goes to the
    // embedded IPv4, so classify THAT — the canonical guard bypass.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return forbidden_ipv4(v4).map(|_| "IPv4-mapped form of a forbidden IPv4 range");
    }
    let seg = ip.segments();
    if seg[..6] == [0, 0, 0, 0, 0, 0] {
        // Deprecated IPv4-compatible ::a.b.c.d (and the rest of ::/96):
        // nothing legitimate lives here; refuse wholesale.
        return Some("::/96 (deprecated IPv4-compatible)");
    }
    if seg[0] == 0x0064 && seg[1] == 0xff9b && (seg[2..6] == [0, 0, 0, 0] || (seg[2] == 1 && seg[3..6] == [0, 0, 0])) {
        // NAT64 well-known (64:ff9b::/96) and local-use (64:ff9b:1::/48)
        // prefixes embed an IPv4 the operator's translator connects to —
        // a NAT64 deployment would let these reach internal v4 space.
        return Some("64:ff9b::/96 (NAT64 translation prefix)");
    }
    if (seg[0] & 0xfe00) == 0xfc00 {
        return Some("fc00::/7 (unique local)");
    }
    if (seg[0] & 0xffc0) == 0xfe80 {
        return Some("fe80::/10 (link-local)");
    }
    if (seg[0] & 0xffc0) == 0xfec0 {
        return Some("fec0::/10 (deprecated site-local)");
    }
    if (seg[0] & 0xff00) == 0xff00 {
        return Some("ff00::/8 (multicast)");
    }
    if seg[0] == 0x2001 && seg[1] == 0x0db8 {
        return Some("2001:db8::/32 (documentation)");
    }
    if seg[0] == 0x2001 && seg[1] < 0x0200 {
        // IETF protocol assignments (2001::/23): includes Teredo (2001::/32,
        // embeds an attacker-chosen, XOR-obfuscated IPv4), benchmarking
        // (2001:2::/48) and ORCHID. Nothing here is a fetchable website.
        return Some("2001::/23 (IETF protocol assignments incl. Teredo)");
    }
    if seg[0] == 0x2002 {
        // 6to4: the IPv4 the relay would deliver to sits in segments 1-2.
        let v4 = Ipv4Addr::new((seg[1] >> 8) as u8, (seg[1] & 0xff) as u8, (seg[2] >> 8) as u8, (seg[2] & 0xff) as u8);
        if forbidden_ipv4(v4).is_some() {
            return Some("2002::/16 (6to4) embedding a forbidden IPv4");
        }
    }
    None
}

/// Resolve and vet the URL's target addresses.
///
/// Returns the DNS pin the caller must apply to the HTTP client:
/// - `Ok(None)` — the host is an IP literal that passed classification; no
///   pinning needed (there is no DNS step to subvert).
/// - `Ok(Some((domain, addrs)))` — a domain that resolved to exclusively
///   public addresses; connect ONLY to `addrs` (via `resolve_to_addrs`).
///
/// Any forbidden address among the results fails the WHOLE set: an attacker
/// controlling DNS can mix a public record with 127.0.0.1 and hope the
/// resolver rotates — all-or-nothing removes that bet.
pub async fn vet_target(url: &Url) -> Result<Option<(String, Vec<SocketAddr>)>, String> {
    let port = url.port_or_known_default().ok_or_else(|| "URL has no port".to_string())?;
    match url.host() {
        None => Err("URL has no host".to_string()),
        Some(Host::Ipv4(ip)) => match forbidden_ip(IpAddr::V4(ip)) {
            Some(range) => Err(format!("address {ip} is in {range}")),
            None => Ok(None),
        },
        Some(Host::Ipv6(ip)) => match forbidden_ip(IpAddr::V6(ip)) {
            Some(range) => Err(format!("address {ip} is in {range}")),
            None => Ok(None),
        },
        Some(Host::Domain(domain)) => {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((domain, port))
                .await
                .map_err(|e| format!("DNS resolution failed for {domain}: {e}"))?
                .collect();
            if addrs.is_empty() {
                return Err(format!("{domain} resolved to no addresses"));
            }
            for addr in &addrs {
                if let Some(range) = forbidden_ip(addr.ip()) {
                    return Err(format!("{domain} resolves to {} ({range})", addr.ip()));
                }
            }
            Ok(Some((domain.to_string(), addrs)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr { s.parse::<Ipv4Addr>().unwrap().into() }
    fn v6(s: &str) -> IpAddr { s.parse::<Ipv6Addr>().unwrap().into() }

    #[test]
    fn forbidden_ipv4_ranges() {
        // Every range from the guard's spec, probed at edges and interior.
        for addr in [
            "0.0.0.1",
            "0.255.255.255",
            "127.0.0.1",
            "127.255.255.254", // whole /8, not just .1
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1", // /12 lower edge
            "172.31.255.255", // /12 upper edge
            "192.168.0.1",
            "192.168.255.255",
            "169.254.169.254", // THE cloud metadata address
            "169.254.0.1",
            "100.64.0.1", // CGNAT lower edge
            "100.127.255.255", // CGNAT upper edge
            "192.0.0.1",
            "192.0.2.5",     // TEST-NET-1
            "198.51.100.7",  // TEST-NET-2
            "203.0.113.9",   // TEST-NET-3
            "198.18.0.1",    // benchmarking /15 lower half
            "198.19.255.255", // benchmarking /15 upper half
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            assert!(forbidden_ip(v4(addr)).is_some(), "{addr} must be forbidden");
        }
    }

    #[test]
    fn allowed_ipv4_neighbors_of_forbidden_ranges() {
        // One step outside each range must remain fetchable — a guard that
        // over-blocks silently breaks legitimate previews.
        for addr in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34", // example.com
            "9.255.255.255", // below 10/8
            "11.0.0.0",      // above 10/8
            "172.15.255.255", // below 172.16/12
            "172.32.0.0",     // above 172.16/12
            "192.167.255.255",
            "192.169.0.0",
            "169.253.255.255",
            "169.255.0.0",
            "100.63.255.255", // below CGNAT
            "100.128.0.0",    // above CGNAT
            "192.0.1.1",      // between 192.0.0/24 and 192.0.2/24
            "192.0.3.0",
            "198.17.255.255", // below benchmarking
            "198.20.0.0",     // above benchmarking
            "223.255.255.255", // last unicast before multicast
            "128.0.0.1",
        ] {
            assert!(forbidden_ip(v4(addr)).is_none(), "{addr} must be allowed");
        }
    }

    #[test]
    fn forbidden_ipv6_ranges() {
        for addr in [
            "::",
            "::1",
            "fc00::1",
            "fdff:ffff::1", // ULA upper half (fd00::/8)
            "fe80::1",
            "febf::1", // link-local /10 upper edge
            "fec0::1", // deprecated site-local
            "ff02::1", // multicast
            "2001:db8::1",
            "2001::1",   // Teredo (inside 2001::/23)
            "2001:2::1", // benchmarking (inside 2001::/23)
            "64:ff9b::a00:1", // NAT64 well-known embedding 10.0.0.1
            "64:ff9b:1::1",   // NAT64 local-use
        ] {
            assert!(forbidden_ip(v6(addr)).is_some(), "{addr} must be forbidden");
        }
    }

    #[test]
    fn ipv4_mapped_and_compatible_bypasses_are_closed() {
        // The classic guard bypass: wrap a forbidden v4 in a v6 literal.
        for addr in ["::ffff:127.0.0.1", "::ffff:10.0.0.1", "::ffff:169.254.169.254", "::ffff:192.168.1.1"] {
            assert!(forbidden_ip(v6(addr)).is_some(), "{addr} must be forbidden (mapped v4)");
        }
        // Deprecated IPv4-compatible form: refused wholesale, even "public".
        assert!(forbidden_ip(v6("::127.0.0.1")).is_some());
        assert!(forbidden_ip(v6("::8.8.8.8")).is_some(), "::/96 is refused wholesale");
        // But a mapped PUBLIC v4 is fine — it is just that v4 address.
        assert!(forbidden_ip(v6("::ffff:8.8.8.8")).is_none());
    }

    #[test]
    fn six_to_four_classified_by_embedded_ipv4() {
        assert!(forbidden_ip(v6("2002:7f00:1::1")).is_some(), "6to4 embedding 127.0.0.1");
        assert!(forbidden_ip(v6("2002:a00:1::1")).is_some(), "6to4 embedding 10.0.0.1");
        assert!(forbidden_ip(v6("2002:a9fe:a9fe::1")).is_some(), "6to4 embedding 169.254.169.254");
        assert!(forbidden_ip(v6("2002:808:808::1")).is_none(), "6to4 embedding 8.8.8.8 is public");
    }

    #[test]
    fn allowed_ipv6_public_addresses() {
        for addr in [
            "2606:4700:4700::1111", // Cloudflare
            "2001:4860:4860::8888", // Google — 0x4860 is well past the 2001::/23 block
            "2620:fe::fe",
        ] {
            assert!(forbidden_ip(v6(addr)).is_none(), "{addr} must be allowed");
        }
    }

    #[test]
    fn vet_url_scheme_and_credential_rules() {
        let ok = |s: &str| vet_url(&Url::parse(s).unwrap());
        assert!(ok("https://example.com/page").is_ok());
        assert!(ok("http://example.com:8080/a?b=c").is_ok());
        // Uppercase scheme input is normalized by the parser, still http.
        assert!(ok("HTTPS://example.com/").is_ok());

        assert!(ok("ftp://example.com/file").is_err());
        assert!(ok("file:///etc/passwd").is_err());
        assert!(ok("gopher://example.com/").is_err());

        assert!(ok("http://user@example.com/").is_err(), "username must be rejected");
        assert!(ok("http://user:pass@example.com/").is_err(), "credentials must be rejected");
    }

    #[tokio::test]
    async fn vet_target_rejects_forbidden_ip_literals() {
        // IP-literal hosts never touch DNS, so this path is fully offline.
        for u in [
            "http://127.0.0.1/",
            "http://10.0.0.1:8080/x",
            "http://169.254.169.254/computeMetadata/v1/",
            "http://[::1]/",
            "http://[::ffff:169.254.169.254]/",
            "http://[fd00::1]/",
        ] {
            let url = Url::parse(u).unwrap();
            assert!(vet_target(&url).await.is_err(), "{u} must be refused");
        }
    }

    #[tokio::test]
    async fn vet_target_allows_public_ip_literals_without_pinning() {
        let url = Url::parse("http://93.184.216.34/").unwrap();
        // No DNS involved → no pin required.
        assert_eq!(vet_target(&url).await.unwrap(), None);
        let url6 = Url::parse("http://[2606:4700:4700::1111]/").unwrap();
        assert_eq!(vet_target(&url6).await.unwrap(), None);
    }
}
