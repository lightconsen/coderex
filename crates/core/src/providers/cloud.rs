//! Cloud providers — picking one, and speaking to it.
//!
//! Every provider below answers the same Anthropic Messages request; what
//! differs is where the model goes in the URL, what has to be added to the
//! body, and how the request is authenticated. That is the whole of it, so it
//! is expressed as data: a [`RequestShape`] per provider, plus an
//! [`AuthStyle`] for the transport to apply.
//!
//! Two things the reference has that this does not: the managed credential
//! chains (the AWS SDK's provider chain, Google application default
//! credentials, Azure's `DefaultAzureCredential`). Those pull in three cloud
//! SDKs. Here the request is signed with credentials the operator supplies —
//! an access token, a static key, or an AWS key pair in the environment —
//! which covers the deployments that configure credentials explicitly.

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

type HmacSha256 = Hmac<Sha256>;

/// Which provider a request goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    /// A gateway in front of everything else; configured, not vendor-run.
    Gateway,
    /// Amazon Bedrock.
    Bedrock,
    /// Microsoft Foundry.
    Foundry,
    /// The Claude platform on AWS.
    AnthropicAws,
    /// The Claude platform on Google Cloud.
    AnthropicGoogleCloud,
    /// Amazon Bedrock (Mantle).
    Mantle,
    /// Google Vertex AI.
    Vertex,
    /// Anthropic's own API.
    FirstParty,
}

impl ProviderKind {
    /// Every provider, in selection order.
    pub const ALL: [Self; 8] = [
        Self::Gateway,
        Self::Bedrock,
        Self::Foundry,
        Self::AnthropicAws,
        Self::AnthropicGoogleCloud,
        Self::Mantle,
        Self::Vertex,
        Self::FirstParty,
    ];

    /// The name used in logs and configuration.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Bedrock => "bedrock",
            Self::Foundry => "foundry",
            Self::AnthropicAws => "anthropicAws",
            Self::AnthropicGoogleCloud => "anthropicGoogleCloud",
            Self::Mantle => "mantle",
            Self::Vertex => "vertex",
            Self::FirstParty => "firstParty",
        }
    }

    /// What a person calls it.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Gateway => "Cloud gateway",
            Self::Bedrock => "Amazon Bedrock",
            Self::Foundry => "Microsoft Foundry",
            Self::AnthropicAws => "Claude Platform on AWS",
            Self::AnthropicGoogleCloud => "Claude Platform on Google Cloud",
            Self::Mantle => "Amazon Bedrock (Mantle)",
            Self::Vertex => "Google Vertex AI",
            Self::FirstParty => "Anthropic API",
        }
    }

    /// The environment variable that selects it, if there is one.
    #[must_use]
    pub const fn enable_var(self) -> Option<&'static str> {
        match self {
            Self::Gateway | Self::FirstParty => None,
            Self::Bedrock => Some("CLAUDE_CODE_USE_BEDROCK"),
            Self::Foundry => Some("CLAUDE_CODE_USE_FOUNDRY"),
            Self::AnthropicAws => Some("CLAUDE_CODE_USE_ANTHROPIC_AWS"),
            Self::AnthropicGoogleCloud => Some("CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD"),
            Self::Mantle => Some("CLAUDE_CODE_USE_MANTLE"),
            Self::Vertex => Some("CLAUDE_CODE_USE_VERTEX"),
        }
    }

    /// How a request to it is authenticated.
    #[must_use]
    pub const fn auth(self) -> AuthStyle {
        match self {
            Self::Bedrock | Self::AnthropicAws | Self::Mantle => AuthStyle::AwsSigV4,
            Self::Vertex | Self::AnthropicGoogleCloud => AuthStyle::GoogleAccessToken,
            Self::Foundry => AuthStyle::AzureApiKey,
            Self::Gateway | Self::FirstParty => AuthStyle::AnthropicApiKey,
        }
    }

    /// Whether a bearer token may stand in for a signed request.
    ///
    /// Bedrock and Mantle accept a bearer token instead of a signature, which
    /// is what makes them usable without an AWS SDK.
    #[must_use]
    pub const fn accepts_bearer(self) -> bool {
        matches!(self, Self::Bedrock | Self::Mantle)
    }
}

