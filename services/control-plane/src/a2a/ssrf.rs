//! SSRF-guarded webhook-URL validation for A2A push notifications (RFC-0006 Phase 3, ADR-0079 §2).
//!
//! A push-notification webhook URL is **untrusted caller input that points our in-cluster pod at an
//! arbitrary address** — the control plane's first *outbound* egress to a caller-controlled
//! destination, and therefore a server-side request forgery (SSRF) primitive. A caller could
//! register `https://10.0.0.5/…`, the cloud-metadata IP `169.254.169.254`, or an in-cluster Service
//! and turn our delivery pod into a probe into the cluster and cloud fabric.
//!
//! This module is the **app-layer** half of the defence (a deny-internal NetworkPolicy on the
//! delivery pod is the network-layer half, wired in the deploy slice). It is deliberately **pure and
//! exhaustively testable**: the DNS resolver is *injected* so tests feed synthetic hostname→IP maps
//! and literal-IP URLs need no DNS at all. The same validator runs at **registration** and at
//! **every delivery attempt** (DNS-rebinding / TOCTOU defence — ADR-0079 §2, P2).
//!
//! The blocked ranges are **hand-rolled and auditable on purpose** (ADR-0079's load-bearing
//! mitigation): a reviewer must be able to see every range. `ipnet` backs *only* the operator-
//! supplied extra deny-list (cluster Service/Pod CIDRs); the fixed ranges never hide behind a crate.
//!
//! SLICE NOTE: slice 1 lands the validator + its tests only. Wiring it into the `create` handler and
//! the delivery client is slice 2; nothing here is called from the request path yet.

// Slice 1 intentionally lands this module unused — its callers (the `create` handler and the
// delivery client) arrive in slice 2. The `#[cfg(test)]` suite fully exercises it now; the
// non-test build has no caller yet, so silence dead-code until slice 2 wires it in.
#![allow(dead_code)]

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

use ipnet::IpNet;
use url::{Host, Url};

/// The only HTTPS port accepted for now. A caller needing a non-standard HTTPS port is an explicit
/// future allowlist knob (ADR-0079 §2 / Out of scope), not a silent widening.
const ALLOWED_PORT: u16 = 443;

/// The SSRF policy. The **fixed** blocked ranges (loopback, RFC 1918, link-local + metadata, ULA,
/// …) are hand-rolled in [`is_blocked`] and are *not* configurable — they are always denied. This
/// struct carries only the **operator-supplied extra deny-list**: the cluster's own Service/Pod
/// CIDRs (and anything else the deploy wants blocked), wired via config in a later slice. Default is
/// empty (the fixed ranges still apply).
#[derive(Debug, Clone, Default)]
pub struct SsrfPolicy {
    /// Extra CIDRs to deny on top of the fixed ranges (e.g. `10.42.0.0/16` Pod CIDR,
    /// `10.43.0.0/16` Service CIDR). Placeholder-empty here; the real values are config-supplied.
    pub extra_denied_cidrs: Vec<IpNet>,
}

impl SsrfPolicy {
    /// A policy denying the fixed ranges plus the given extra CIDRs.
    pub fn with_extra_denied(extra_denied_cidrs: Vec<IpNet>) -> Self {
        Self { extra_denied_cidrs }
    }
}

/// A webhook URL that passed the full SSRF policy. `pinned_ips` is the set the host resolved to,
/// **every one of which is public** — slice 2's delivery client connects to a *pinned* address from
/// this set rather than re-resolving (so the check and the socket target are the same address).
#[derive(Debug, Clone)]
pub struct ValidatedWebhook {
    /// The parsed, validated URL (HTTPS, port 443, host present).
    pub url: Url,
    /// The resolved, all-public addresses to pin the connect to (DNS-rebinding defence).
    pub pinned_ips: Vec<IpAddr>,
}

