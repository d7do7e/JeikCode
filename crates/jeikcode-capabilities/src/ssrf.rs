//! Shared SSRF protection helpers for outbound HTTP fetches.
//!
//! Rejects IPs pointing at loopback, private networks (RFC1918), link-local /
//! cloud-metadata (169.254.0.0/16, fe80::/10), CGNAT (100.64/10), reserved
//! ranges, IPv6 ULA (fc00::/7), and embedded IPv4 addresses inside IPv6.

use std::net::{IpAddr, SocketAddr};

/// Reject IPs pointing at the host / local network / cloud metadata (loopback, RFC1918,
/// link-local 169.254/CGNAT/reserved, IPv6 ULA/link-local, IPv4-mapped/compatible v6).
pub fn is_safe_ip(ip: IpAddr) -> Result<(), String> {
    let reject = |cat: &str| {
        Err(format!(
            "refusing to connect to {ip} ({cat}) — SSRF protection"
        ))
    };
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                return reject("loopback 127.0.0.0/8");
            }
            if v4.is_private() {
                return reject("private network");
            }
            if v4.is_link_local() {
                return reject("link-local / cloud metadata");
            }
            if v4.is_broadcast() {
                return reject("broadcast");
            }
            if v4.is_multicast() {
                return reject("multicast");
            }
            if v4.is_unspecified() {
                return reject("unspecified 0.0.0.0");
            }
            let o = v4.octets();
            if o[0] == 0 {
                return reject("reserved 0.0.0.0/8");
            }
            if o[0] >= 240 {
                return reject("reserved 240.0.0.0/4");
            }
            if o[0] == 100 && (o[1] & 0xc0) == 64 {
                return reject("CGNAT 100.64/10");
            }
            Ok(())
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return reject("loopback ::1");
            }
            if v6.is_unspecified() {
                return reject("unspecified ::");
            }
            if v6.is_multicast() {
                return reject("multicast");
            }
            let first = v6.segments()[0];
            if (first & 0xfe00) == 0xfc00 {
                return reject("unique-local fc00::/7");
            }
            if (first & 0xffc0) == 0xfe80 {
                return reject("link-local fe80::/10");
            }
            // Embedded IPv4 — the OS may dial the v4 directly, so both IPv4-mapped
            // (`::ffff:a.b.c.d`) and IPv4-compatible (`::a.b.c.d`) forms must be checked.
            if let Some(embedded) = v6.to_ipv4() {
                return is_safe_ip(IpAddr::V4(embedded));
            }
            Ok(())
        }
    }
}

/// Resolve `host:port` and require EVERY returned IP to pass [`is_safe_ip`].
/// Returns the validated [`SocketAddr`]s so callers can pin them into `reqwest`
/// (`resolve_to_addrs`), closing the DNS-rebinding TOCTOU window. Returns an
/// empty `Vec` for a literal-IP host (no DNS to rebind).
pub async fn validate_host_port(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let clean_host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = clean_host.parse::<IpAddr>() {
        is_safe_ip(ip)?;
        return Ok(Vec::new());
    }
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((clean_host, port))
        .await
        .map_err(|e| format!("DNS resolution failed for `{clean_host}`: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("DNS returned no addresses for `{clean_host}`"));
    }
    for addr in &addrs {
        is_safe_ip(addr.ip())?;
    }
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_private_loopback_metadata_and_embedded_v6() {
        for bad in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1",
            "::169.254.169.254",
        ] {
            let ip: IpAddr = bad.parse().unwrap();
            assert!(is_safe_ip(ip).is_err(), "{bad} must be rejected");
        }
        assert!(is_safe_ip("93.184.216.34".parse().unwrap()).is_ok());
        assert!(is_safe_ip("2606:2800:220:1:248:1893:25c8:1946".parse().unwrap()).is_ok());
    }
}