/// A deployment: which provider, and where in it the work runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudEndpoint {
    /// The provider.
    pub kind: ProviderKind,
    /// The Google Cloud project, for Vertex.
    pub project: String,
    /// The region the endpoint is in.
    pub region: String,
}

impl CloudEndpoint {
    /// A deployment of `kind` in `region`.
    #[must_use]
    pub fn new(kind: ProviderKind, region: impl Into<String>) -> Self {
        Self {
            kind,
            project: String::new(),
            region: region.into(),
        }
    }

    /// The same deployment with a project set.
    #[must_use]
    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = project.into();
        self
    }
}

/// How a provider authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStyle {
    /// `x-api-key`, as Anthropic's own API uses.
    AnthropicApiKey,
    /// `Authorization: Bearer`.
    Bearer,
    /// AWS signature version 4.
    AwsSigV4,
    /// A Google OAuth access token, sent as a bearer token.
    GoogleAccessToken,
    /// Azure's `api-key` header.
    AzureApiKey,
}

/// What to read to find out which provider to use.
#[derive(Debug, Clone, Default)]
pub struct SelectionInputs {
    /// Whether a gateway is configured. It outranks everything.
    pub gateway_configured: bool,
    /// Variables that are set, by name.
    pub flags: BTreeMap<String, String>,
}

impl SelectionInputs {
    /// Read the selection inputs from the environment.
    #[must_use]
    pub fn from_env() -> Self {
        let mut flags = BTreeMap::new();
        for kind in ProviderKind::ALL {
            if let Some(name) = kind.enable_var() {
                if let Ok(value) = std::env::var(name) {
                    flags.insert(name.to_string(), value);
                }
            }
        }
        Self {
            gateway_configured: std::env::var("CODEREX_GATEWAY_URL").is_ok(),
            flags,
        }
    }

    fn enabled(&self, name: &str) -> bool {
        self.flags
            .get(name)
            .is_some_and(|value| !value.is_empty() && value != "0" && value != "false")
    }
}

/// Choose the provider, in the order the reference uses.
///
/// The order matters and is not alphabetical: a gateway is an explicit routing
/// decision and outranks a vendor switch, and the vendor switches themselves
/// are ordered so that a deployment which sets two of them gets the same
/// answer every time rather than whichever the environment happened to
/// enumerate first.
#[must_use]
pub fn select(inputs: &SelectionInputs) -> ProviderKind {
    if inputs.gateway_configured {
        return ProviderKind::Gateway;
    }

    for kind in [
        ProviderKind::Bedrock,
        ProviderKind::Foundry,
        ProviderKind::AnthropicAws,
        ProviderKind::AnthropicGoogleCloud,
        ProviderKind::Mantle,
        ProviderKind::Vertex,
    ] {
        if let Some(name) = kind.enable_var() {
            if inputs.enabled(name) {
                return kind;
            }
        }
    }

    ProviderKind::FirstParty
}

/// How a provider's request differs from a plain Anthropic one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestShape {
    /// Path template; `{model}` is replaced. May be absolute, in which case it
    /// replaces the base URL's path.
    pub path: String,
    /// Body fields to add.
    pub body_extras: BTreeMap<String, Value>,
    /// Headers to add, with values that may contain `{model}`.
    pub extra_headers: BTreeMap<String, String>,
    /// Headers to move into the body under a new name.
    pub header_to_body: BTreeMap<String, String>,
    /// Query parameters to drop.
    pub drop_query: Vec<String>,
    /// Whether the model leaves the body once it is in the path.
    pub model_in_path: bool,
}

impl RequestShape {
    /// The shape Anthropic's own API uses.
    #[must_use]
    pub fn anthropic() -> Self {
        Self {
            path: "/v1/messages".to_string(),
            body_extras: BTreeMap::new(),
            extra_headers: BTreeMap::new(),
            header_to_body: BTreeMap::new(),
            drop_query: Vec::new(),
            model_in_path: false,
        }
    }

