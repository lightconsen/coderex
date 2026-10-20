//! Additional telemetry export channels: OTLP/HTTP JSON logs and Datadog Logs.
//!
//! Both channels consume the same scrubbed event payload the first-party
//! uploader uses. They are deliberately dependency-light: OTLP is plain
//! OTLP/HTTP JSON (no OpenTelemetry SDK) and Datadog is its public Logs intake
//! API, so each is a single HTTP POST.
//!
//! Channels are configured from the environment by [`ChannelConfig::from_env`]
//! (or programmatically with [`ChannelConfig`]) and are only constructed when
//! the owning [`crate::TelemetryConfig`] is enabled. An uploader whose
//! telemetry opt-in is off never calls them, and [`TelemetryChannels::send_events`]
//! itself refuses to send when `enabled` is false.
//!
//! | Channel | Environment variables |
//! |---|---|
//! | OTLP/HTTP logs | `CODEREX_OTLP_ENDPOINT`, `CODEREX_OTLP_HEADERS` |
//! | Datadog Logs | `CODEREX_DATADOG_API_KEY` (or `DD_API_KEY`), `CODEREX_DATADOG_SITE`, `CODEREX_DATADOG_ENDPOINT` |

use crate::{PendingEvent, Scrubber, TelemetryError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Environment variable holding the OTLP/HTTP endpoint.
pub const OTLP_ENDPOINT_ENV: &str = "CODEREX_OTLP_ENDPOINT";
/// Optional OTLP request headers, as `key=value,key2=value2`.
pub const OTLP_HEADERS_ENV: &str = "CODEREX_OTLP_HEADERS";
/// Datadog API key (preferred over the generic `DD_API_KEY`).
pub const DATADOG_API_KEY_ENV: &str = "CODEREX_DATADOG_API_KEY";
/// Conventional Datadog API key variable, used as a fallback.
pub const DD_API_KEY_ENV: &str = "DD_API_KEY";
/// Datadog site (`datadoghq.com`, `datadoghq.eu`, `us3.datadoghq.com`, …).
pub const DATADOG_SITE_ENV: &str = "CODEREX_DATADOG_SITE";
/// Full Datadog Logs intake URL, overriding the site-derived endpoint.
pub const DATADOG_ENDPOINT_ENV: &str = "CODEREX_DATADOG_ENDPOINT";

/// Default Datadog site when none is configured.
pub const DEFAULT_DATADOG_SITE: &str = "datadoghq.com";

const SERVICE_NAME: &str = "coderex";
const OTLP_LOGS_PATH: &str = "/v1/logs";
const DATADOG_LOGS_PATH: &str = "/api/v2/logs";

/// Configuration for the optional export channels.
///
/// `Default` is enabled with no destinations; a channel is only active when
/// its endpoint (and, for Datadog, an API key) is present.
#[derive(Clone, PartialEq, Eq)]
pub struct ChannelConfig {
    /// Master switch. When false, nothing is sent even if endpoints are set.
    pub enabled: bool,
    /// OTLP/HTTP base endpoint (the `/v1/logs` path is appended) or a full
    /// `.../v1/logs` URL.
    pub otlp_endpoint: Option<String>,
    /// Additional OTLP request headers.
    pub otlp_headers: Vec<(String, String)>,
    /// Datadog API key.
    pub datadog_api_key: Option<String>,
    /// Datadog site used to derive the intake host.
    pub datadog_site: String,
    /// Full Datadog intake URL; overrides `datadog_site` when set.
    pub datadog_endpoint: Option<String>,
}

impl std::fmt::Debug for ChannelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the API key in a log line or a panic message.
        let header_names: Vec<&str> = self
            .otlp_headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        f.debug_struct("ChannelConfig")
            .field("enabled", &self.enabled)
            .field("otlp_endpoint", &self.otlp_endpoint)
            .field("otlp_headers", &header_names)
            .field(
                "datadog_api_key",
                &self.datadog_api_key.as_ref().map(|_| "<redacted>"),
            )
            .field("datadog_site", &self.datadog_site)
            .field("datadog_endpoint", &self.datadog_endpoint)
            .finish()
    }
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            otlp_endpoint: None,
            otlp_headers: Vec::new(),
            datadog_api_key: None,
            datadog_site: DEFAULT_DATADOG_SITE.to_string(),
            datadog_endpoint: None,
        }
    }
}

