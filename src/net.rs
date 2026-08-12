//! Network egress policy — the security boundary for "an agent picks the URL".
//!
//! webseek follows links chosen by search engines and by page content, so the
//! set of hosts it may reach has to be enforced by the tool, not trusted from
//! the input. Three layers cooperate:
//!
//! 1. [`check_url`] — scheme allow-list (`http`/`https` only), no `userinfo`,
//!    a host must be present, and **IP-literal hosts are checked immediately**
//!    (hyper never asks a resolver for those).
//! 2. [`redirect_policy`] — every redirect hop is re-validated with the same
//!    rules, so a public first hop cannot bounce us into `127.0.0.1` or
//!    `169.254.169.254`.
//! 3. [`GuardedResolver`] — installed as reqwest's DNS resolver, so the
//!    addresses hyper actually connects to are filtered at connect time. This
//!    is what closes the DNS-rebinding gap: re-resolution goes through the
//!    same filter.
//!
//! `--allow-private` (or `allow_private_network = true`) turns the IP filter
//! off for people who deliberately point webseek at localhost or a LAN host.
//!
//! **Proxy caveat:** when an HTTP(S) proxy is configured, the proxy resolves
//! the destination, so layer 3 cannot see the target IP. [`active_proxy_env`]
//! is used at startup (see `lib.rs::build_http`) to print a one-time note
//! when a proxy env var is set and the private-network guard is still on, so
//! this isn't a silent gap; set `allow_proxy = false` (`--no-proxy`) to force
//! direct connections and restore the full guarantee.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

use crate::error::{Error, Result};

/// What the egress layer is allowed to reach.
#[derive(Debug, Clone, Copy, Default)]
pub struct EgressPolicy {
    /// Allow loopback / private / link-local / reserved destinations.
    pub allow_private: bool,
}

impl EgressPolicy {
    pub fn permissive() -> Self {
        Self {
            allow_private: true,
        }
    }
}

/// Schemes webseek will ever fetch.
pub const ALLOWED_SCHEMES: &[&str] = &["http", "https"];

/// Validate a URL against the policy before it is used for a request.
///
/// Hostname destinations are only partly checked here (scheme/userinfo); their
/// addresses are filtered by [`GuardedResolver`] at connect time.
pub fn check_url(url: &Url, policy: EgressPolicy) -> Result<()> {
    if !ALLOWED_SCHEMES.contains(&url.scheme()) {
        return Err(Error::Blocked(format!(
            "scheme '{}' is not allowed (only http/https)",
            url.scheme()
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Blocked(
            "URLs with embedded credentials (user:pass@host) are not allowed".into(),
        ));
    }
    match url.host() {
        None => Err(Error::Blocked("URL has no host".into())),
        Some(Host::Ipv4(ip)) => check_ip(IpAddr::V4(ip), policy),
        Some(Host::Ipv6(ip)) => check_ip(IpAddr::V6(ip), policy),
        Some(Host::Domain(d)) => {
            if d.is_empty() {
                return Err(Error::Blocked("URL has an empty host".into()));
            }
            Ok(())
        }
    }
}

/// Parse and validate a user- or engine-supplied URL in one step.
pub fn parse_checked(raw: &str, policy: EgressPolicy) -> Result<Url> {
    let url = Url::parse(raw).map_err(|e| Error::Config(format!("invalid URL '{raw}': {e}")))?;
    check_url(&url, policy)?;
    Ok(url)
}

/// Validate a URL for `--open`. This only ever launches the *user's own*
/// browser/OS handler, so private-network destinations are not the risk
/// they are for an automated `fetch` — but the scheme still matters: a
/// search result or page link with `mailto:`, `file:`, or a custom
/// `ms-settings:`-style scheme should be shown, not launched, unless the
/// caller explicitly opts in.
pub fn check_open_url(url: &Url, allow_external_schemes: bool) -> Result<()> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Blocked(
            "URLs with embedded credentials (user:pass@host) are not allowed".into(),
        ));
    }
    if allow_external_schemes {
        return Ok(());
    }
    if !ALLOWED_SCHEMES.contains(&url.scheme()) {
        return Err(Error::Blocked(format!(
            "refusing to open scheme '{}' (only http/https; pass \
             --allow-external-schemes to open it anyway)",
            url.scheme()
        )));
    }
    Ok(())
}