    /// The shape for a provider.
    #[must_use]
    pub fn for_provider(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::FirstParty | ProviderKind::Gateway => Self::anthropic(),
            ProviderKind::Bedrock => Self {
                // Bedrock takes the model in the path and streams on a
                // different path, not through a `stream` flag.
                path: "/model/{model}/invoke".to_string(),
                body_extras: BTreeMap::from([(
                    "anthropic_version".to_string(),
                    json!("bedrock-2023-05-31"),
                )]),
                extra_headers: BTreeMap::from([(
                    "X-Amzn-Bedrock-Service-Tier".to_string(),
                    String::new(),
                )]),
                // Bedrock cannot read the beta header, so it is carried in the
                // body instead of dropped.
                header_to_body: BTreeMap::from([(
                    "anthropic-beta".to_string(),
                    "anthropic_beta".to_string(),
                )]),
                drop_query: vec!["beta".to_string()],
                model_in_path: true,
            },
            ProviderKind::Mantle => Self {
                path: "/model/{model}/invoke".to_string(),
                body_extras: BTreeMap::from([(
                    "anthropic_version".to_string(),
                    json!("bedrock-2023-05-31"),
                )]),
                extra_headers: BTreeMap::new(),
                header_to_body: BTreeMap::new(),
                drop_query: vec!["beta".to_string()],
                model_in_path: true,
            },
            ProviderKind::Vertex => Self {
                path: "/projects/{project}/locations/{region}/publishers/anthropic/models/{model}:rawPredict"
                    .to_string(),
                body_extras: BTreeMap::from([(
                    "anthropic_version".to_string(),
                    json!("vertex-2023-10-16"),
                )]),
                extra_headers: BTreeMap::new(),
                header_to_body: BTreeMap::new(),
                drop_query: Vec::new(),
                model_in_path: false,
            },
            ProviderKind::AnthropicGoogleCloud => Self {
                path: "/v1/messages".to_string(),
                body_extras: BTreeMap::new(),
                extra_headers: BTreeMap::new(),
                header_to_body: BTreeMap::new(),
                drop_query: Vec::new(),
                model_in_path: false,
            },
            ProviderKind::Foundry => Self {
                path: "/v1/messages".to_string(),
                body_extras: BTreeMap::new(),
                extra_headers: BTreeMap::new(),
                header_to_body: BTreeMap::new(),
                drop_query: Vec::new(),
                model_in_path: false,
            },
            ProviderKind::AnthropicAws => Self::anthropic(),
        }
    }

    /// The streaming variant of this shape.
    ///
    /// Bedrock and Mantle stream on their own path; everyone else asks for a
    /// stream the way Anthropic does.
    #[must_use]
    pub fn streaming(&self, kind: ProviderKind) -> Self {
        let mut shape = self.clone();
        if let Some(base) = shape.path.strip_suffix("/invoke") {
            shape.path = format!("{base}/invoke-with-response-stream");
        } else if kind == ProviderKind::Vertex {
            shape.path = shape.path.replace(":rawPredict", ":streamRawPredict");
        }
        shape
    }

    /// Render the path for a model and deployment.
    #[must_use]
    pub fn render_path(&self, model: &str, project: &str, region: &str) -> String {
        self.path
            .replace("{model}", model)
            .replace("{project}", project)
            .replace("{region}", region)
    }

    /// Add this shape's body fields to a request body.
    pub fn apply_to_body(&self, body: &mut Value) {
        let Some(object) = body.as_object_mut() else {
            return;
        };
        for (name, value) in &self.body_extras {
            object.insert(name.clone(), value.clone());
        }
    }
}

/// Where a provider's requests go.
#[must_use]
pub fn default_base_url(kind: ProviderKind, region: &str) -> String {
    match kind {
        ProviderKind::Gateway | ProviderKind::FirstParty => "https://api.anthropic.com".to_string(),
        ProviderKind::Bedrock => {
            format!("https://bedrock-runtime.{region}.amazonaws.com")
        }
        ProviderKind::Mantle => format!("https://bedrock-mantle.{region}.api.aws/anthropic"),
        ProviderKind::AnthropicAws => {
            "https://aws-external-anthropic.us-east-1.api.aws".to_string()
        }
        ProviderKind::AnthropicGoogleCloud => "https://claude.googleapis.com".to_string(),
        ProviderKind::Vertex => vertex_base_url(region),
        // Foundry is per-resource; without one there is no sensible default.
        ProviderKind::Foundry => String::new(),
    }
}

