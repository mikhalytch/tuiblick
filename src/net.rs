//! Default route + public/exit-IP probing (spec §6).
//!
//! - Default route (platform-specific, spec §6):
//!   macOS: `route -n get default` → parse `gateway:` + `interface:`.
//!   Fallback: `route -n get 1.1.1.1`.
//!   Linux (and other non-macOS): `ip route show default` → parse
//!   `via <gateway>` + `dev <iface>` on the first `default` line.
//!   Fallback: `ip route get 1.1.1.1` (same token scan; tunnel routes
//!   without a `via` hop parse as `None`).
//!   Failure → [`AppError::NetProbe`] (the caller in Task 7 shows `n/a`);
//!   never blocks beyond timeouts.
//! - Public/exit IP: `curl -s --max-time 5 https://api.ipify.org` via
//!   `tokio::process` (no reqwest/TLS in tree). Fallback:
//!   `dig +short +time=3 +tries=1 myip.opendns.com @208.67.222.222`.
//!   Total budget ≤ 8 s; failure → [`AppError::NetProbe`] (caller shows
//!   `unknown` with the reason in the status bar).
//! - All commands run unprivileged via argv arrays (no shell, no sudo) and
//!   are wrapped in `tokio::time::timeout`, so no probe ever blocks input
//!   rendering: the UI shows `resolving…` meanwhile (Task 7/8).

use std::time::Duration;

use crate::errors::AppError;

/// Parsed default route: gateway address + egress interface name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteInfo {
    pub gateway: String,
    pub iface: String,
}

/// Per-command timeout for `route` probes.
const ROUTE_TIMEOUT: Duration = Duration::from_secs(3);
/// `curl --max-time` budget (plus 1 s scheduling margin in the timeout).
const CURL_TIMEOUT: Duration = Duration::from_secs(6);
/// `dig +time=3 +tries=1` budget (plus 1 s scheduling margin).
const DIG_TIMEOUT: Duration = Duration::from_secs(4);
/// Total budget for [`get_public_ip`] (curl + dig combined, spec §6).
const PUBLIC_IP_TOTAL_TIMEOUT: Duration = Duration::from_secs(8);

/// Parse macOS `route -n get <dst>` output.
///
/// Looks for `gateway: <addr>` and `interface: <name>` lines: the key is
/// matched ASCII case-insensitively, leading whitespace is ignored, and all
/// other lines are skipped. Both fields are required — anything else yields
/// `None` (the caller then tries the fallback route target).
pub fn parse_route_get_default(out: &str) -> Option<RouteInfo> {
    let mut gateway: Option<String> = None;
    let mut iface: Option<String> = None;
    for line in out.lines() {
        // Split on the first `:`; lines without one (headers, stats) are ignored.
        let Some(colon) = line.find(':') else {
            continue;
        };
        let key = line[..colon].trim();
        let value = line[colon + 1..].trim();
        if value.is_empty() {
            continue;
        }
        if gateway.is_none() && key.eq_ignore_ascii_case("gateway") {
            gateway = Some(value.to_string());
        } else if iface.is_none() && key.eq_ignore_ascii_case("interface") {
            iface = Some(value.to_string());
        }
    }
    Some(RouteInfo {
        gateway: gateway?,
        iface: iface?,
    })
}

/// Parse Linux `ip route show default` output.
///
/// Scans for the first line whose first whitespace-separated token is
/// `default`, then reads the `via <gateway>` and `dev <iface>` tokens.
/// Both are required — anything else yields `None` (the caller then tries
/// the `ip route get` fallback). Extra tokens (`proto`, `src`, `metric`,
/// …) are ignored, so distro-specific columns never break the parse.
///
/// Example: `default via 192.168.1.1 dev wlo1 proto dhcp src 192.168.1.12 metric 20600`
pub fn parse_ip_route_show_default(out: &str) -> Option<RouteInfo> {
    for line in out.lines() {
        let mut tokens = line.split_whitespace();
        if tokens.next() != Some("default") {
            continue;
        }
        if let Some(info) = parse_via_dev_tokens(tokens) {
            return Some(info);
        }
    }
    None
}

