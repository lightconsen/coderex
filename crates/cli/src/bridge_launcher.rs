//! Production entry point for the bridge worker.
//!
//! `coderex bridge` resolves a [`BridgeLaunchConfig`] from CLI flags and
//! environment variables, builds the configured transport through
//! `coderex_bridge::transport::default_factory()`, constructs a [`Bridge`],
//! and runs [`Bridge::serve_transport`] until Ctrl+C or SIGTERM.
//!
//! The transport is injectable ([`run_with_transport`]) so tests can drive a
//! real bridge over an in-process transport without a network.

use anyhow::{Context, Result};
use coderex_bridge::heartbeat::HeartbeatConfig;
use coderex_bridge::orchestrator::{Bridge, BridgeConfig};
use coderex_bridge::transport::{Transport, TransportConfig, default_factory};
use std::future::Future;
use std::path::{Path, PathBuf};

use crate::cli::BridgeArgs;

/// Environment variable naming the control-plane HTTP API.
pub const ENV_CONTROL_PLANE_URL: &str = "CODEREX_BRIDGE_CONTROL_PLANE_URL";
/// Environment variable naming this environment.
pub const ENV_ENV_ID: &str = "CODEREX_BRIDGE_ENV_ID";
/// Environment variable naming the repository root.
pub const ENV_REPO_ROOT: &str = "CODEREX_BRIDGE_REPO_ROOT";
/// Environment variable naming the worktree base directory.
pub const ENV_WORKTREE_BASE: &str = "CODEREX_BRIDGE_WORKTREE_BASE";
/// Environment variable naming the session base directory.
pub const ENV_SESSION_BASE: &str = "CODEREX_BRIDGE_SESSION_BASE";
/// Environment variable naming the archive base directory.
pub const ENV_ARCHIVE_BASE: &str = "CODEREX_BRIDGE_ARCHIVE_BASE";
/// Environment variable selecting the transport kind.
pub const ENV_TRANSPORT: &str = "CODEREX_BRIDGE_TRANSPORT";
/// Environment variable naming the transport endpoint. The historical spelling
/// is a `ws://host:port` URL.
pub const ENV_TRANSPORT_URL: &str = "CODEREX_BRIDGE_URL";
/// Environment variable overriding the transport connection ID.
pub const ENV_CONNECTION_ID: &str = "CODEREX_BRIDGE_CONNECTION_ID";
/// Environment variable carrying the bearer token. Prefer a flag or an
/// inherited environment over a shell history entry.
pub const ENV_AUTH_TOKEN: &str = "CODEREX_BRIDGE_TOKEN";

/// Default transport kind: the real gateway transport.
pub const DEFAULT_TRANSPORT: &str = "websocket";
/// Default maximum concurrent sessions.
pub const DEFAULT_MAX_CONCURRENT: usize = 5;

/// Errors raised while turning CLI arguments into a launch configuration.
#[derive(Debug, thiserror::Error)]
pub enum BridgeConfigError {
    /// One or more required values were not supplied by flag or environment.
    #[error("missing required bridge configuration: {0}")]
    MissingRequired(String),
    /// A supplied value cannot be used as given.
    #[error("invalid bridge configuration: {0}")]
    Invalid(String),
}

/// A fully resolved bridge launch configuration.
#[derive(Debug, Clone)]
pub struct BridgeLaunchConfig {
    /// Control-plane base URL (HTTP API).
    pub control_plane_url: String,
    /// Environment identifier.
    pub env_id: String,
    /// Git repository root.
    pub repo_root: PathBuf,
    /// Base directory for worktrees.
    pub worktree_base: PathBuf,
    /// Base directory for session data.
    pub session_base: PathBuf,
    /// Base directory for archives.
    pub archive_base: PathBuf,
    /// Maximum concurrent sessions.
    pub max_concurrent: usize,
    /// Heartbeat settings.
    pub heartbeat: HeartbeatConfig,
    /// Registered transport kind.
    pub transport: String,
    /// Transport endpoint.
    pub transport_url: String,
    /// Connection identifier reported by the transport.
    pub connection_id: String,
    /// Optional bearer token.
    pub auth_token: Option<String>,
}