/// Vertex's base URL for a region.
///
/// The global, US, and EU endpoints are not `{region}-aiplatform`: they are
/// their own hosts, which is why a region cannot simply be substituted in.
#[must_use]
pub fn vertex_base_url(region: &str) -> String {
    match region {
        "global" => "https://aiplatform.googleapis.com/v1".to_string(),
        "us" => "https://us.rep.googleapis.com/v1".to_string(),
        "eu" => "https://eu.rep.googleapis.com/v1".to_string(),
        other => format!("https://{other}-aiplatform.googleapis.com/v1"),
    }
}

/// The endpoint Foundry serves a resource from.
#[must_use]
pub fn foundry_base_url(resource: &str) -> String {
    if resource.is_empty() {
        return String::new();
    }
    format!("https://{resource}.services.ai.azure.com/anthropic")
}

/// A signed AWS request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedRequest {
    /// The `Authorization` header value.
    pub authorization: String,
    /// The `x-amz-date` header value.
    pub amz_date: String,
    /// The payload hash, which is the last line of the canonical request.
    ///
    /// It is *not* sent as a header: `x-amz-content-sha256` is an S3
    /// requirement, and an `x-amz-*` header that is sent has to be signed —
    /// which would mean the signed-header set is no longer the one AWS
    /// publishes test vectors for. Bedrock takes the hash from the signature.
    pub payload_sha256: String,
}

/// Credentials to sign an AWS request with.
#[derive(Debug, Clone)]
pub struct AwsCredentials {
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// Session token, for temporary credentials.
    pub session_token: Option<String>,
}

impl AwsCredentials {
    /// Read credentials from the environment, the way the AWS CLI does.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").ok()?;
        let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok()?;
        Some(Self {
            access_key_id,
            secret_access_key,
            session_token: std::env::var("AWS_SESSION_TOKEN").ok(),
        })
    }
}

/// Sign a request with AWS signature version 4.
///
/// `service` is the signing name — `bedrock-runtime` for Bedrock,
/// `bedrock-mantle` for Mantle — which is part of the derived key, not just a
/// label, so getting it wrong fails the signature.
#[must_use]
pub fn sign_v4(
    credentials: &AwsCredentials,
    method: &str,
    host: &str,
    path: &str,
    query: &str,
    body: &[u8],
    region: &str,
    service: &str,
    timestamp: &str,
) -> SignedRequest {
    let payload_sha256 = hex(&Sha256::digest(body));
    let (amz_date, date_stamp) = split_timestamp(timestamp);

    // The minimal signed set: `host` and `x-amz-date`. Every additional signed
    // header changes the canonical request, and this is the set AWS publishes
    // test vectors for, so a wrong key derivation shows up in a test rather
    // than as a 403 in a deployment.
    let canonical_headers = format!("host:{host}\nx-amz-date:{amz_date}\n");
    let signed_headers = "host;x-amz-date";
    let canonical_request = format!(
        "{method}\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_sha256}"
    );

    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );

    let signing_key = signing_key(&credentials.secret_access_key, &date_stamp, region, service);
    let signature = hex(&hmac(&signing_key, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    );

    SignedRequest {
        authorization,
        amz_date,
        payload_sha256,
    }
}

/// The signing name a provider uses.
#[must_use]
pub const fn signing_service(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Mantle => "bedrock-mantle",
        _ => "bedrock-runtime",
    }
}