impl ChannelConfig {
    /// Read channel configuration from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// Pure helper behind [`Self::from_env`], testable without touching the
    /// process environment.
    fn from_vars(get: impl Fn(&str) -> Option<String>) -> Self {
        let get_non_empty = |name: &str| {
            get(name)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };

        let datadog_api_key =
            get_non_empty(DATADOG_API_KEY_ENV).or_else(|| get_non_empty(DD_API_KEY_ENV));

        Self {
            enabled: true,
            otlp_endpoint: get_non_empty(OTLP_ENDPOINT_ENV),
            otlp_headers: parse_headers(get(OTLP_HEADERS_ENV).as_deref()),
            datadog_api_key,
            datadog_site: get_non_empty(DATADOG_SITE_ENV)
                .unwrap_or_else(|| DEFAULT_DATADOG_SITE.to_string()),
            datadog_endpoint: get_non_empty(DATADOG_ENDPOINT_ENV),
        }
    }

    /// Whether at least one channel has a usable destination.
    #[must_use]
    pub fn has_destination(&self) -> bool {
        self.otlp_logs_endpoint().is_some() || self.datadog_logs_endpoint().is_some()
    }

    /// The OTLP logs URL, appending `/v1/logs` to a base endpoint.
    #[must_use]
    pub fn otlp_logs_endpoint(&self) -> Option<String> {
        let endpoint = self.otlp_endpoint.as_deref()?.trim();
        if endpoint.is_empty() {
            return None;
        }
        let endpoint = endpoint.trim_end_matches('/');
        if endpoint.ends_with(OTLP_LOGS_PATH) {
            Some(endpoint.to_string())
        } else {
            Some(format!("{endpoint}{OTLP_LOGS_PATH}"))
        }
    }

    /// The Datadog Logs intake URL, or `None` without an API key.
    #[must_use]
    pub fn datadog_logs_endpoint(&self) -> Option<String> {
        let api_key = self.datadog_api_key.as_deref()?.trim();
        if api_key.is_empty() {
            return None;
        }
        if let Some(endpoint) = self.datadog_endpoint.as_deref() {
            let endpoint = endpoint.trim();
            if !endpoint.is_empty() {
                return Some(endpoint.to_string());
            }
        }

        let site = self
            .datadog_site
            .trim()
            .trim_end_matches('/')
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        if site.is_empty() {
            return None;
        }
        let host = if site.starts_with("http-intake.logs.") {
            site.to_string()
        } else {
            format!("http-intake.logs.{site}")
        };
        Some(format!("https://{host}{DATADOG_LOGS_PATH}"))
    }
}

/// Parse `key=value,key2=value2` OTLP headers, skipping malformed entries.
///
/// This is the same syntax accepted by `CODEREX_OTLP_HEADERS`, exposed so a
/// host that carries the value in its own settings file can build a
/// [`ChannelConfig`] without duplicating the parser.
#[must_use]
pub fn parse_otlp_headers(raw: Option<&str>) -> Vec<(String, String)> {
    parse_headers(raw)
}

