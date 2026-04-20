//! Hook security — SSRF guard, URL validator, and env interpolator.
//!
//! These utilities are used by HTTP hook implementations to prevent
//! common security issues:
//! - **SSRF**: Blocking requests to private/internal networks
//! - **URL validation**: Ensuring only safe schemes and hosts are used
//! - **Env interpolation**: Resolving `${VAR}` patterns with safe defaults

use std::net::IpAddr;
use thiserror::Error;

/// Errors from URL validation.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum UrlValidationError {
    /// Failed to parse the URL.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    /// Scheme is not allowed.
    #[error("scheme '{0}' is not allowed")]
    DisallowedScheme(String),
    /// Host is missing or empty.
    #[error("missing or empty host")]
    MissingHost,
    /// Host is in the blocklist.
    #[error("host '{0}' is blocked")]
    BlockedHost(String),
    /// Host is not in the allowlist.
    #[error("host '{0}' is not in allowlist")]
    NotInAllowlist(String),
    /// Port is not allowed.
    #[error("port {0} is not allowed")]
    DisallowedPort(u16),
}

/// Validates URLs for safe HTTP hook usage.
#[derive(Debug, Clone)]
pub struct UrlValidator {
    allowed_schemes: Vec<String>,
    blocked_hosts: Vec<String>,
    allowed_hosts: Option<Vec<String>>,
    allowed_ports: Option<Vec<u16>>,
}

impl Default for UrlValidator {
    fn default() -> Self {
        Self {
            allowed_schemes: vec!["http".into(), "https".into()],
            blocked_hosts: Vec::new(),
            allowed_hosts: None,
            allowed_ports: None,
        }
    }
}

impl UrlValidator {
    /// Create a default validator (allows http/https).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Only allow the given URL schemes.
    #[must_use]
    pub fn with_schemes(mut self, schemes: Vec<String>) -> Self {
        self.allowed_schemes = schemes;
        self
    }

    /// Block specific hosts (exact match or suffix match).
    #[must_use]
    pub fn with_blocked_hosts(mut self, hosts: Vec<String>) -> Self {
        self.blocked_hosts = hosts;
        self
    }

    /// Restrict to an allowlist of hosts.
    #[must_use]
    pub fn with_allowed_hosts(mut self, hosts: Vec<String>) -> Self {
        self.allowed_hosts = Some(hosts);
        self
    }

    /// Restrict to specific ports.
    #[must_use]
    pub fn with_allowed_ports(mut self, ports: Vec<u16>) -> Self {
        self.allowed_ports = Some(ports);
        self
    }

    /// Validate a URL.
    ///
    /// Checks scheme, host, and port against configured rules.
    pub fn validate(&self, url: &str) -> Result<(), UrlValidationError> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|e| UrlValidationError::InvalidUrl(e.to_string()))?;

        // Scheme check
        let scheme = parsed.scheme();
        if !self.allowed_schemes.iter().any(|s| s == scheme) {
            return Err(UrlValidationError::DisallowedScheme(scheme.into()));
        }

        // Host check
        let host = parsed
            .host_str()
            .ok_or(UrlValidationError::MissingHost)?
            .to_lowercase();
        if host.is_empty() {
            return Err(UrlValidationError::MissingHost);
        }

        // Blocked hosts
        for blocked in &self.blocked_hosts {
            let blocked = blocked.to_lowercase();
            if host == blocked || host.ends_with(&format!(".{blocked}")) {
                return Err(UrlValidationError::BlockedHost(host));
            }
        }

        // Allowed hosts
        if let Some(ref allowed) = self.allowed_hosts {
            if !allowed.iter().any(|a| {
                let a = a.to_lowercase();
                host == a || host.ends_with(&format!(".{a}"))
            }) {
                return Err(UrlValidationError::NotInAllowlist(host));
            }
        }

        // Port check
        if let Some(port) = parsed.port() {
            if let Some(ref allowed) = self.allowed_ports {
                if !allowed.contains(&port) {
                    return Err(UrlValidationError::DisallowedPort(port));
                }
            }
        }

        Ok(())
    }
}