fn signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    hmac(&k_service, b"aws4_request")
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Split an RFC 3339 timestamp into the two forms the signature needs.
fn split_timestamp(timestamp: &str) -> (String, String) {
    let compact: String = timestamp
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == 'T')
        .take(16)
        .collect();
    // `20260115T103000Z` → `20260115T103000Z` and `20260115`.
    let amz_date = if compact.ends_with('Z') {
        compact
    } else {
        format!("{compact}Z")
    };
    let date_stamp = amz_date.chars().take(8).collect();
    (amz_date, date_stamp)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(flags: &[(&str, &str)]) -> SelectionInputs {
        SelectionInputs {
            gateway_configured: false,
            flags: flags
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect(),
        }
    }

    #[test]
    fn nothing_set_means_the_anthropic_api() {
        assert_eq!(select(&inputs(&[])), ProviderKind::FirstParty);
    }

    #[test]
    fn each_switch_selects_its_provider() {
        for kind in ProviderKind::ALL {
            let Some(name) = kind.enable_var() else {
                continue;
            };
            let selected = select(&inputs(&[(name, "1")]));
            assert_eq!(selected, kind, "{name} selected {selected:?}");
        }
    }

    #[test]
    fn a_gateway_outranks_a_vendor_switch() {
        let mut selection = inputs(&[("CLAUDE_CODE_USE_BEDROCK", "1")]);
        selection.gateway_configured = true;
        assert_eq!(select(&selection), ProviderKind::Gateway);
    }

    #[test]
    fn the_priority_between_switches_is_fixed() {
        // Bedrock outranks Foundry, which outranks everything below it.
        let selection = inputs(&[
            ("CLAUDE_CODE_USE_VERTEX", "1"),
            ("CLAUDE_CODE_USE_FOUNDRY", "1"),
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
        ]);
        assert_eq!(select(&selection), ProviderKind::Bedrock);

        let selection = inputs(&[
            ("CLAUDE_CODE_USE_VERTEX", "1"),
            ("CLAUDE_CODE_USE_FOUNDRY", "1"),
        ]);
        assert_eq!(select(&selection), ProviderKind::Foundry);

        let selection = inputs(&[
            ("CLAUDE_CODE_USE_VERTEX", "1"),
            ("CLAUDE_CODE_USE_MANTLE", "1"),
        ]);
        assert_eq!(select(&selection), ProviderKind::Mantle);
    }

    #[test]
    fn a_switch_that_is_off_does_not_select() {
        for value in ["", "0", "false"] {
            assert_eq!(
                select(&inputs(&[("CLAUDE_CODE_USE_BEDROCK", value)])),
                ProviderKind::FirstParty,
                "{value:?} selected a provider"
            );
        }
    }

    #[test]
    fn every_provider_knows_how_it_authenticates() {
        assert_eq!(ProviderKind::Bedrock.auth(), AuthStyle::AwsSigV4);
        assert_eq!(ProviderKind::Vertex.auth(), AuthStyle::GoogleAccessToken);
        assert_eq!(ProviderKind::Foundry.auth(), AuthStyle::AzureApiKey);
        assert_eq!(ProviderKind::FirstParty.auth(), AuthStyle::AnthropicApiKey);
        assert!(ProviderKind::Bedrock.accepts_bearer());
        assert!(!ProviderKind::Vertex.accepts_bearer());
    }

    #[test]
    fn bedrock_puts_the_model_in_the_path_and_stamps_the_body() {
        let shape = RequestShape::for_provider(ProviderKind::Bedrock);
        assert_eq!(
            shape.render_path("claude-sonnet-4-6", "", "us-east-1"),
            "/model/claude-sonnet-4-6/invoke"
        );
        assert!(shape.model_in_path);

        let mut body = json!({"model": "claude-sonnet-4-6", "max_tokens": 10});
        shape.apply_to_body(&mut body);
        assert_eq!(body["anthropic_version"], "bedrock-2023-05-31");

        // The beta header cannot be read by Bedrock, so it moves into the body.
        assert_eq!(
            shape
                .header_to_body
                .get("anthropic-beta")
                .map(String::as_str),
            Some("anthropic_beta")
        );
        assert_eq!(shape.drop_query, vec!["beta".to_string()]);
    }

    #[test]
    fn bedrock_streams_on_its_own_path() {
        let shape = RequestShape::for_provider(ProviderKind::Bedrock);
        let streaming = shape.streaming(ProviderKind::Bedrock);
        assert_eq!(
            streaming.render_path("m", "", "r"),
            "/model/m/invoke-with-response-stream"
        );
    }

    #[test]
    fn vertex_names_the_project_and_region_in_the_path() {
        let shape = RequestShape::for_provider(ProviderKind::Vertex);
        assert_eq!(
            shape.render_path("claude-sonnet-4-6", "my-project", "us-central1"),
            "/projects/my-project/locations/us-central1/publishers/anthropic/models/\
             claude-sonnet-4-6:rawPredict"
        );

        let mut body = json!({});
        shape.apply_to_body(&mut body);
        assert_eq!(body["anthropic_version"], "vertex-2023-10-16");

        assert!(
            shape
                .streaming(ProviderKind::Vertex)
                .render_path("m", "p", "r")
                .ends_with(":streamRawPredict")
        );
    }

    #[test]
    fn vertex_regions_have_their_own_hosts() {
        assert_eq!(
            vertex_base_url("global"),
            "https://aiplatform.googleapis.com/v1"
        );
        assert_eq!(vertex_base_url("us"), "https://us.rep.googleapis.com/v1");
        assert_eq!(vertex_base_url("eu"), "https://eu.rep.googleapis.com/v1");
        assert_eq!(
            vertex_base_url("asia-northeast1"),
            "https://asia-northeast1-aiplatform.googleapis.com/v1"
        );
    }

    #[test]
    fn foundry_is_per_resource_and_has_no_default_without_one() {
        assert!(default_base_url(ProviderKind::Foundry, "us").is_empty());
        assert_eq!(
            foundry_base_url("my-resource"),
            "https://my-resource.services.ai.azure.com/anthropic"
        );
        assert!(foundry_base_url("").is_empty());
    }

    #[test]
    fn the_mantle_endpoint_carries_its_own_service_name() {
        assert_eq!(
            default_base_url(ProviderKind::Mantle, "us-east-1"),
            "https://bedrock-mantle.us-east-1.api.aws/anthropic"
        );
        assert_eq!(signing_service(ProviderKind::Mantle), "bedrock-mantle");
        assert_eq!(signing_service(ProviderKind::Bedrock), "bedrock-runtime");
    }

    #[test]
    fn a_signature_matches_the_published_test_vector() {
        // The example from the AWS documentation ("Signature Version 4 test
        // suite" / the GET-vanilla case): a known key, request, and timestamp
        // whose signature is published, so a wrong derivation shows up here
        // rather than as an opaque 403 in production.
        let credentials = AwsCredentials {
            access_key_id: "AKIDEXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
        };

        let signed = sign_v4(
            &credentials,
            "GET",
            "example.amazonaws.com",
            "/",
            "",
            b"",
            "us-east-1",
            "service",
            "2015-08-30T12:36:00Z",
        );

        assert_eq!(signed.amz_date, "20150830T123600Z");
        assert_eq!(
            signed.payload_sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            signed.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn the_signature_covers_the_body_and_the_service_name() {
        let credentials = AwsCredentials {
            access_key_id: "AKIDEXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
        };
        let sign = |body: &[u8], service: &str| {
            sign_v4(
                &credentials,
                "POST",
                "bedrock-runtime.us-east-1.amazonaws.com",
                "/model/m/invoke",
                "",
                body,
                "us-east-1",
                service,
                "2026-01-15T10:30:00Z",
            )
            .authorization
        };

        let base = sign(b"{}", "bedrock-runtime");
        assert_ne!(base, sign(b"{\"a\":1}", "bedrock-runtime"), "body ignored");
        assert_ne!(base, sign(b"{}", "bedrock-mantle"), "service ignored");
    }

    #[test]
    fn the_timestamp_is_split_the_way_the_signature_needs() {
        assert_eq!(
            split_timestamp("2026-01-15T10:30:00Z"),
            ("20260115T103000Z".to_string(), "20260115".to_string())
        );
        assert_eq!(
            split_timestamp("20260115T103000Z"),
            ("20260115T103000Z".to_string(), "20260115".to_string())
        );
    }
}