impl BridgeLaunchConfig {
    /// Resolve flags, falling back to `env` for values the parser left empty.
    ///
    /// In production `env` is `std::env::var(..).ok()`; tests inject a map so
    /// the mapping can be checked without mutating the process environment.
    pub fn resolve<F>(args: &BridgeArgs, cwd: &Path, env: F) -> Result<Self, BridgeConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let control_plane_url = optional_string(
            args.control_plane_url.as_deref(),
            env(ENV_CONTROL_PLANE_URL),
        );
        let env_id = optional_string(args.env_id.as_deref(), env(ENV_ENV_ID));

        // Both required values are reported together; a person running the
        // command for the first time should not have to fix one at a time.
        if control_plane_url.is_none() || env_id.is_none() {
            let mut missing = Vec::new();
            if control_plane_url.is_none() {
                missing.push(format!("--control-plane-url (${ENV_CONTROL_PLANE_URL})"));
            }
            if env_id.is_none() {
                missing.push(format!("--env-id (${ENV_ENV_ID})"));
            }
            return Err(BridgeConfigError::MissingRequired(missing.join(", ")));
        }
        // The poller appends `/api/...`, so a trailing slash would produce a
        // doubled separator.
        let control_plane_url = control_plane_url
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string();
        let env_id = env_id.unwrap_or_default();

        let repo_root = optional_path(args.repo_root.clone(), env(ENV_REPO_ROOT))
            .unwrap_or_else(|| cwd.to_path_buf());
        if !repo_root.is_dir() {
            return Err(BridgeConfigError::Invalid(format!(
                "repository root {} is not a directory",
                repo_root.display()
            )));
        }

        let worktree_base = optional_path(args.worktree_base.clone(), env(ENV_WORKTREE_BASE))
            .unwrap_or_else(|| repo_root.join(".coderex/worktrees"));
        let session_base = optional_path(args.session_base.clone(), env(ENV_SESSION_BASE))
            .unwrap_or_else(|| repo_root.join(".coderex/sessions"));
        let archive_base = optional_path(args.archive_base.clone(), env(ENV_ARCHIVE_BASE))
            .unwrap_or_else(|| repo_root.join(".coderex/archives"));

        if args.max_concurrent == 0 {
            return Err(BridgeConfigError::Invalid(
                "--max-concurrent must be at least 1".into(),
            ));
        }
        if args.heartbeat_interval_secs == 0 {
            return Err(BridgeConfigError::Invalid(
                "--heartbeat-interval-secs must be at least 1".into(),
            ));
        }
        if args.heartbeat_timeout_secs == 0 {
            return Err(BridgeConfigError::Invalid(
                "--heartbeat-timeout-secs must be at least 1".into(),
            ));
        }
        if args.heartbeat_degraded_threshold == 0 {
            return Err(BridgeConfigError::Invalid(
                "--heartbeat-degraded-threshold must be at least 1".into(),
            ));
        }
        if args.heartbeat_lost_threshold < args.heartbeat_degraded_threshold {
            return Err(BridgeConfigError::Invalid(format!(
                "--heartbeat-lost-threshold ({}) must be at least the degraded threshold ({})",
                args.heartbeat_lost_threshold, args.heartbeat_degraded_threshold
            )));
        }

        let transport = optional_string(Some(args.transport.as_str()), env(ENV_TRANSPORT))
            .unwrap_or_else(|| DEFAULT_TRANSPORT.to_string());
        let transport_url = optional_string(args.transport_url.as_deref(), env(ENV_TRANSPORT_URL))
            .unwrap_or_else(|| derive_transport_url(&control_plane_url));
        let connection_id = optional_string(args.connection_id.as_deref(), env(ENV_CONNECTION_ID))
            .unwrap_or_else(|| env_id.clone());
        let auth_token = optional_string(args.auth_token.as_deref(), env(ENV_AUTH_TOKEN));

        if is_websocket(&transport) && !is_websocket_url(&transport_url) {
            return Err(BridgeConfigError::Invalid(format!(
                "transport '{transport}' needs a ws:// or wss:// URL, got '{transport_url}' \
                 (set --transport-url or ${ENV_TRANSPORT_URL})"
            )));
        }