/// Parse Linux `ip route get <addr>` output.
///
/// Same `via`/`dev` token scan over the whole reply. Replies routed into a
/// tunnel without a gateway hop (e.g. `1.1.1.1 dev wgtun2 table 52 src …`,
/// no `via` token) yield `None` — the caller reports `n/a` rather than a
/// half-filled route.
pub fn parse_ip_route_get(out: &str) -> Option<RouteInfo> {
    parse_via_dev_tokens(out.split_whitespace())
}

/// Shared `via <gateway>` + `dev <iface>` token scan for `ip route` output.
/// Both tokens are required; the first occurrence of each wins.
fn parse_via_dev_tokens<'a>(tokens: impl Iterator<Item = &'a str>) -> Option<RouteInfo> {
    let mut gateway: Option<String> = None;
    let mut iface: Option<String> = None;
    let mut tokens = tokens.peekable();
    while let Some(token) = tokens.next() {
        if gateway.is_none() && token == "via" {
            if let Some(value) = tokens.next() {
                gateway = Some(value.to_string());
            }
        } else if iface.is_none() && token == "dev" {
            if let Some(value) = tokens.next() {
                iface = Some(value.to_string());
            }
        }
        if gateway.is_some() && iface.is_some() {
            break;
        }
    }
    Some(RouteInfo {
        gateway: gateway?,
        iface: iface?,
    })
}

/// Trim probe output down to a single IP literal.
///
/// Trims surrounding whitespace, then applies light validation: the result
/// must be one non-empty token (no whitespace inside, so multiline junk is
/// rejected) made of IP characters (`0-9a-fA-F.:`) containing at least one
/// digit and at least one `.`/`:` separator. Accepts IPv4 and IPv6;
/// anything else yields `None`.
pub fn trim_ip(out: &str) -> Option<String> {
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return None;
    }
    // One token only: any inner whitespace means multiline/wordy junk.
    if trimmed.split_whitespace().count() != 1 {
        return None;
    }
    // Light IP-literal shape check (IPv4 + IPv6, no DNS names, no sentences).
    let has_digit = trimmed.bytes().any(|b| b.is_ascii_digit());
    let has_separator = trimmed.bytes().any(|b| b == b'.' || b == b':');
    let all_ip_chars = trimmed
        .bytes()
        .all(|b| b.is_ascii_hexdigit() || b == b'.' || b == b':');
    if !(has_digit && has_separator && all_ip_chars) {
        return None;
    }
    Some(trimmed.to_string())
}

/// Default route via the platform probe chain (spec §6): the native pair
/// first — macOS `route -n get default` + `route -n get 1.1.1.1`, Linux
/// `ip route show default` + `ip route get 1.1.1.1` — then the other
/// platform's pair as a last resort (e.g. macOS with iproute2mac, or a box
/// missing the native tool). Every attempt runs unprivileged with a ~3 s
/// timeout; a missing binary fails fast to the next attempt. All failing
/// yields [`AppError::NetProbe`].
pub async fn get_default_route() -> Result<RouteInfo, AppError> {
    for (prog, args, parse) in route_probes() {
        if let Some(info) = run_route_probe(prog, args, parse).await {
            return Ok(info);
        }
    }
    Err(AppError::NetProbe(
        "no default route (native and fallback route probes all failed)".to_string(),
    ))
}

/// One entry of the [`route_probes`] chain: backend program, argv, and the
/// parser for its stdout.
type RouteProbe = (
    &'static str,
    &'static [&'static str],
    fn(&str) -> Option<RouteInfo>,
);

/// Ordered `(program, args, parser)` probe chain: native pair first, the
/// other platform's pair second. One shared runner keeps both parsers live
/// on every platform (no `cfg` split, no dead code).
#[cfg(target_os = "macos")]
fn route_probes() -> [RouteProbe; 4] {
    [
        ("route", &["-n", "get", "default"], parse_route_get_default),
        ("route", &["-n", "get", "1.1.1.1"], parse_route_get_default),
        (
            "ip",
            &["route", "show", "default"],
            parse_ip_route_show_default,
        ),
        ("ip", &["route", "get", "1.1.1.1"], parse_ip_route_get),
    ]
}