/// Errors from SSRF guard.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SsrfError {
    /// Failed to parse the URL.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    /// Host resolved to a private IP.
    #[error("host '{host}' resolves to private IP {ip}")]
    PrivateIp {
        /// The host that was blocked.
        host: String,
        /// The private IP it resolved to.
        ip: String,
    },
    /// Host resolved to localhost.
    #[error("host '{host}' resolves to localhost")]
    Localhost {
        /// The localhost host that was blocked.
        host: String,
    },
    /// Host resolution failed.
    #[error("failed to resolve host '{0}'")]
    ResolutionFailed(String),
}

/// Guards against Server-Side Request Forgery (SSRF) by blocking
/// requests to private/internal networks and localhost.
#[derive(Debug, Clone, Default)]
pub struct SsrfGuard;

impl SsrfGuard {
    /// Check if a URL is safe from SSRF (does not point to private/internal).
    ///
    /// This resolves the host and checks the resulting IP address.
    /// For best security, run this *before* making the HTTP request.
    pub fn guard(url: &str) -> Result<(), SsrfError> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|e| SsrfError::InvalidUrl(e.to_string()))?;

        let host = parsed
            .host_str()
            .ok_or_else(|| SsrfError::InvalidUrl("missing host".into()))?;

        // Check if host is already an IP address
        if let Ok(ip) = host.parse::<IpAddr>() {
            Self::check_ip(ip, host)?;
            return Ok(());
        }

        // Block common localhost names without doing DNS lookup
        let lower = host.to_lowercase();
        if lower == "localhost"
            || lower == "127.0.0.1"
            || lower == "::1"
            || lower.ends_with(".localhost")
        {
            return Err(SsrfError::Localhost {
                host: host.into(),
            });
        }

        // Resolve host to IP addresses
        let addrs: Vec<_> = match std::net::ToSocketAddrs::to_socket_addrs(&format!("{host}:80"))
        {
            Ok(iter) => iter.map(|a| a.ip()).collect(),
            Err(_) => {
                return Err(SsrfError::ResolutionFailed(host.into()));
            }
        };

        if addrs.is_empty() {
            return Err(SsrfError::ResolutionFailed(host.into()));
        }

        for ip in addrs {
            Self::check_ip(ip, host)?;
        }

        Ok(())
    }

    fn check_ip(ip: IpAddr, host: &str) -> Result<(), SsrfError> {
        match ip {
            IpAddr::V4(v4) => {
                let octets = v4.octets();
                // 127.0.0.0/8
                if octets[0] == 127 {
                    return Err(SsrfError::Localhost { host: host.into() });
                }
                // 10.0.0.0/8
                if octets[0] == 10 {
                    return Err(SsrfError::PrivateIp {
                        host: host.into(),
                        ip: ip.to_string(),
                    });
                }
                // 172.16.0.0/12
                if octets[0] == 172 && (16..=31).contains(&octets[1]) {
                    return Err(SsrfError::PrivateIp {
                        host: host.into(),
                        ip: ip.to_string(),
                    });
                }
                // 192.168.0.0/16
                if octets[0] == 192 && octets[1] == 168 {
                    return Err(SsrfError::PrivateIp {
                        host: host.into(),
                        ip: ip.to_string(),
                    });
                }
                // 169.254.0.0/16 (link-local)
                if octets[0] == 169 && octets[1] == 254 {
                    return Err(SsrfError::PrivateIp {
                        host: host.into(),
                        ip: ip.to_string(),
                    });
                }
                // 0.0.0.0
                if octets == [0, 0, 0, 0] {
                    return Err(SsrfError::PrivateIp {
                        host: host.into(),
                        ip: ip.to_string(),
                    });
                }
            }
            IpAddr::V6(v6) => {
                let segments = v6.segments();
                // ::1
                if segments == [0, 0, 0, 0, 0, 0, 0, 1] {
                    return Err(SsrfError::Localhost { host: host.into() });
                }
                // fe80::/10 (link-local)
                if segments[0] & 0xffc0 == 0xfe80 {
                    return Err(SsrfError::PrivateIp {
                        host: host.into(),
                        ip: ip.to_string(),
                    });
                }
                // fc00::/7 (unique local)
                if segments[0] & 0xfe00 == 0xfc00 {
                    return Err(SsrfError::PrivateIp {
                        host: host.into(),
                        ip: ip.to_string(),
                    });
                }
                // ::ffff:0:0/96 (IPv4-mapped) — handled by the resolver returning V4
            }
        }
        Ok(())
    }
}