/// Why a webhook URL was rejected. Variants are precise so the handler can map them to a clear
/// caller-facing error (slice 2) and so tests assert the exact failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SsrfError {
    /// The string did not parse as a URL at all.
    InvalidUrl,
    /// The scheme was not exactly `https` (e.g. `http`, `file`, `ftp`, `gopher`).
    NotHttps,
    /// The URL had no host component.
    MissingHost,
    /// The port was present and not 443.
    DisallowedPort(u16),
    /// The host could not be resolved (DNS failure / injected resolver error).
    Resolution,
    /// A resolved (or literal) address fell in a blocked range — the offending address is carried.
    BlockedAddress(IpAddr),
    /// The host resolved to zero addresses.
    NoAddresses,
}

impl fmt::Display for SsrfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SsrfError::InvalidUrl => write!(f, "webhook URL did not parse"),
            SsrfError::NotHttps => write!(f, "webhook URL must use the https scheme"),
            SsrfError::MissingHost => write!(f, "webhook URL has no host"),
            SsrfError::DisallowedPort(p) => {
                write!(f, "webhook URL port {p} is not allowed (only 443)")
            }
            SsrfError::Resolution => write!(f, "webhook host could not be resolved"),
            SsrfError::BlockedAddress(ip) => {
                write!(f, "webhook host resolves to a blocked address {ip}")
            }
            SsrfError::NoAddresses => write!(f, "webhook host resolved to no addresses"),
        }
    }
}

impl std::error::Error for SsrfError {}

/// Validate a caller-supplied webhook URL against the SSRF policy.
///
/// Checks, in order: parses → scheme is exactly `https` → host present → port is 443 → host resolves
/// → **every** resolved address is public. If *any* address is blocked the whole URL is rejected
/// (never cherry-pick a public one — a host resolving to `[public, private]` is rejected).
///
/// The `resolve` closure is injected: a literal-IP URL is checked directly with **no** resolver
/// call; only a domain triggers `resolve(domain)`. Production callers pass [`system_resolver`].
pub fn validate_webhook_url(
    raw: &str,
    policy: &SsrfPolicy,
    resolve: impl Fn(&str) -> std::io::Result<Vec<IpAddr>>,
) -> Result<ValidatedWebhook, SsrfError> {
    let url = Url::parse(raw).map_err(|_| SsrfError::InvalidUrl)?;

    // Scheme must be exactly `https` — reject `http` (plaintext also exposes the auth token) and any
    // other scheme (`file`, `ftp`, `gopher`, …).
    if url.scheme() != "https" {
        return Err(SsrfError::NotHttps);
    }

    // Host must be present.
    let host = url.host().ok_or(SsrfError::MissingHost)?;

    // Port must be 443. `port_or_known_default` yields 443 when the URL omits the port (the https
    // default) and the explicit port otherwise.
    let port = url.port_or_known_default().unwrap_or(ALLOWED_PORT);
    if port != ALLOWED_PORT {
        return Err(SsrfError::DisallowedPort(port));
    }

    // Resolve to a concrete address set. A literal IP is its own address (no DNS); a domain is
    // resolved through the injected resolver.
    let addrs: Vec<IpAddr> = match host {
        Host::Ipv4(v4) => vec![IpAddr::V4(v4)],
        Host::Ipv6(v6) => vec![IpAddr::V6(v6)],
        Host::Domain(name) => {
            if name.is_empty() {
                return Err(SsrfError::MissingHost);
            }
            resolve(name).map_err(|_| SsrfError::Resolution)?
        }
    };

    if addrs.is_empty() {
        return Err(SsrfError::NoAddresses);
    }

    // Every resolved address must be public. ANY blocked address rejects the whole URL — we never
    // cherry-pick a public address out of a mixed set (a DNS-rebinding / split-horizon vector).
    for ip in &addrs {
        if is_blocked(*ip, policy) {
            return Err(SsrfError::BlockedAddress(*ip));
        }
    }

    Ok(ValidatedWebhook {
        url,
        pinned_ips: addrs,
    })
}