        Ok(Self {
            control_plane_url,
            env_id,
            repo_root,
            worktree_base,
            session_base,
            archive_base,
            max_concurrent: args.max_concurrent,
            heartbeat: HeartbeatConfig {
                interval_secs: args.heartbeat_interval_secs,
                timeout_secs: args.heartbeat_timeout_secs,
                degraded_threshold: args.heartbeat_degraded_threshold,
                lost_threshold: args.heartbeat_lost_threshold,
            },
            transport,
            transport_url,
            connection_id,
            auth_token,
        })
    }

    /// Build the orchestrator configuration.
    #[must_use]
    pub fn bridge_config(&self) -> BridgeConfig {
        BridgeConfig {
            env_id: self.env_id.clone(),
            max_concurrent: self.max_concurrent,
            control_plane_url: self.control_plane_url.clone(),
            repo_root: self.repo_root.clone(),
            worktree_base: self.worktree_base.clone(),
            session_base: self.session_base.clone(),
            archive_base: self.archive_base.clone(),
            heartbeat: self.heartbeat,
        }
    }

    /// Endpoint label for diagnostics, with credentials left out.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        if is_websocket(&self.transport) {
            &self.transport_url
        } else {
            &self.env_id
        }
    }
}

/// Build the production transport from the resolved configuration.
///
/// Returns an error for an unregistered transport kind; it never silently
/// substitutes a no-op transport.
pub fn create_transport(config: &BridgeLaunchConfig) -> Result<Box<dyn Transport>> {
    let factory = default_factory();
    let transport_config = TransportConfig {
        kind: config.transport.clone(),
        endpoint: config.endpoint().to_string(),
        connection_id: config.connection_id.clone(),
        auth_token: config.auth_token.clone(),
    };

    factory.create_configured(&transport_config).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown bridge transport '{}'; available transports: {}",
            config.transport,
            factory.registered_types().join(", ")
        )
    })
}

/// Resolve the configuration, build the transport, and serve until shutdown.
pub async fn run(args: BridgeArgs, cwd: &Path) -> Result<()> {
    let config = BridgeLaunchConfig::resolve(&args, cwd, |key| std::env::var(key).ok())
        .context("cannot start the bridge")?;
    let transport = create_transport(&config)?;

    tracing::info!(
        env_id = %config.env_id,
        transport = %config.transport,
        endpoint = %config.endpoint(),
        control_plane = %config.control_plane_url,
        max_concurrent = config.max_concurrent,
        "starting bridge worker"
    );

    run_with_transport(config, transport, shutdown_signal()).await
}

/// Run a bridge over an injected transport until `shutdown` resolves.
///
/// This is the seam used by tests: the transport is supplied by the caller
/// and no process-wide signal handler is installed.
pub async fn run_with_transport<F>(
    config: BridgeLaunchConfig,
    transport: Box<dyn Transport>,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut bridge = Bridge::new(config.bridge_config(), transport);

    bridge.connect().await.map_err(|err| {
        anyhow::anyhow!(
            "bridge transport '{}' could not connect to {}: {err}",
            config.transport,
            config.endpoint()
        )
    })?;

    let stop = bridge.stop_handle();
    let shutdown_task = tokio::spawn(async move {
        shutdown.await;
        stop.stop();
    });

    let served = bridge.serve_transport().await;
    let _ = bridge.disconnect().await;
    shutdown_task.abort();

    served.map_err(|e| anyhow::anyhow!("bridge serve loop failed: {e}"))
}

/// Wait for Ctrl+C, or SIGTERM on Unix, to request a graceful bridge stop.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(err) => {
                tracing::warn!("cannot listen for SIGTERM ({err}); waiting for Ctrl+C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                match result {
                    Ok(()) => tracing::info!("Ctrl+C received — stopping bridge"),
                    Err(err) => tracing::warn!("Ctrl+C listener failed: {err}"),
                }
            }
            _ = terminate.recv() => tracing::info!("SIGTERM received — stopping bridge"),
        }
    }

    #[cfg(not(unix))]
    {
        match tokio::signal::ctrl_c().await {
            Ok(()) => tracing::info!("Ctrl+C received — stopping bridge"),
            Err(err) => tracing::warn!("Ctrl+C listener failed: {err}"),
        }
    }
}

