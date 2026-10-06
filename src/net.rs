//! Default route + public/exit-IP probing (spec §6).
//!
//! - Default route: `route -n get default` → parse `gateway:` +
//!   `interface:`. Fallback: `route -n get 1.1.1.1`. Failure → [`AppError::NetProbe`]
//!   (the caller in Task 7 shows `n/a`); never blocks beyond timeouts.
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

/// Default route via `route -n get default`, falling back to
/// `route -n get 1.1.1.1`. Both run unprivileged with a ~3 s timeout each;
/// both failing yields [`AppError::NetProbe`].
pub async fn get_default_route() -> Result<RouteInfo, AppError> {
    // Primary target first, then the literal-IP fallback (spec §6).
    if let Some(info) = run_route(&["-n", "get", "default"]).await {
        return Ok(info);
    }
    if let Some(info) = run_route(&["-n", "get", "1.1.1.1"]).await {
        return Ok(info);
    }
    Err(AppError::NetProbe(
        "no default route (`route -n get default` and `route -n get 1.1.1.1` both failed)"
            .to_string(),
    ))
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

/// One `route -n get <target>` attempt: unprivileged, argv array (no shell),
/// `ROUTE_TIMEOUT`-bounded. Any failure (missing binary, timeout, non-zero
/// exit, unparsable output) is `None` — the caller decides fallback vs error.
async fn run_route(args: &[&str]) -> Option<RouteInfo> {
    let output = tokio::time::timeout(
        ROUTE_TIMEOUT,
        tokio::process::Command::new("route").args(args).output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_route_get_default(&String::from_utf8_lossy(&output.stdout))
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