/// True if `ip` falls in any blocked range: the fixed, hand-rolled table below, or the operator's
/// extra deny-list. An IPv4-mapped/compatible IPv6 address is unwrapped and re-checked against the
/// IPv4 table (a classic bypass), and the extra deny-list is also tested against the embedded v4.
fn is_blocked(ip: IpAddr, policy: &SsrfPolicy) -> bool {
    let fixed_blocked = match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    };
    if fixed_blocked {
        return true;
    }

    // Operator extra deny-list (cluster Service/Pod CIDRs, etc.) — check the address itself…
    if policy
        .extra_denied_cidrs
        .iter()
        .any(|net| net.contains(&ip))
    {
        return true;
    }
    // …and, for a v4-in-v6 form, the embedded v4 too, so `::ffff:<cluster-ip>` cannot slip past a
    // v4 CIDR entry in the extra list.
    if let IpAddr::V6(v6) = ip {
        if let Some(v4) = v6.to_ipv4() {
            let mapped = IpAddr::V4(v4);
            if policy
                .extra_denied_cidrs
                .iter()
                .any(|n| n.contains(&mapped))
            {
                return true;
            }
        }
    }

    false
}

/// The fixed IPv4 deny table (ADR-0079 §2). Every range is spelled out explicitly so a reviewer can
/// audit it; `std` helpers are used where they map 1:1 to a listed range and hand-rolled otherwise.
fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();

    // 0.0.0.0/8 — "this network" (includes the 0.0.0.0 unspecified address).
    if o[0] == 0 {
        return true;
    }
    // 127.0.0.0/8 — loopback.
    if ip.is_loopback() {
        return true;
    }
    // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16 — RFC 1918 private.
    if ip.is_private() {
        return true;
    }
    // 169.254.0.0/16 — link-local, which INCLUDES the cloud-metadata IP 169.254.169.254.
    if ip.is_link_local() {
        return true;
    }
    // 100.64.0.0/10 — CGNAT (RFC 6598): first octet 100, second octet 64..=127.
    if o[0] == 100 && (64..=127).contains(&o[1]) {
        return true;
    }
    // 198.18.0.0/15 — benchmarking (RFC 2544): 198.18.x and 198.19.x.
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return true;
    }
    // 224.0.0.0/4 — multicast.
    if ip.is_multicast() {
        return true;
    }
    // 255.255.255.255 — limited broadcast.
    if ip.is_broadcast() {
        return true;
    }
    // 240.0.0.0/4 — reserved / future use (also covers the 255.255.255.255 broadcast above).
    if o[0] >= 240 {
        return true;
    }

    false
}