/// Rewrite an HTTP control-plane URL to its WebSocket scheme.
///
/// The path is kept as-is; deployments whose gateway lives on a different
/// path should pass `--transport-url` explicitly.
#[must_use]
pub fn derive_transport_url(control_plane_url: &str) -> String {
    let trimmed = control_plane_url.trim_end_matches('/');
    match (
        trimmed.strip_prefix("https://"),
        trimmed.strip_prefix("http://"),
    ) {
        (Some(rest), _) => format!("wss://{rest}"),
        (None, Some(rest)) => format!("ws://{rest}"),
        (None, None) => trimmed.to_string(),
    }
}

fn optional_string(arg: Option<&str>, env: Option<String>) -> Option<String> {
    arg.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            env.map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

fn optional_path(arg: Option<PathBuf>, env: Option<String>) -> Option<PathBuf> {
    arg.or_else(|| optional_string(None, env).map(PathBuf::from))
}

fn is_websocket(transport: &str) -> bool {
    transport.eq_ignore_ascii_case("websocket") || transport.eq_ignore_ascii_case("ws")
}

fn is_websocket_url(url: &str) -> bool {
    url.starts_with("ws://") || url.starts_with("wss://")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;
    use coderex_bridge::transport::InProcessTransport;
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn args_with(url: &str, env_id: &str) -> BridgeArgs {
        BridgeArgs {
            control_plane_url: Some(url.into()),
            env_id: Some(env_id.into()),
            repo_root: None,
            worktree_base: None,
            session_base: None,
            archive_base: None,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            transport: DEFAULT_TRANSPORT.into(),
            transport_url: None,
            connection_id: None,
            auth_token: None,
            heartbeat_interval_secs: 30,
            heartbeat_timeout_secs: 10,
            heartbeat_degraded_threshold: 2,
            heartbeat_lost_threshold: 5,
        }
    }

    #[test]
    fn maps_every_field_from_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();

        let args = BridgeArgs {
            control_plane_url: Some("https://cp.example.test".into()),
            env_id: Some("env-1".into()),
            repo_root: Some(repo.clone()),
            worktree_base: Some(tmp.path().join("wt")),
            session_base: Some(tmp.path().join("sessions")),
            archive_base: Some(tmp.path().join("archives")),
            max_concurrent: 7,
            transport: "websocket".into(),
            transport_url: Some("wss://gateway.example.test/bridge".into()),
            connection_id: Some("conn-1".into()),
            auth_token: Some("secret".into()),
            heartbeat_interval_secs: 15,
            heartbeat_timeout_secs: 4,
            heartbeat_degraded_threshold: 3,
            heartbeat_lost_threshold: 9,
        };

        let config = BridgeLaunchConfig::resolve(&args, tmp.path(), |_| None).unwrap();
        assert_eq!(config.control_plane_url, "https://cp.example.test");
        assert_eq!(config.env_id, "env-1");
        assert_eq!(config.repo_root, repo);
        assert_eq!(config.worktree_base, tmp.path().join("wt"));
        assert_eq!(config.session_base, tmp.path().join("sessions"));
        assert_eq!(config.archive_base, tmp.path().join("archives"));
        assert_eq!(config.max_concurrent, 7);
        assert_eq!(config.heartbeat.interval_secs, 15);
        assert_eq!(config.heartbeat.timeout_secs, 4);
        assert_eq!(config.heartbeat.degraded_threshold, 3);
        assert_eq!(config.heartbeat.lost_threshold, 9);
        assert_eq!(config.transport, "websocket");
        assert_eq!(config.transport_url, "wss://gateway.example.test/bridge");
        assert_eq!(config.connection_id, "conn-1");
        assert_eq!(config.auth_token.as_deref(), Some("secret"));
    }

    #[test]
    fn parse_defaults_apply() {
        let parsed = Cli::try_parse_from([
            "coderex",
            "bridge",
            "--control-plane-url",
            "http://127.0.0.1:3000/",
            "--env-id",
            "env-local",
        ])
        .unwrap();

        let crate::cli::Command::Bridge(args) = parsed.command.unwrap() else {
            panic!("expected the bridge subcommand");
        };
        assert_eq!(args.max_concurrent, DEFAULT_MAX_CONCURRENT);
        assert_eq!(args.transport, DEFAULT_TRANSPORT);
        assert_eq!(args.heartbeat_interval_secs, 30);
        assert_eq!(args.heartbeat_timeout_secs, 10);
        assert_eq!(args.heartbeat_degraded_threshold, 2);
        assert_eq!(args.heartbeat_lost_threshold, 5);

        let cwd = std::env::current_dir().unwrap();
        let config = BridgeLaunchConfig::resolve(&args, &cwd, |_| None).unwrap();
        assert_eq!(config.repo_root, cwd);
        assert_eq!(config.worktree_base, cwd.join(".coderex/worktrees"));
        assert_eq!(config.session_base, cwd.join(".coderex/sessions"));
        assert_eq!(config.archive_base, cwd.join(".coderex/archives"));
        // The trailing slash is trimmed and the scheme is rewritten.
        assert_eq!(config.control_plane_url, "http://127.0.0.1:3000");
        assert_eq!(config.transport_url, "ws://127.0.0.1:3000");
        assert_eq!(config.connection_id, "env-local");
        assert!(config.auth_token.is_none());
    }

    #[test]
    fn environment_fills_required_and_optional_values() {
        let tmp = tempfile::tempdir().unwrap();
        let mut args = args_with("ignored", "ignored");
        args.control_plane_url = None;
        args.env_id = None;
        args.repo_root = None;
        args.transport_url = None;

        let repo = tmp.path().join("env-repo");
        std::fs::create_dir(&repo).unwrap();
        let config = BridgeLaunchConfig::resolve(&args, tmp.path(), |key| match key {
            ENV_CONTROL_PLANE_URL => Some("http://cp.example.test".into()),
            ENV_ENV_ID => Some("env-from-env".into()),
            ENV_REPO_ROOT => Some(repo.display().to_string()),
            ENV_TRANSPORT_URL => Some("ws://gateway.example.test:9000".into()),
            ENV_AUTH_TOKEN => Some("token-from-env".into()),
            _ => None,
        })
        .unwrap();

        assert_eq!(config.control_plane_url, "http://cp.example.test");
        assert_eq!(config.env_id, "env-from-env");
        assert_eq!(config.repo_root, repo);
        assert_eq!(config.transport_url, "ws://gateway.example.test:9000");
        assert_eq!(config.auth_token.as_deref(), Some("token-from-env"));
    }

    #[test]
    fn missing_required_values_are_reported_together() {
        let mut args = args_with("ignored", "ignored");
        args.control_plane_url = None;
        args.env_id = None;

        let error = BridgeLaunchConfig::resolve(&args, Path::new("."), |_| None).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("--control-plane-url"), "{message}");
        assert!(message.contains(ENV_CONTROL_PLANE_URL), "{message}");
        assert!(message.contains("--env-id"), "{message}");
        assert!(message.contains(ENV_ENV_ID), "{message}");
    }

    #[test]
    fn clap_rejects_a_bridge_command_without_required_values() {
        let result = Cli::try_parse_from(["coderex", "bridge"]);
        let error = result.unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        let message = error.to_string();
        assert!(message.contains("--control-plane-url"), "{message}");
        assert!(message.contains("--env-id"), "{message}");
    }

    #[test]
    fn bridge_help_is_available() {
        let error = Cli::try_parse_from(["coderex", "bridge", "--help"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = error.to_string();
        assert!(help.contains("--control-plane-url"), "{help}");
        assert!(help.contains("--auth-token"), "{help}");
        assert!(help.contains(ENV_CONTROL_PLANE_URL), "{help}");
    }

    #[test]
    fn rejects_zero_concurrency_and_bad_heartbeat_thresholds() {
        let mut args = args_with("http://cp.example.test", "env-1");
        args.max_concurrent = 0;
        assert!(
            BridgeLaunchConfig::resolve(&args, Path::new("."), |_| None)
                .unwrap_err()
                .to_string()
                .contains("--max-concurrent")
        );

        let mut args = args_with("http://cp.example.test", "env-1");
        args.heartbeat_lost_threshold = 1;
        args.heartbeat_degraded_threshold = 2;
        assert!(
            BridgeLaunchConfig::resolve(&args, Path::new("."), |_| None)
                .unwrap_err()
                .to_string()
                .contains("--heartbeat-lost-threshold")
        );
    }

    #[test]
    fn rejects_a_non_websocket_url_for_the_websocket_transport() {
        let mut args = args_with("unix:///run/control.sock", "env-1");
        args.transport_url = None;
        let error = BridgeLaunchConfig::resolve(&args, Path::new("."), |_| None).unwrap_err();
        assert!(error.to_string().contains("ws:// or wss://"));
    }

    #[test]
    fn unknown_transport_fails_without_a_fallback() {
        let mut args = args_with("http://cp.example.test", "env-1");
        args.transport = "carrier-pigeon".into();
        args.transport_url = Some("ws://gateway.example.test".into());
        let config = BridgeLaunchConfig::resolve(&args, Path::new("."), |_| None).unwrap();

        let error = match create_transport(&config) {
            Ok(_) => panic!("an unknown transport must not fall back to a real one"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("carrier-pigeon"), "{error}");
        assert!(error.contains("websocket"), "{error}");
    }

    #[tokio::test]
    async fn launcher_receives_a_frame_and_shuts_down() {
        use coderex_bridge::transport::{Transport, TransportMessage};

        let tmp = tempfile::tempdir().unwrap();
        let mut args = args_with("http://cp.example.test", "env-test");
        args.transport = "in-process".into();
        args.repo_root = Some(tmp.path().to_path_buf());
        let config = BridgeLaunchConfig::resolve(&args, tmp.path(), |_| None).unwrap();

        let (bridge_transport, mut remote) =
            InProcessTransport::create_pair("bridge-env".into(), "gateway".into());
        remote.connect().await.unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let running = tokio::spawn(async move {
            run_with_transport(config, Box::new(bridge_transport), async move {
                let _ = shutdown_rx.await;
            })
            .await
        });

        remote
            .send(TransportMessage {
                r#type: "heartbeat".into(),
                correlation_id: Some("beat-1".into()),
                payload: "{}".into(),
                metadata: std::collections::HashMap::new(),
            })
            .await
            .unwrap();

        let reply = tokio::time::timeout(Duration::from_secs(1), remote.receive())
            .await
            .expect("the bridge should answer the heartbeat")
            .unwrap();
        assert_eq!(reply.r#type, "heartbeat_ack");
        assert_eq!(reply.correlation_id.as_deref(), Some("beat-1"));

        shutdown_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .expect("the bridge should shut down")
            .unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn launcher_reports_a_connect_failure() {
        use async_trait::async_trait;
        use coderex_bridge::transport::{TransportError, TransportMessage};

        #[derive(Debug)]
        struct RefusingTransport;

        #[async_trait]
        impl Transport for RefusingTransport {
            async fn connect(&mut self) -> Result<(), TransportError> {
                Err(TransportError::ConnectionLost("refused by test".into()))
            }

            async fn disconnect(&mut self) -> Result<(), TransportError> {
                Ok(())
            }

            fn is_connected(&self) -> bool {
                false
            }

            async fn send(&mut self, _: TransportMessage) -> Result<(), TransportError> {
                Err(TransportError::NotConnected)
            }

            async fn receive(&mut self) -> Result<TransportMessage, TransportError> {
                Err(TransportError::NotConnected)
            }

            fn connection_id(&self) -> &str {
                "refusing"
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let mut args = args_with("http://cp.example.test", "env-test");
        args.transport = "in-process".into();
        args.repo_root = Some(tmp.path().to_path_buf());
        let config = BridgeLaunchConfig::resolve(&args, tmp.path(), |_| None).unwrap();

        let error = run_with_transport(config, Box::new(RefusingTransport), async {})
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("could not connect"), "{error}");
        assert!(error.contains("refused by test"), "{error}");
    }
}
