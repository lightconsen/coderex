//! Two machines sharing a session through the rendezvous service.
//!
//! The local `TeleportManager` shares into a directory, which only works on one
//! machine. This runs the real service on a loopback port and drives it with
//! the client the CLI uses, so the whole path — upload, token, join, list,
//! revoke — is exercised across two managers that share nothing but the URL.

use std::path::PathBuf;

use coderex_bridge::teleport::{TeleportError, TeleportManager, TeleportRendezvous};
use coderex_core::state::SessionState;
use coderex_core::types::Message;
use coderex_server::teleport::TeleportService;

fn session() -> SessionState {
    let mut state = SessionState::new(PathBuf::from("/work/shared"));
    state.title = Some("a shared session".to_string());
    state.messages.push(Message::user("what changed?"));
    state.messages.push(Message::assistant("two files"));
    state
}

/// Start the service on a free loopback port and return its base URL.
async fn start_service() -> String {
    let service = TeleportService::new(0);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("local address");
    tokio::spawn(async move {
        let _ = service.run_on(listener).await;
    });
    format!("http://{address}")
}

#[tokio::test]
async fn a_session_shared_on_one_machine_joins_on_another() {
    let base = start_service().await;
    let share_side = TeleportManager::new().with_rendezvous(TeleportRendezvous::new(&base));
    let join_side = TeleportManager::new().with_rendezvous(TeleportRendezvous::new(&base));

    let token = share_side.share(&session()).await.expect("share");
    assert!(!token.is_empty());

    let joined = join_side.join(&token).await.expect("join");
    assert_eq!(joined.messages.len(), 2);
    assert_eq!(joined.title.as_deref(), Some("a shared session"));
    assert_eq!(joined.cwd, PathBuf::from("/work/shared"));
}

#[tokio::test]
async fn the_service_lists_a_share_with_its_session_and_revocation_ends_it() {
    let base = start_service().await;
    let manager = TeleportManager::new().with_rendezvous(TeleportRendezvous::new(&base));

    let token = manager.share(&session()).await.expect("share");
    let shares = manager.list().await.expect("list");
    assert_eq!(shares.len(), 1);
    assert_eq!(shares[0].token, token);
    assert!(shares[0].session_id.is_some(), "{shares:?}");
    assert!(shares[0].created_at.is_some(), "{shares:?}");

    assert!(manager.revoke(&token).await.expect("revoke"));
    assert!(!manager.revoke(&token).await.expect("second revoke"));
    assert!(manager.list().await.expect("list").is_empty());
    assert!(matches!(
        manager.join(&token).await,
        Err(TeleportError::SessionNotFound(_))
    ));
}

#[tokio::test]
async fn an_unknown_token_is_not_found_rather_than_an_error() {
    let base = start_service().await;
    let manager = TeleportManager::new().with_rendezvous(TeleportRendezvous::new(&base));
    assert!(matches!(
        manager.join("0123456789abcdef0123456789abcdef").await,
        Err(TeleportError::SessionNotFound(_))
    ));
}

#[tokio::test]
async fn an_unreachable_service_is_reported_as_a_service_error() {
    let manager =
        TeleportManager::new().with_rendezvous(TeleportRendezvous::new("http://127.0.0.1:9"));
    assert!(matches!(
        manager.share(&session()).await,
        Err(TeleportError::Service(_))
    ));
}