/// Ordered `(program, args, parser)` probe chain: native pair first, the
/// other platform's pair second. One shared runner keeps both parsers live
/// on every platform (no `cfg` split, no dead code).
#[cfg(not(target_os = "macos"))]
fn route_probes() -> [RouteProbe; 4] {
    [
        (
            "ip",
            &["route", "show", "default"],
            parse_ip_route_show_default,
        ),
        ("ip", &["route", "get", "1.1.1.1"], parse_ip_route_get),
        ("route", &["-n", "get", "default"], parse_route_get_default),
        ("route", &["-n", "get", "1.1.1.1"], parse_route_get_default),
    ]
}

/// Public/exit IP via `curl` (ipify) with a `dig` (OpenDNS) fallback.
/// Total budget ≤ 8 s; both failing yields [`AppError::NetProbe`].
pub async fn get_public_ip() -> Result<String, AppError> {
    // Outer budget caps curl + dig combined at ≤ 8 s (spec §6), so a slow
    // first probe cannot push the total past the UI's background budget.
    let probed = tokio::time::timeout(PUBLIC_IP_TOTAL_TIMEOUT, async {
        if let Some(ip) = run_curl_ipify().await {
            return Some(ip);
        }
        run_dig_opendns().await
    })
    .await;
    match probed {
        Ok(Some(ip)) => Ok(ip),
        _ => Err(AppError::NetProbe(
            "no exit IP (ipify and OpenDNS probes both failed)".to_string(),
        )),
    }
}