/// Errors from environment variable interpolation.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum InterpolationError {
    /// Variable is not set and has no default.
    #[error("environment variable '{0}' is not set")]
    VarNotSet(String),
    /// Invalid syntax in the interpolation pattern.
    #[error("invalid interpolation syntax")]
    InvalidSyntax,
}

/// Interpolates `${VAR}` and `${VAR:-default}` patterns in strings.
///
/// Useful for HTTP hook configurations that need to reference
/// environment variables (e.g. API keys, endpoint URLs).
#[derive(Debug, Clone, Default)]
pub struct EnvInterpolator;

impl EnvInterpolator {
    /// Interpolate environment variables in the input string.
    ///
    /// Supports:
    /// - `${VAR}` — replaced with the value of `VAR`
    /// - `${VAR:-default}` — replaced with `VAR` or `default` if unset
    /// - `$VAR` — replaced with the value of `VAR` (simpler form)
    ///
    /// Unknown variables without a default return an error.
    pub fn interpolate(input: &str) -> Result<String, InterpolationError> {
        let mut result = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '$' {
                if chars.peek() == Some(&'{') {
                    chars.next(); // consume '{'
                    let mut var_name = String::new();
                    let mut default_value = None;

                    while let Some(&c) = chars.peek() {
                        if c == '}' {
                            chars.next();
                            break;
                        }
                        if c == ':' && chars.clone().nth(1) == Some('-') {
                            chars.next(); // consume ':'
                            chars.next(); // consume '-'
                            let mut def = String::new();
                            while let Some(&d) = chars.peek() {
                                if d == '}' {
                                    break;
                                }
                                def.push(d);
                                chars.next();
                            }
                            default_value = Some(def);
                        } else {
                            var_name.push(c);
                        }
                        chars.next();
                    }

                    // Skip to closing brace if we were reading default
                    if default_value.is_some() {
                        if chars.peek() == Some(&'}') {
                            chars.next();
                        }
                    }

                    let value = std::env::var(&var_name)
                        .ok()
                        .or(default_value)
                        .ok_or_else(|| InterpolationError::VarNotSet(var_name.clone()))?;
                    result.push_str(&value);
                } else {
                    // $VAR form (no braces)
                    let mut var_name = String::new();
                    while let Some(&c) = chars.peek() {
                        if c.is_alphanumeric() || c == '_' {
                            var_name.push(c);
                            chars.next();
                        } else {
                            break;
                        }
                    }

                    if var_name.is_empty() {
                        result.push('$');
                    } else {
                        let value = std::env::var(&var_name)
                            .map_err(|_| InterpolationError::VarNotSet(var_name.clone()))?;
                        result.push_str(&value);
                    }
                }
            } else {
                result.push(ch);
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── UrlValidator tests ────────────────────────────────────────────────

    #[test]
    fn validator_allows_http_https() {
        let v = UrlValidator::new();
        assert!(v.validate("https://example.com/api").is_ok());
        assert!(v.validate("http://example.com/api").is_ok());
    }

    #[test]
    fn validator_blocks_file_scheme() {
        let v = UrlValidator::new();
        assert_eq!(
            v.validate("file:///etc/passwd"),
            Err(UrlValidationError::DisallowedScheme("file".into()))
        );
    }

    #[test]
    fn validator_blocks_missing_host() {
        let v = UrlValidator::new();
        // Malformed URL without a host
        assert!(matches!(
            v.validate("not-a-url"),
            Err(UrlValidationError::InvalidUrl(_))
        ));
    }

    #[test]
    fn validator_blocked_hosts() {
        let v = UrlValidator::new().with_blocked_hosts(vec!["evil.com".into()]);
        assert!(v.validate("https://evil.com/hook").is_err());
        assert!(v.validate("https://sub.evil.com/hook").is_err());
        assert!(v.validate("https://good.com/hook").is_ok());
    }

    #[test]
    fn validator_allowed_hosts() {
        let v = UrlValidator::new().with_allowed_hosts(vec!["trusted.com".into()]);
        assert!(v.validate("https://trusted.com/hook").is_ok());
        assert!(v.validate("https://other.com/hook").is_err());
    }

    #[test]
    fn validator_allowed_ports() {
        let v = UrlValidator::new().with_allowed_ports(vec![80, 443]);
        assert!(v.validate("https://example.com:443/hook").is_ok());
        assert!(v.validate("https://example.com:8080/hook").is_err());
    }

    // ─── SsrfGuard tests ───────────────────────────────────────────────────

    #[test]
    fn ssrf_blocks_localhost() {
        assert!(matches!(
            SsrfGuard::guard("http://localhost:8080/hook"),
            Err(SsrfError::Localhost { .. })
        ));
    }

    #[test]
    fn ssrf_blocks_127_0_0_1() {
        assert!(matches!(
            SsrfGuard::guard("http://127.0.0.1/hook"),
            Err(SsrfError::Localhost { .. })
        ));
    }

    #[test]
    fn ssrf_blocks_private_ipv4() {
        assert!(matches!(
            SsrfGuard::guard("http://10.0.0.1/hook"),
            Err(SsrfError::PrivateIp { .. })
        ));
        assert!(matches!(
            SsrfGuard::guard("http://192.168.1.1/hook"),
            Err(SsrfError::PrivateIp { .. })
        ));
        assert!(matches!(
            SsrfGuard::guard("http://172.16.0.1/hook"),
            Err(SsrfError::PrivateIp { .. })
        ));
    }

    #[test]
    fn ssrf_allows_public_ip() {
        // 1.1.1.1 is Cloudflare's public DNS
        let result = SsrfGuard::guard("http://1.1.1.1/hook");
        assert!(result.is_ok(), "expected ok, got {result:?}");
    }

    #[test]
    fn ssrf_allows_public_domain() {
        let result = SsrfGuard::guard("https://api.github.com/hook");
        assert!(result.is_ok(), "expected ok, got {result:?}");
    }

    // ─── EnvInterpolator tests ─────────────────────────────────────────────

    #[test]
    fn interpolate_simple_var() {
        // PATH exists on all platforms
        let path_val = std::env::var("PATH").expect("PATH should be set");
        assert_eq!(
            EnvInterpolator::interpolate("${PATH}").unwrap(),
            path_val
        );
    }

    #[test]
    fn interpolate_with_default() {
        assert_eq!(
            EnvInterpolator::interpolate("${THIS_VAR_DOES_NOT_EXIST_12345:-fallback}").unwrap(),
            "fallback"
        );
    }

    #[test]
    fn interpolate_dollar_var_form() {
        let path_val = std::env::var("PATH").expect("PATH should be set");
        assert_eq!(
            EnvInterpolator::interpolate("$PATH").unwrap(),
            path_val
        );
    }

    #[test]
    fn interpolate_mixed_text() {
        let home_val = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .expect("HOME or USERPROFILE should be set");
        let result = EnvInterpolator::interpolate("prefix-${HOME}-suffix").unwrap();
        assert_eq!(result, format!("prefix-{home_val}-suffix"));
    }

    #[test]
    fn interpolate_missing_var_errors() {
        assert!(matches!(
            EnvInterpolator::interpolate("${THIS_VAR_DOES_NOT_EXIST_12345}"),
            Err(InterpolationError::VarNotSet(_))
        ));
    }

    #[test]
    fn interpolate_no_op_for_plain_text() {
        assert_eq!(
            EnvInterpolator::interpolate("no variables here").unwrap(),
            "no variables here"
        );
    }
}