/// Parse `key=value,key2=value2` headers, skipping malformed entries.
fn parse_headers(raw: Option<&str>) -> Vec<(String, String)> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    raw.split(',')
        .filter_map(|entry| {
            let (name, value) = entry.split_once('=')?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some((name.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Client for the optional export channels.
#[derive(Debug, Clone)]
pub struct TelemetryChannels {
    config: ChannelConfig,
    client: reqwest::Client,
    scrubber: Scrubber,
}

impl TelemetryChannels {
    /// Build a channel set, or `None` when disabled or no destination exists.
    #[must_use]
    pub fn new(config: ChannelConfig) -> Option<Self> {
        if !config.enabled || !config.has_destination() {
            return None;
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Some(Self {
            config,
            client,
            scrubber: Scrubber::new(),
        })
    }

    /// Whether this set has any active channel.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.config.enabled || !self.config.has_destination()
    }

    /// Send events to every configured channel.
    ///
    /// A failure in one channel does not prevent the other from being tried;
    /// the failures are reported together. Sending is a no-op when the channel
    /// set or the events are empty.
    pub async fn send_events(&self, events: &[PendingEvent]) -> Result<(), TelemetryError> {
        if !self.config.enabled || events.is_empty() {
            return Ok(());
        }

        let mut errors = Vec::new();

        if let Some(endpoint) = self.config.otlp_logs_endpoint() {
            if let Err(error) = self.send_otlp(&endpoint, events).await {
                errors.push(format!("OTLP: {error}"));
            }
        }

        if let Some(endpoint) = self.config.datadog_logs_endpoint() {
            if let Err(error) = self.send_datadog(&endpoint, events).await {
                errors.push(format!("Datadog: {error}"));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(TelemetryError::Upload(errors.join("; ")))
        }
    }

    async fn send_otlp(
        &self,
        endpoint: &str,
        events: &[PendingEvent],
    ) -> Result<(), TelemetryError> {
        let payload = build_otlp_payload(&self.scrubber, events);
        let mut request = self.client.post(endpoint);
        for (name, value) in &self.config.otlp_headers {
            request = request.header(name.as_str(), value.as_str());
        }

        let response = request
            .json(&payload)
            .send()
            .await
            .map_err(|e| TelemetryError::Upload(format!("request failed: {e}")))?;
        ensure_success(&response, "OTLP")
    }

    async fn send_datadog(
        &self,
        endpoint: &str,
        events: &[PendingEvent],
    ) -> Result<(), TelemetryError> {
        let api_key = self.config.datadog_api_key.as_deref().unwrap_or_default();
        let payload = build_datadog_logs(&self.scrubber, events);

        let response = self
            .client
            .post(endpoint)
            .header("DD-API-KEY", api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| TelemetryError::Upload(format!("request failed: {e}")))?;
        ensure_success(&response, "Datadog")
    }
}

fn ensure_success(response: &reqwest::Response, channel: &str) -> Result<(), TelemetryError> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status == reqwest::StatusCode::FORBIDDEN
    {
        return Err(TelemetryError::Rejected {
            status: status.as_u16(),
        });
    }
    Err(TelemetryError::Upload(format!("{channel} HTTP {status}")))
}

/// Build an OTLP/HTTP JSON `ExportLogsServiceRequest`.
fn build_otlp_payload(scrubber: &Scrubber, events: &[PendingEvent]) -> serde_json::Value {
    let log_records: Vec<serde_json::Value> = events
        .iter()
        .map(|event| {
            let mut attributes = vec![otlp_string_attribute("event.type", &event.event_type)];

            if let Ok(mut payload) = serde_json::from_str::<serde_json::Value>(&event.payload) {
                scrubber.scrub_json(&mut payload);
                if let Some(object) = payload.as_object() {
                    for (key, value) in object {
                        // `type` duplicates the `event.type` attribute.
                        if key == "type" {
                            continue;
                        }
                        if let Some(attribute) = json_to_otlp_attribute(key, value) {
                            attributes.push(attribute);
                        }
                    }
                }
            }

            let severity = severity_for(&event.event_type);
            serde_json::json!({
                "timeUnixNano": event_time_nanos(event).to_string(),
                "observedTimeUnixNano": event_time_nanos(event).to_string(),
                "severityNumber": severity.0,
                "severityText": severity.1,
                "body": { "stringValue": event.event_type },
                "attributes": attributes,
            })
        })
        .collect();

    serde_json::json!({
        "resourceLogs": [{
            "resource": {
                "attributes": [
                    otlp_string_attribute("service.name", SERVICE_NAME),
                    otlp_string_attribute("service.version", env!("CARGO_PKG_VERSION")),
                    otlp_string_attribute("telemetry.sdk.name", "coderex-telemetry"),
                    otlp_string_attribute("telemetry.sdk.language", "rust"),
                ],
            },
            "scopeLogs": [{
                "scope": {
                    "name": "coderex.telemetry",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "logRecords": log_records,
            }],
        }],
    })
}

/// Map one JSON payload field to an OTLP `KeyValue` attribute.
fn json_to_otlp_attribute(key: &str, value: &serde_json::Value) -> Option<serde_json::Value> {
    use serde_json::Value;

    let any_value = match value {
        Value::Null => return None,
        Value::String(text) => serde_json::json!({ "stringValue": text }),
        Value::Bool(flag) => serde_json::json!({ "boolValue": flag }),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                // OTLP JSON encodes 64-bit integers as strings.
                serde_json::json!({ "intValue": integer.to_string() })
            } else if let Some(integer) = number.as_u64() {
                serde_json::json!({ "intValue": integer.to_string() })
            } else {
                let float = number.as_f64()?;
                serde_json::json!({ "doubleValue": float })
            }
        }
        // Composite values have no OTLP scalar AnyValue; keep them inspectable
        // as JSON text instead of dropping them.
        other => serde_json::json!({ "stringValue": other.to_string() }),
    };
    Some(otlp_attribute(key, &any_value))
}

fn otlp_attribute(key: &str, value: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "key": key, "value": value })
}

/// Build a string-valued OTLP `KeyValue` attribute.
fn otlp_string_attribute(key: &str, value: &str) -> serde_json::Value {
    otlp_attribute(key, &serde_json::json!({ "stringValue": value }))
}

/// OTLP severity number/text for an event type.
fn severity_for(event_type: &str) -> (u8, &'static str) {
    if event_type == "error" {
        // OTLP SeverityNumber.ERROR
        (17, "ERROR")
    } else {
        // OTLP SeverityNumber.INFO
        (9, "INFO")
    }
}

/// Build the Datadog Logs intake array.
fn build_datadog_logs(scrubber: &Scrubber, events: &[PendingEvent]) -> Vec<serde_json::Value> {
    events
        .iter()
        .map(|event| {
            let mut payload = serde_json::from_str::<serde_json::Value>(&event.payload)
                .unwrap_or(serde_json::Value::Null);
            scrubber.scrub_json(&mut payload);
            // `message` is the scrubbed JSON document; Datadog indexes either
            // a string or an object, and a string preserves the exact bytes.
            let message = payload.to_string();
            let status = if event.event_type == "error" {
                "error"
            } else {
                "info"
            };
            serde_json::json!({
                "ddsource": SERVICE_NAME,
                "service": SERVICE_NAME,
                "ddtags": format!(
                    "event_type:{},version:{}",
                    event.event_type,
                    env!("CARGO_PKG_VERSION"),
                ),
                "message": message,
                "status": status,
                "timestamp": event_time_millis(event),
            })
        })
        .collect()
}

/// Event time in nanoseconds since the Unix epoch.
fn event_time_nanos(event: &PendingEvent) -> u128 {
    chrono::DateTime::parse_from_rfc3339(&event.created_at).map_or_else(
        |_| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        },
        |timestamp| {
            u128::try_from(timestamp.timestamp_nanos_opt().unwrap_or_default()).unwrap_or_default()
        },
    )
}