/// One route-probe attempt (`prog` + `args`, parsed by `parse`):
/// unprivileged, argv array (no shell), `ROUTE_TIMEOUT`-bounded. Any failure
/// (missing binary, timeout, non-zero exit, unparsable output) is `None` —
/// the caller moves to the next probe in the chain.
async fn run_route_probe(
    prog: &str,
    args: &[&str],
    parse: fn(&str) -> Option<RouteInfo>,
) -> Option<RouteInfo> {
    let output = tokio::time::timeout(
        ROUTE_TIMEOUT,
        tokio::process::Command::new(prog).args(args).output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}

/// `curl -s --max-time 5 https://api.ipify.org`, `CURL_TIMEOUT`-bounded.
/// Non-zero exit or unparseable body is `None` (→ dig fallback).
async fn run_curl_ipify() -> Option<String> {
    let output = tokio::time::timeout(
        CURL_TIMEOUT,
        tokio::process::Command::new("curl")
            .args(["-s", "--max-time", "5", "https://api.ipify.org"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    trim_ip(&String::from_utf8_lossy(&output.stdout))
}

/// `dig +short +time=3 +tries=1 myip.opendns.com @208.67.222.222`,
/// `DIG_TIMEOUT`-bounded. `dig` exits 0 even with empty answers, so the
/// `trim_ip` check is what rejects empty/garbage replies.
async fn run_dig_opendns() -> Option<String> {
    let output = tokio::time::timeout(
        DIG_TIMEOUT,
        tokio::process::Command::new("dig")
            .args([
                "+short",
                "+time=3",
                "+tries=1",
                "myip.opendns.com",
                "@208.67.222.222",
            ])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    trim_ip(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Realistic macOS `route -n get default` output shape.
    const ROUTE_FIXTURE: &str = "\
   route to: default
destination: default
       mask: default
    gateway: 192.168.0.1
  interface: en0
      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>
 recvpipe  sendpipe  ssthresh  rtt,msec    rttvar  hopcount      mtu     expire
       0         0         0         0         0         0      1500         0
";

    #[test]
    fn parses_route_fixture_gateway_and_iface() {
        assert_eq!(
            parse_route_get_default(ROUTE_FIXTURE),
            Some(RouteInfo {
                gateway: "192.168.0.1".to_string(),
                iface: "en0".to_string(),
            })
        );
    }

    /// Live `ip route show default` shape (Manjaro, 2026-10-29).
    const IP_ROUTE_SHOW_FIXTURE: &str = "\
default via 192.168.1.1 dev wlo1 proto dhcp src 192.168.1.12 metric 20600 
";

    #[test]
    fn parses_ip_route_show_default_gateway_and_iface() {
        assert_eq!(
            parse_ip_route_show_default(IP_ROUTE_SHOW_FIXTURE),
            Some(RouteInfo {
                gateway: "192.168.1.1".to_string(),
                iface: "wlo1".to_string(),
            })
        );
    }

    #[test]
    fn ip_route_show_uses_first_default_line() {
        let out = "default via 10.0.0.1 dev eth0 proto static metric 100\n\
                   default via 192.168.1.1 dev wlo1 proto dhcp metric 600\n";
        assert_eq!(
            parse_ip_route_show_default(out),
            Some(RouteInfo {
                gateway: "10.0.0.1".to_string(),
                iface: "eth0".to_string(),
            })
        );
    }

    #[test]
    fn ip_route_show_ignores_non_default_lines() {
        let out = "192.168.1.0/24 dev wlo1 proto kernel scope link src 192.168.1.12\n\
                   default via 192.168.1.1 dev wlo1 proto dhcp metric 20600\n";
        assert_eq!(
            parse_ip_route_show_default(out),
            Some(RouteInfo {
                gateway: "192.168.1.1".to_string(),
                iface: "wlo1".to_string(),
            })
        );
    }

    #[test]
    fn ip_route_show_requires_via_and_dev() {
        assert_eq!(parse_ip_route_show_default(""), None);
        assert_eq!(parse_ip_route_show_default("default dev wlo1\n"), None);
        assert_eq!(
            parse_ip_route_show_default("default via 192.168.1.1\n"),
            None
        );
        assert_eq!(
            parse_ip_route_show_default("some junk\nwithout the keys\n"),
            None
        );
    }

    #[test]
    fn parses_ip_route_get_with_gateway() {
        let out = "1.1.1.1 via 192.168.1.1 dev wlo1 src 192.168.1.12 uid 1000\n    cache \n";
        assert_eq!(
            parse_ip_route_get(out),
            Some(RouteInfo {
                gateway: "192.168.1.1".to_string(),
                iface: "wlo1".to_string(),
            })
        );
    }

    #[test]
    fn ip_route_get_without_via_is_none() {
        // Live shape when the reply is routed into a tunnel with no gateway
        // hop (Manjaro + active wgtun2, 2026-10-29): no half-filled route —
        // the caller shows `n/a` instead.
        let out = "1.1.1.1 dev wgtun2 table 52 src 10.75.171.185 uid 1000\n    cache \n";
        assert_eq!(parse_ip_route_get(out), None);
        assert_eq!(parse_ip_route_get(""), None);
    }

    #[test]
    fn route_keys_match_case_insensitively_with_leading_whitespace() {
        let out = "   Gateway: 10.0.0.1\n\tINTERFACE: en1\n";
        assert_eq!(
            parse_route_get_default(out),
            Some(RouteInfo {
                gateway: "10.0.0.1".to_string(),
                iface: "en1".to_string(),
            })
        );
    }

    #[test]
    fn route_parser_requires_both_fields() {
        assert_eq!(parse_route_get_default(""), None);
        assert_eq!(parse_route_get_default("gateway: 192.168.0.1\n"), None);
        assert_eq!(parse_route_get_default("interface: en0\n"), None);
        assert_eq!(parse_route_get_default("gateway: \ninterface: \n"), None);
        assert_eq!(
            parse_route_get_default("some junk\nwithout the keys\n"),
            None
        );
    }

    #[test]
    fn trims_ip_output() {
        assert_eq!(trim_ip("  1.2.3.4\n"), Some("1.2.3.4".to_string()));
        assert_eq!(trim_ip("2001:db8::1\n"), Some("2001:db8::1".to_string()));
    }

    #[test]
    fn rejects_garbage_ip() {
        assert_eq!(trim_ip(""), None);
        assert_eq!(trim_ip("   \n  \t\n"), None);
        assert_eq!(trim_ip("1.2.3.4\n5.6.7.8\n"), None);
        assert_eq!(trim_ip("not-an-ip\n"), None);
        assert_eq!(trim_ip(" Dien Bien Phu \n"), None);
    }
}