/// Environment variables reqwest's default (non-`no_proxy`) client consults
/// to decide whether to route through a proxy. `NO_PROXY`/`no_proxy` is
/// deliberately excluded: it's an exclusion list, not an indicator that a
/// proxy is in use.
const PROXY_ENV_VARS: &[&str] = &[
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// The first proxy-indicating environment variable that is actually set and
/// non-empty, if any. Used to print a one-time startup note that the
/// private-network/DNS-rebinding checks can't see a proxy-resolved
/// destination — see this module's doc comment.
pub fn active_proxy_env() -> Option<&'static str> {
    PROXY_ENV_VARS
        .iter()
        .copied()
        .find(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
}

/// True when the destination is one we refuse to reach by default.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match normalize(ip) {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

/// Reject a single resolved address unless the policy opts out.
pub fn check_ip(ip: IpAddr, policy: EgressPolicy) -> Result<()> {
    if policy.allow_private || !is_blocked_ip(ip) {
        return Ok(());
    }
    Err(Error::Blocked(format!(
        "{ip} is a loopback/private/link-local/reserved address \
         (use --allow-private to permit it)"
    )))
}

/// Unwrap IPv4-in-IPv6 forms so a mapped address cannot smuggle 127.0.0.1 in.
fn normalize(ip: IpAddr) -> IpAddr {
    let IpAddr::V6(v6) = ip else { return ip };
    if let Some(v4) = v6.to_ipv4_mapped() {
        return IpAddr::V4(v4);
    }
    let seg = v6.segments();
    // RFC 6052 well-known NAT64 prefix. Preserve IPv6-only access to public
    // IPv4 services, but do not let a synthesized address tunnel to a blocked
    // IPv4 destination.
    if seg[0] == 0x0064
        && seg[1] == 0xff9b
        && seg[2] == 0
        && seg[3] == 0
        && seg[4] == 0
        && seg[5] == 0
    {
        return IpAddr::V4(Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            (seg[6] & 0xff) as u8,
            (seg[7] >> 8) as u8,
            (seg[7] & 0xff) as u8,
        ));
    }
    // 6to4 (2002::/16) embeds the IPv4 address in the next 32 bits.
    if seg[0] == 0x2002 {
        return IpAddr::V4(Ipv4Addr::new(
            (seg[1] >> 8) as u8,
            (seg[1] & 0xff) as u8,
            (seg[2] >> 8) as u8,
            (seg[2] & 0xff) as u8,
        ));
    }
    // Teredo (2001:0000::/32) stores the obfuscated client IPv4 last.
    if seg[0] == 0x2001 && seg[1] == 0 {
        let a = !seg[6];
        let b = !seg[7];
        return IpAddr::V4(Ipv4Addr::new(
            (a >> 8) as u8,
            (a & 0xff) as u8,
            (b >> 8) as u8,
            (b & 0xff) as u8,
        ));
    }
    // `::ffff:0:x.y.z.w` style compat addresses.
    if let Some(v4) = v6.to_ipv4() {
        if seg[0] == 0 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 && seg[4] == 0 {
            return IpAddr::V4(v4);
        }
    }
    ip
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local() // 169.254/16 — includes 169.254.169.254 metadata
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_unspecified()
        || ip.is_documentation()
        || o[0] == 0 // "this network"
        || (o[0] == 100 && (64..128).contains(&o[1])) // 100.64/10 CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0/24 IETF protocol
        || (o[0] == 192 && o[1] == 88 && o[2] == 99) // 6to4 relay anycast
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // 198.18/15 benchmarking
        || o[0] >= 240 // 240/4 reserved + 255/8
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (seg[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
        || (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        || (seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2] == 1) // local-use NAT64
        || (seg[0] == 0x2001 && seg[1] == 2) // benchmarking
        || (seg[0] == 0x2001 && (seg[1] & 0xfff0) == 0x0010) // ORCHIDv1
        || (seg[0] == 0x2001 && (seg[1] & 0xfff0) == 0x0020) // ORCHIDv2
        || (seg[0] == 0x2001 && seg[1] == 0x0db8) // 2001:db8::/32 documentation
        || (seg[0] & 0xfff0) == 0x3ff0 // 3fff::/20 documentation
        || seg[0] == 0x5f00 // 5f00::/16 segment-routing SIDs
        || (seg[0] == 0x0100 && seg[1] == 0) // 100::/64 discard-only
}

/// Redirect policy that re-checks every hop and caps the chain length.
pub fn redirect_policy(policy: EgressPolicy, max_hops: usize) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= max_hops {
            return attempt.error(format!("too many redirects (>{max_hops})"));
        }
        match check_url(attempt.url(), policy) {
            Ok(()) => attempt.follow(),
            Err(e) => attempt.error(e.to_string()),
        }
    })
}