/// Event time in milliseconds since the Unix epoch (Datadog's unit).
fn event_time_millis(event: &PendingEvent) -> u128 {
    event_time_nanos(event) / 1_000_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn pending_event(event_type: &str, payload: &serde_json::Value) -> PendingEvent {
        PendingEvent {
            id: 1,
            event_type: event_type.to_string(),
            payload: payload.to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn sample_events() -> Vec<PendingEvent> {
        vec![
            pending_event(
                "tool_used",
                &json!({
                    "type": "tool_used",
                    "tool_name": "Bash",
                    "success": true,
                    "duration_ms": 12,
                    "timestamp": 1_767_225_600,
                }),
            ),
            pending_event(
                "error",
                &json!({
                    "type": "error",
                    "error_type": "io",
                    "message": "failed for bob@example.com",
                    "timestamp": 1_767_225_601,
                }),
            ),
        ]
    }

    // ── Configuration / endpoint derivation ─────────────────────────────

    #[test]
    fn otlp_endpoint_appends_the_logs_path() {
        let config = ChannelConfig {
            otlp_endpoint: Some("http://localhost:4318".into()),
            ..Default::default()
        };
        assert_eq!(
            config.otlp_logs_endpoint().as_deref(),
            Some("http://localhost:4318/v1/logs")
        );
    }

    #[test]
    fn otlp_full_endpoint_is_preserved() {
        let config = ChannelConfig {
            otlp_endpoint: Some("http://localhost:4318/v1/logs/".into()),
            ..Default::default()
        };
        assert_eq!(
            config.otlp_logs_endpoint().as_deref(),
            Some("http://localhost:4318/v1/logs")
        );
    }

    #[test]
    fn datadog_endpoint_uses_the_configured_site() {
        let config = ChannelConfig {
            datadog_api_key: Some("key".into()),
            datadog_site: "datadoghq.eu".into(),
            ..Default::default()
        };
        assert_eq!(
            config.datadog_logs_endpoint().as_deref(),
            Some("https://http-intake.logs.datadoghq.eu/api/v2/logs")
        );
    }

    #[test]
    fn datadog_accepts_an_intake_host_as_the_site() {
        let config = ChannelConfig {
            datadog_api_key: Some("key".into()),
            datadog_site: "http-intake.logs.us3.datadoghq.com".into(),
            ..Default::default()
        };
        assert_eq!(
            config.datadog_logs_endpoint().as_deref(),
            Some("https://http-intake.logs.us3.datadoghq.com/api/v2/logs")
        );
    }

    #[test]
    fn datadog_explicit_endpoint_wins() {
        let config = ChannelConfig {
            datadog_api_key: Some("key".into()),
            datadog_endpoint: Some("http://localhost:9000/logs".into()),
            ..Default::default()
        };
        assert_eq!(
            config.datadog_logs_endpoint().as_deref(),
            Some("http://localhost:9000/logs")
        );
    }

    #[test]
    fn datadog_without_an_api_key_is_not_a_destination() {
        let config = ChannelConfig {
            datadog_site: "datadoghq.com".into(),
            ..Default::default()
        };
        assert!(config.datadog_logs_endpoint().is_none());
        assert!(!config.has_destination());
    }

    #[test]
    fn env_configuration_is_parsed_and_dd_api_key_is_a_fallback() {
        let config = ChannelConfig::from_vars(|name| match name {
            OTLP_ENDPOINT_ENV => Some("http://collector:4318/".into()),
            OTLP_HEADERS_ENV => Some("x-api-key=secret, x-tenant = acme".into()),
            DD_API_KEY_ENV => Some("dd-key".into()),
            DATADOG_SITE_ENV => Some("datadoghq.eu".into()),
            _ => None,
        });

        assert_eq!(
            config.otlp_logs_endpoint().as_deref(),
            Some("http://collector:4318/v1/logs")
        );
        assert_eq!(
            config.otlp_headers,
            vec![
                ("x-api-key".to_string(), "secret".to_string()),
                ("x-tenant".to_string(), "acme".to_string()),
            ]
        );
        assert_eq!(config.datadog_api_key.as_deref(), Some("dd-key"));
        assert_eq!(config.datadog_site, "datadoghq.eu");
    }

    #[test]
    fn debug_does_not_reveal_the_datadog_key() {
        let config = ChannelConfig {
            datadog_api_key: Some("super-secret".into()),
            ..Default::default()
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn disabled_config_has_no_destination_when_disabled() {
        let config = ChannelConfig {
            enabled: false,
            otlp_endpoint: Some("http://localhost:4318".into()),
            ..Default::default()
        };
        // Endpoint derivation is independent of the switch, but `new` refuses
        // to build a channel set.
        assert!(TelemetryChannels::new(config).is_none());
    }

    #[test]
    fn empty_config_builds_no_channels() {
        assert!(TelemetryChannels::new(ChannelConfig::default()).is_none());
    }

    // ── Event mapping ───────────────────────────────────────────────────

    #[test]
    fn otlp_payload_maps_events_to_log_records() {
        let payload = build_otlp_payload(&Scrubber::new(), &sample_events());
        let records = &payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"];
        assert_eq!(records.as_array().unwrap().len(), 2);

        let tool = &records[0];
        assert_eq!(tool["body"]["stringValue"], json!("tool_used"));
        assert_eq!(tool["severityNumber"], json!(9));
        assert_eq!(tool["severityText"], json!("INFO"));
        assert_eq!(tool["timeUnixNano"], json!("1767225600000000000"));

        let attributes = tool["attributes"].as_array().unwrap();
        let attribute = |key: &str| {
            attributes
                .iter()
                .find(|entry| entry["key"] == json!(key))
                .unwrap_or_else(|| panic!("missing attribute {key}"))
        };
        assert_eq!(
            attribute("event.type")["value"]["stringValue"],
            json!("tool_used")
        );
        assert_eq!(
            attribute("tool_name")["value"]["stringValue"],
            json!("Bash")
        );
        assert_eq!(attribute("success")["value"]["boolValue"], json!(true));
        assert_eq!(attribute("duration_ms")["value"]["intValue"], json!("12"));

        let error = &records[1];
        assert_eq!(error["severityNumber"], json!(17));
        assert_eq!(error["severityText"], json!("ERROR"));
    }

    #[test]
    fn otlp_payload_scrubs_secrets() {
        let events = vec![pending_event(
            "error",
            &json!({
                "type": "error",
                "message": "mail bob@example.com key A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6",
            }),
        )];
        let rendered = build_otlp_payload(&Scrubber::new(), &events).to_string();
        assert!(!rendered.contains("bob@example.com"), "{rendered}");
        assert!(
            !rendered.contains("A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6"),
            "{rendered}"
        );
        assert!(rendered.contains("<EMAIL>"), "{rendered}");
        assert!(rendered.contains("<SECRET>"), "{rendered}");
    }

    #[test]
    fn datadog_payload_maps_events_and_scrubs() {
        let payload = build_datadog_logs(&Scrubber::new(), &sample_events());
        assert_eq!(payload.len(), 2);

        assert_eq!(payload[0]["ddsource"], json!("coderex"));
        assert_eq!(payload[0]["service"], json!("coderex"));
        assert_eq!(payload[0]["status"], json!("info"));
        assert_eq!(payload[0]["timestamp"], json!(1_767_225_600_000_u128));
        assert!(
            payload[0]["ddtags"]
                .as_str()
                .unwrap()
                .contains("event_type:tool_used")
        );

        assert_eq!(payload[1]["status"], json!("error"));
        let message = payload[1]["message"].as_str().unwrap();
        assert!(!message.contains("bob@example.com"), "{message}");
        assert!(message.contains("<EMAIL>"), "{message}");
    }

    // ── HTTP delivery (mock server) ─────────────────────────────────────

    #[tokio::test]
    async fn sends_to_both_channels_with_expected_wire_format() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/logs"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v2/logs"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let channels = TelemetryChannels::new(ChannelConfig {
            otlp_endpoint: Some(format!("{}/v1/logs", server.uri())),
            datadog_api_key: Some("dd-key".into()),
            datadog_endpoint: Some(format!("{}/api/v2/logs", server.uri())),
            ..Default::default()
        })
        .unwrap();

        channels.send_events(&sample_events()).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);

        let otlp = requests
            .iter()
            .find(|request| request.url.path() == "/v1/logs")
            .expect("OTLP request");
        let otlp_body: serde_json::Value = serde_json::from_slice(&otlp.body).unwrap();
        assert_eq!(
            otlp_body["resourceLogs"][0]["resource"]["attributes"][0]["value"]["stringValue"],
            json!("coderex")
        );

        let datadog = requests
            .iter()
            .find(|request| request.url.path() == "/api/v2/logs")
            .expect("Datadog request");
        assert_eq!(
            datadog.headers.get("DD-API-KEY").unwrap().to_str().unwrap(),
            "dd-key"
        );
        let datadog_body: serde_json::Value = serde_json::from_slice(&datadog.body).unwrap();
        assert!(datadog_body.is_array());
        assert_eq!(datadog_body[0]["ddsource"], json!("coderex"));
    }

    #[tokio::test]
    async fn disabled_channels_send_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let config = ChannelConfig {
            enabled: false,
            otlp_endpoint: Some(format!("{}/v1/logs", server.uri())),
            datadog_api_key: Some("dd-key".into()),
            datadog_endpoint: Some(format!("{}/api/v2/logs", server.uri())),
            ..Default::default()
        };

        // No channel set can be built while disabled.
        assert!(TelemetryChannels::new(config).is_none());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_http_failure_is_reported_but_other_channels_still_send() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/logs"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v2/logs"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let channels = TelemetryChannels::new(ChannelConfig {
            otlp_endpoint: Some(format!("{}/v1/logs", server.uri())),
            datadog_api_key: Some("dd-key".into()),
            datadog_endpoint: Some(format!("{}/api/v2/logs", server.uri())),
            ..Default::default()
        })
        .unwrap();

        let result = channels.send_events(&sample_events()).await;
        assert!(result.is_err(), "the OTLP failure must surface");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}