/// The fixed IPv6 deny table (ADR-0079 §2). ULA and link-local need hand-rolled prefix masks (the
/// `std` helpers for them are unstable); the IPv4-mapped/compatible forms are unwrapped and run back
/// through the IPv4 table.
fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    // :: — unspecified.
    if ip.is_unspecified() {
        return true;
    }
    // ::1 — loopback.
    if ip.is_loopback() {
        return true;
    }
    // ff00::/8 — multicast.
    if ip.is_multicast() {
        return true;
    }

    let seg = ip.segments();

    // fc00::/7 — unique-local (ULA): top 7 bits are 1111110.
    if (seg[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // fe80::/10 — link-local: top 10 bits are 1111111010.
    if (seg[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // 2001:db8::/32 — documentation.
    if seg[0] == 0x2001 && seg[1] == 0x0db8 {
        return true;
    }

    // ::ffff:0:0/96 IPv4-mapped — the classic bypass: unwrap and re-run the IPv4 table so
    // ::ffff:10.0.0.1 is blocked exactly like 10.0.0.1.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_v4(v4);
    }
    // Deprecated ::/96 IPv4-compatible form (::a.b.c.d) — cheap to close the twin bypass; `to_ipv4`
    // covers both mapped (handled above) and compatible. A normal global v6 yields `None` here.
    if let Some(v4) = ip.to_ipv4() {
        return is_blocked_v4(v4);
    }

    false
}

/// Production DNS resolver: resolve `host` to its address set via the system resolver. The port is
/// irrelevant to the address records, so 443 is a placeholder for `to_socket_addrs`. Slice 2's
/// delivery client re-runs [`validate_webhook_url`] with this resolver and connects to a pinned
/// result address (never a fresh re-resolution).
pub fn system_resolver(host: &str) -> std::io::Result<Vec<IpAddr>> {
    (host, ALLOWED_PORT)
        .to_socket_addrs()
        .map(|iter| iter.map(|sa| sa.ip()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// A resolver that must never be called — used for literal-IP URLs (which need no DNS) and for
    /// checks that reject before resolution (scheme/port).
    fn never_resolve(_host: &str) -> std::io::Result<Vec<IpAddr>> {
        panic!("resolver must not be called for this input");
    }

    /// A resolver returning a fixed address set for any host.
    fn resolver_to(addrs: Vec<IpAddr>) -> impl Fn(&str) -> std::io::Result<Vec<IpAddr>> {
        move |_host| Ok(addrs.clone())
    }

    fn public_v4() -> IpAddr {
        // 93.184.216.34 (example.com) — not in any blocked range.
        IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))
    }

    fn validate(raw: &str) -> Result<ValidatedWebhook, SsrfError> {
        validate_webhook_url(raw, &SsrfPolicy::default(), never_resolve)
    }

    // ---- accept paths -------------------------------------------------------------------------

    #[test]
    fn accepts_public_domain_and_pins_the_ip() {
        let out = validate_webhook_url(
            "https://hooks.example.com/a2a",
            &SsrfPolicy::default(),
            resolver_to(vec![public_v4()]),
        )
        .expect("public host accepted");
        assert_eq!(out.pinned_ips, vec![public_v4()]);
        assert_eq!(out.url.scheme(), "https");
    }

    #[test]
    fn accepts_literal_public_ip_without_dns() {
        // never_resolve panics if called — proves a literal IP takes no DNS.
        let out = validate("https://93.184.216.34/hook").expect("literal public IP accepted");
        assert_eq!(out.pinned_ips, vec![public_v4()]);
    }

    #[test]
    fn accepts_explicit_443() {
        validate("https://93.184.216.34:443/hook").expect("explicit :443 accepted");
    }

    // ---- scheme / structure -------------------------------------------------------------------

    #[test]
    fn rejects_http() {
        assert_eq!(
            validate("http://example.com/").unwrap_err(),
            SsrfError::NotHttps
        );
    }

    #[test]
    fn rejects_other_schemes() {
        for raw in [
            "ftp://example.com/",
            "file:///etc/passwd",
            "gopher://example.com/",
        ] {
            assert_eq!(validate(raw).unwrap_err(), SsrfError::NotHttps, "{raw}");
        }
    }

    #[test]
    fn rejects_malformed_url() {
        for raw in ["not a url", "://nope", "https://exa mple.com/"] {
            assert_eq!(validate(raw).unwrap_err(), SsrfError::InvalidUrl, "{raw}");
        }
    }

    #[test]
    fn rejects_host_less_url() {
        // A special scheme (`https`) REQUIRES a host: the `url` crate rejects an empty authority at
        // parse time, so a host-less URL surfaces as InvalidUrl. (`https:///path` is NOT host-less —
        // WHATWG collapses the empty authority and reads `path` as the host — so it is not this
        // case.) The `MissingHost` variant is a defensive guard for a parsed `host() == None` /
        // empty-domain URL, which https input cannot actually produce; both reject, never accept.
        assert_eq!(validate("https://").unwrap_err(), SsrfError::InvalidUrl);
        assert_eq!(
            validate("https://:443/x").unwrap_err(),
            SsrfError::InvalidUrl
        );
    }

    #[test]
    fn rejects_non_443_port() {
        assert_eq!(
            validate("https://93.184.216.34:8443/hook").unwrap_err(),
            SsrfError::DisallowedPort(8443)
        );
        assert_eq!(
            validate("https://example.com:80/hook").unwrap_err(),
            SsrfError::DisallowedPort(80)
        );
    }

    // ---- resolution edge cases ----------------------------------------------------------------

    #[test]
    fn resolver_error_is_resolution() {
        let err = validate_webhook_url("https://example.com/", &SsrfPolicy::default(), |_h| {
            Err(std::io::Error::new(std::io::ErrorKind::Other, "boom"))
        })
        .unwrap_err();
        assert_eq!(err, SsrfError::Resolution);
    }

    #[test]
    fn empty_resolution_is_no_addresses() {
        let err = validate_webhook_url(
            "https://example.com/",
            &SsrfPolicy::default(),
            resolver_to(vec![]),
        )
        .unwrap_err();
        assert_eq!(err, SsrfError::NoAddresses);
    }

    // ---- any-blocked-rejects ------------------------------------------------------------------

    #[test]
    fn any_blocked_address_rejects_the_whole_set() {
        let private = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        // [public, private] and [private, public] both reject — never cherry-pick the public one.
        let a = validate_webhook_url(
            "https://mixed.example.com/",
            &SsrfPolicy::default(),
            resolver_to(vec![public_v4(), private]),
        )
        .unwrap_err();
        assert_eq!(a, SsrfError::BlockedAddress(private));
        let b = validate_webhook_url(
            "https://mixed.example.com/",
            &SsrfPolicy::default(),
            resolver_to(vec![private, public_v4()]),
        )
        .unwrap_err();
        assert_eq!(b, SsrfError::BlockedAddress(private));
    }

    // ---- IPv4 blocked ranges (literal IP, no DNS) ---------------------------------------------

    /// Assert a literal-IP webhook URL is rejected as a blocked address (resolver never called).
    fn assert_v4_blocked(a: u8, b: u8, c: u8, d: u8) {
        let raw = format!("https://{a}.{b}.{c}.{d}/hook");
        let err = validate(&raw).unwrap_err();
        assert_eq!(
            err,
            SsrfError::BlockedAddress(IpAddr::V4(Ipv4Addr::new(a, b, c, d))),
            "{raw} should be blocked"
        );
    }

    #[test]
    fn blocks_ipv4_ranges() {
        assert_v4_blocked(0, 0, 0, 0); // 0.0.0.0/8 unspecified / this-network
        assert_v4_blocked(0, 1, 2, 3); // 0.0.0.0/8 non-zero
        assert_v4_blocked(127, 0, 0, 1); // loopback
        assert_v4_blocked(10, 0, 0, 1); // 10/8 private
        assert_v4_blocked(172, 16, 5, 4); // 172.16/12 private
        assert_v4_blocked(172, 31, 255, 254); // 172.16/12 upper edge
        assert_v4_blocked(192, 168, 1, 1); // 192.168/16 private
        assert_v4_blocked(169, 254, 0, 1); // 169.254/16 link-local
        assert_v4_blocked(100, 64, 0, 1); // 100.64/10 CGNAT lower edge
        assert_v4_blocked(100, 127, 255, 254); // 100.64/10 CGNAT upper edge
        assert_v4_blocked(198, 18, 0, 1); // 198.18/15 benchmarking
        assert_v4_blocked(198, 19, 0, 1); // 198.18/15 benchmarking
        assert_v4_blocked(224, 0, 0, 1); // 224/4 multicast
        assert_v4_blocked(239, 255, 255, 255); // 224/4 multicast upper edge
        assert_v4_blocked(240, 0, 0, 1); // 240/4 reserved
        assert_v4_blocked(255, 255, 255, 255); // limited broadcast
    }

    /// The cloud-metadata IP specifically — the single most important address to block.
    #[test]
    fn blocks_cloud_metadata_ip() {
        assert_v4_blocked(169, 254, 169, 254);
    }

    /// A CGNAT-adjacent but PUBLIC address must NOT be blocked (guards against an over-broad mask):
    /// 100.63.x is below 100.64/10, and 100.128.x is above it.
    #[test]
    fn allows_cgnat_adjacent_public() {
        validate("https://100.63.255.255/hook").expect("100.63/x is public");
        validate("https://100.128.0.1/hook").expect("100.128/x is public");
    }

    // ---- IPv6 blocked ranges ------------------------------------------------------------------

    fn assert_v6_blocked(literal: &str, expect: Ipv6Addr) {
        let raw = format!("https://[{literal}]/hook");
        let err = validate(&raw).unwrap_err();
        assert_eq!(err, SsrfError::BlockedAddress(IpAddr::V6(expect)), "{raw}");
    }

    #[test]
    fn blocks_ipv6_ranges() {
        assert_v6_blocked("::", Ipv6Addr::UNSPECIFIED); // unspecified
        assert_v6_blocked("::1", Ipv6Addr::LOCALHOST); // loopback
        assert_v6_blocked("fc00::1", "fc00::1".parse().unwrap()); // ULA lower
        assert_v6_blocked("fdff::1", "fdff::1".parse().unwrap()); // ULA upper (fc00::/7)
        assert_v6_blocked("fe80::1", "fe80::1".parse().unwrap()); // link-local
        assert_v6_blocked("febf::1", "febf::1".parse().unwrap()); // link-local upper (fe80::/10)
        assert_v6_blocked("ff02::1", "ff02::1".parse().unwrap()); // multicast
        assert_v6_blocked("2001:db8::1", "2001:db8::1".parse().unwrap()); // documentation
    }

    #[test]
    fn allows_public_ipv6() {
        // 2606:4700:4700::1111 (Cloudflare) — global unicast, not in any blocked range.
        validate("https://[2606:4700:4700::1111]/hook").expect("public v6 accepted");
    }

    // ---- IPv4-mapped / compatible IPv6 bypass -------------------------------------------------

    #[test]
    fn blocks_ipv4_mapped_private_and_metadata() {
        // ::ffff:10.0.0.1 and ::ffff:169.254.169.254 must be blocked exactly like the bare v4.
        let raw = "https://[::ffff:10.0.0.1]/hook";
        assert!(
            matches!(validate(raw).unwrap_err(), SsrfError::BlockedAddress(_)),
            "{raw}"
        );
        let meta = "https://[::ffff:169.254.169.254]/hook";
        assert!(
            matches!(validate(meta).unwrap_err(), SsrfError::BlockedAddress(_)),
            "{meta}"
        );
    }

    // ---- configurable extra deny-list ---------------------------------------------------------

    #[test]
    fn extra_cidr_blocks_otherwise_public_address() {
        // 203.0.113.0/24 (TEST-NET-3) is public per the fixed table; a simulated cluster CIDR entry
        // must block 203.0.113.5, while the default policy accepts it.
        validate("https://203.0.113.5/hook").expect("public by fixed table");

        let policy = SsrfPolicy::with_extra_denied(vec!["203.0.113.0/24".parse().unwrap()]);
        let err =
            validate_webhook_url("https://203.0.113.5/hook", &policy, never_resolve).unwrap_err();
        assert_eq!(
            err,
            SsrfError::BlockedAddress(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)))
        );
    }

    #[test]
    fn extra_cidr_also_blocks_ipv4_mapped_form() {
        // The mapped form ::ffff:203.0.113.5 must not slip past a v4 extra-CIDR entry.
        let policy = SsrfPolicy::with_extra_denied(vec!["203.0.113.0/24".parse().unwrap()]);
        let err = validate_webhook_url("https://[::ffff:203.0.113.5]/hook", &policy, never_resolve)
            .unwrap_err();
        assert!(matches!(err, SsrfError::BlockedAddress(_)));
    }

    // ---- display ------------------------------------------------------------------------------

    #[test]
    fn error_display_is_clear() {
        assert_eq!(
            SsrfError::NotHttps.to_string(),
            "webhook URL must use the https scheme"
        );
        assert_eq!(
            SsrfError::DisallowedPort(8443).to_string(),
            "webhook URL port 8443 is not allowed (only 443)"
        );
        assert_eq!(
            SsrfError::BlockedAddress(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))).to_string(),
            "webhook host resolves to a blocked address 10.0.0.1"
        );
    }
}