/// DNS resolver that drops (and, if nothing is left, refuses) blocked addresses.
pub struct GuardedResolver {
    policy: EgressPolicy,
}

impl GuardedResolver {
    pub fn new(policy: EgressPolicy) -> Arc<Self> {
        Arc::new(Self { policy })
    }
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        let allow_private = self.policy.allow_private;
        Box::pin(async move {
            let lookup = tokio::task::spawn_blocking(move || {
                // Port 0: reqwest replaces it with the scheme's port.
                (host.as_str(), 0u16)
                    .to_socket_addrs()
                    .map(|it| it.collect::<Vec<SocketAddr>>())
                    .map_err(|e| format!("cannot resolve '{host}': {e}"))
                    .and_then(|addrs| filter_addrs(&host, addrs, allow_private))
            })
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::from(e.to_string()) })?
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::from(e) })?;
            let addrs: Addrs = Box::new(lookup.into_iter());
            Ok(addrs)
        })
    }
}

/// Keep only addresses the policy permits; an all-blocked host is an error so
/// the caller sees "blocked", not a confusing connection failure.
fn filter_addrs(
    host: &str,
    addrs: Vec<SocketAddr>,
    allow_private: bool,
) -> std::result::Result<Vec<SocketAddr>, String> {
    if allow_private {
        return Ok(addrs);
    }
    let total = addrs.len();
    let kept: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|a| !is_blocked_ip(a.ip()))
        .collect();
    if kept.is_empty() && total > 0 {
        return Err(format!(
            "'{host}' resolves only to loopback/private/link-local/reserved \
             addresses; refusing to connect (use --allow-private to permit it)"
        ));
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn strict() -> EgressPolicy {
        EgressPolicy::default()
    }

    #[test]
    fn rejects_non_http_schemes() {
        for raw in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "gopher://example.com",
            "javascript:alert(1)",
            "data:text/html,hi",
            "ms-settings:x",
        ] {
            let err = parse_checked(raw, strict()).unwrap_err();
            assert_eq!(err.kind(), "blocked_by_policy", "{raw} -> {err}");
        }
    }

    #[test]
    fn rejects_userinfo() {
        let err = parse_checked("https://user:pw@example.com/", strict()).unwrap_err();
        assert!(err.to_string().contains("credentials"), "{err}");
        let err = parse_checked("https://user@example.com/", strict()).unwrap_err();
        assert!(err.to_string().contains("credentials"), "{err}");
    }

    #[test]
    fn rejects_ip_literal_hosts_in_blocked_ranges() {
        for raw in [
            "http://127.0.0.1:8080/x",
            "http://[::1]/x",
            "http://10.0.0.5/x",
            "http://192.168.1.1/x",
            "http://172.16.9.9/x",
            "http://169.254.169.254/latest/meta-data/",
            "http://0.0.0.0/",
            "http://100.100.100.200/",    // Alibaba metadata (CGNAT range)
            "http://[fd00:ec2::254]/",    // AWS IPv6 metadata (unique local)
            "http://[::ffff:127.0.0.1]/", // IPv4-mapped loopback
            "http://[64:ff9b::7f00:1]/",  // NAT64-wrapped loopback
            "http://[64:ff9b:1::1]/",     // local-use NAT64 prefix
            "http://[2002:7f00:1::]/",    // 6to4-wrapped loopback
        ] {
            let err = parse_checked(raw, strict()).unwrap_err();
            assert_eq!(err.kind(), "blocked_by_policy", "{raw} should be blocked");
        }
    }

    #[test]
    fn allow_private_opens_the_gate() {
        let p = EgressPolicy::permissive();
        assert!(parse_checked("http://127.0.0.1:8080/x", p).is_ok());
        assert!(parse_checked("http://[::1]/x", p).is_ok());
        // ...but never the scheme allow-list.
        assert!(parse_checked("file:///etc/passwd", p).is_err());
    }

    #[test]
    fn open_url_blocks_non_http_schemes_by_default() {
        for raw in [
            "file:///etc/passwd",
            "mailto:x@example.com",
            "ms-settings:x",
        ] {
            let url = Url::parse(raw).unwrap();
            assert!(check_open_url(&url, false).is_err(), "{raw}");
            assert!(check_open_url(&url, true).is_ok(), "{raw} with override");
        }
    }

    #[test]
    fn open_url_allows_http_and_still_blocks_credentials() {
        let url = Url::parse("https://example.com/x").unwrap();
        assert!(check_open_url(&url, false).is_ok());
        let url = Url::parse("https://user:pw@example.com/x").unwrap();
        assert!(check_open_url(&url, false).is_err());
    }

    #[test]
    fn open_url_does_not_block_private_ips() {
        // Opening the user's own browser to their own router is not SSRF.
        let url = Url::parse("http://192.168.1.1/").unwrap();
        assert!(check_open_url(&url, false).is_ok());
    }

    #[test]
    fn public_hosts_pass() {
        for raw in [
            "https://example.com/a?b=c#d",
            "http://8.8.8.8/",
            "https://[2606:4700::1111]/",
        ] {
            assert!(parse_checked(raw, strict()).is_ok(), "{raw}");
        }
    }

    #[test]
    fn filter_addrs_drops_blocked_and_errors_when_empty() {
        let mixed = vec![
            "127.0.0.1:80".parse().unwrap(),
            "93.184.216.34:80".parse().unwrap(),
        ];
        let kept = filter_addrs("mixed.test", mixed, false).unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].ip().to_string(), "93.184.216.34");

        let all_private: Vec<SocketAddr> = vec!["10.0.0.1:80".parse().unwrap()];
        assert!(filter_addrs("lan.test", all_private, false).is_err());

        // With allow_private the list is untouched.
        let all_private: Vec<SocketAddr> = vec!["10.0.0.1:80".parse().unwrap()];
        assert_eq!(
            filter_addrs("lan.test", all_private, true).unwrap().len(),
            1
        );
    }

    #[test]
    fn empty_resolution_is_not_reported_as_blocked() {
        // A genuinely empty lookup must not be turned into a policy error.
        assert!(filter_addrs("nowhere.test", Vec::new(), false)
            .unwrap()
            .is_empty());
    }

    // PROXY_ENV_VARS is process-global state; serialize tests that touch it
    // so they can't observe each other's env var mutations.
    static PROXY_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn active_proxy_env_detects_any_known_var() {
        let _guard = PROXY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for var in PROXY_ENV_VARS {
            std::env::remove_var(var);
        }
        assert_eq!(active_proxy_env(), None);

        std::env::set_var("HTTPS_PROXY", "http://proxy.example:8080");
        assert_eq!(active_proxy_env(), Some("HTTPS_PROXY"));
        std::env::remove_var("HTTPS_PROXY");

        // An empty value doesn't count as "set".
        std::env::set_var("http_proxy", "");
        assert_eq!(active_proxy_env(), None);
        std::env::remove_var("http_proxy");
    }
}
