//! Integration tests for the telemetry fan-out: one upload reaches the
//! first-party endpoint, the OTLP logs channel and the Datadog logs channel,
//! each in its own wire format.

use coderex_telemetry::{ChannelConfig, TelemetryConfig, TelemetryUploader};
use coderex_telemetry::{TelemetryEvent, TelemetryStore};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn mount_ok(server: &MockServer, route: &str) {
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
}

#[tokio::test]
async fn one_batch_reaches_all_three_channels_in_their_own_formats() {
    let server = MockServer::start().await;
    mount_ok(&server, "/events").await;
    mount_ok(&server, "/v1/logs").await;
    Mock::given(method("POST"))
        .and(path("/api/v2/logs"))
        .and(header("DD-API-KEY", "dd-test-key"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("telemetry.db");
    let store = TelemetryStore::open(&db).unwrap();
    store
        .insert(&TelemetryEvent::SessionStart {
            session_id: "integration".into(),
            timestamp: chrono::Utc::now(),
        })
        .unwrap();
    drop(store);

    let uploader = TelemetryUploader::new(
        db.clone(),
        TelemetryConfig {
            endpoint: format!("{}/events", server.uri()),
            enabled: true,
            ..Default::default()
        },
    )
    .with_channels(ChannelConfig {
        otlp_endpoint: Some(server.uri()),
        datadog_api_key: Some("dd-test-key".into()),
        datadog_endpoint: Some(format!("{}/api/v2/logs", server.uri())),
        ..Default::default()
    });

    uploader
        .upload_pending()
        .await
        .expect("all channels accept");

    let requests = server.received_requests().await.unwrap();
    let mut by_path = std::collections::HashMap::new();
    for request in &requests {
        by_path.insert(
            request.url.path().to_string(),
            serde_json::from_slice::<serde_json::Value>(&request.body).expect("json body"),
        );
    }
    assert_eq!(by_path.len(), 3, "three destinations: {:?}", by_path.keys());

    let first_party = &by_path["/events"];
    assert!(
        first_party
            .get("events")
            .is_some_and(serde_json::Value::is_array),
        "{first_party}"
    );

    let otlp = &by_path["/v1/logs"];
    assert!(
        otlp.get("resourceLogs")
            .is_some_and(serde_json::Value::is_array),
        "{otlp}"
    );

    let datadog = &by_path["/api/v2/logs"];
    let entries = datadog.as_array().expect("Datadog takes an array");
    assert_eq!(entries.len(), 1, "{datadog}");
    assert_eq!(entries[0]["ddsource"], "coderex");
    assert_eq!(entries[0]["service"], "coderex");
    assert!(entries[0]["message"].is_string(), "{datadog}");

    let store = TelemetryStore::open(&db).unwrap();
    assert_eq!(
        store.pending_count().unwrap(),
        0,
        "a delivered batch is removed from the local queue"
    );
}

#[tokio::test]
async fn a_failing_channel_is_best_effort() {
    let server = MockServer::start().await;
    mount_ok(&server, "/events").await;
    Mock::given(method("POST"))
        .and(path("/v1/logs"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("telemetry.db");
    let store = TelemetryStore::open(&db).unwrap();
    store
        .insert(&TelemetryEvent::SessionStart {
            session_id: "failure".into(),
            timestamp: chrono::Utc::now(),
        })
        .unwrap();
    drop(store);

    let uploader = TelemetryUploader::new(
        db,
        TelemetryConfig {
            endpoint: format!("{}/events", server.uri()),
            enabled: true,
            ..Default::default()
        },
    )
    .with_channels(ChannelConfig {
        otlp_endpoint: Some(server.uri()),
        ..Default::default()
    });

    // The first-party upload already succeeded, so a channel outage is logged
    // rather than retried into a duplicate first-party upload.
    uploader
        .upload_pending()
        .await
        .expect("a channel failure is best effort");

    let paths: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| request.url.path().to_string())
        .collect();
    assert!(paths.iter().any(|path| path == "/events"), "{paths:?}");
    assert!(paths.iter().any(|path| path == "/v1/logs"), "{paths:?}");
}

#[tokio::test]
async fn a_disabled_channel_config_sends_no_extra_requests() {
    let server = MockServer::start().await;
    mount_ok(&server, "/events").await;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("telemetry.db");
    let store = TelemetryStore::open(&db).unwrap();
    store
        .insert(&TelemetryEvent::SessionStart {
            session_id: "no-channels".into(),
            timestamp: chrono::Utc::now(),
        })
        .unwrap();
    drop(store);

    let uploader = TelemetryUploader::new(
        db,
        TelemetryConfig {
            endpoint: format!("{}/events", server.uri()),
            enabled: true,
            ..Default::default()
        },
    )
    .with_channels(ChannelConfig {
        enabled: false,
        otlp_endpoint: Some(server.uri()),
        datadog_api_key: Some("key".into()),
        datadog_endpoint: Some(format!("{}/api/v2/logs", server.uri())),
        ..Default::default()
    });

    uploader.upload_pending().await.expect("first-party upload");

    let paths: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| request.url.path().to_string())
        .collect();
    assert_eq!(paths, vec!["/events"], "{paths:?}");
}
