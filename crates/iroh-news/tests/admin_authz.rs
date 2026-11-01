//! Admin ALPN is not the bot allowlist. A runtime peer can connect without a restart.

use std::sync::Arc;
use std::time::Duration;

use iroh::SecretKey;
use iroh::endpoint::RelayMode;
use iroh_news::{ADMIN_ALPN, Session, Settings, admin_call};
use renews::admin::ADMIN_ALPN as SERVER_ADMIN_ALPN;
use renews::config::Config;
use renews::iroh::{IrohBind, IrohParts};
use renews::limits::UsageTracker;
use renews::queue::{ArticleQueue, WorkerPool};
use serde_json::json;
use tokio::sync::RwLock;

fn settings(
    server: &str,
    key: SecretKey,
    dir: &std::path::Path,
    user: &str,
    password: &str,
) -> Settings {
    Settings {
        server: server.to_string(),
        username: user.into(),
        password: password.into(),
        from: format!("{user}@bots.local"),
        secret_key: key,
        relays: false,
        state_dir: dir.to_path_buf(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_rejects_bots_and_runtime_peer_connects() {
    assert_eq!(ADMIN_ALPN, SERVER_ADMIN_ALPN);

    let dir = tempfile::tempdir().unwrap();
    let storage = renews::storage::open("sqlite::memory:").await.unwrap();
    let auth = renews::auth::open("sqlite::memory:").await.unwrap();
    storage.add_group("bots.general", false).await.unwrap();
    auth.add_user("bot", "secret").await.unwrap();

    let bot_key = SecretKey::generate();
    let admin_key = SecretKey::generate();
    let bot_id = bot_key.public().to_string();
    let admin_id = admin_key.public().to_string();
    let cfg: Config = toml::from_str(&format!(
        r#"
        addr = "127.0.0.1:119"
        site_name = "bots.local"
        db_path = "sqlite::memory:"
        auth_db_path = "sqlite::memory:"
        [iroh]
        enabled = true
        allowed_peers = ["{bot_id}"]
        admin_peers = ["{admin_id}"]
        "#
    ))
    .unwrap();
    let cfg = Arc::new(RwLock::new(cfg));
    let queue = ArticleQueue::new(16);
    let usage = Arc::new(UsageTracker::new(
        auth.clone(),
        cfg.read().await.user_limits.clone(),
    ));
    let workers = WorkerPool::new(queue.clone(), storage.clone(), auth.clone(), cfg.clone(), 1);
    let _workers = workers.start().await;

    let server = renews::iroh::serve(
        IrohParts {
            storage,
            auth,
            config: cfg,
            queue,
            usage_tracker: usage,
        },
        IrohBind {
            secret_key: SecretKey::generate(),
            relay_mode: RelayMode::Disabled,
            wait_for_relay: false,
            bind_addr: Some("127.0.0.1:0".parse().unwrap()),
            ticket_path: None,
            runtime_db: format!("sqlite://{}", dir.path().join("runtime.db").display()),
        },
    )
    .await
    .expect("iroh listener");

    let ticket = server.ticket.clone();
    let state = dir.path();

    let bot_admin = admin_call(&ticket, state, bot_key.clone(), false, "status", json!({}))
        .await
        .expect_err("bot allowlist must not open the admin channel");
    let bot_admin = format!("{bot_admin:#}");
    assert!(bot_admin.contains("not allowed"), "{bot_admin}");

    let stranger = admin_call(
        &ticket,
        state,
        SecretKey::generate(),
        false,
        "status",
        json!({}),
    )
    .await
    .expect_err("stranger");
    let stranger = format!("{stranger:#}");
    assert!(stranger.contains("not allowed"), "{stranger}");

    let status = admin_call(
        &ticket,
        state,
        admin_key.clone(),
        false,
        "status",
        json!({}),
    )
    .await
    .expect("admin status");
    assert!(status["endpoint_id"].is_string());
    assert!(status["ticket"].as_str().unwrap().starts_with("endpoint"));

    let sticky = admin_call(
        &ticket,
        state,
        admin_key.clone(),
        false,
        "remove_peer",
        json!({"endpoint_id": bot_id, "confirm": true}),
    )
    .await
    .expect_err("nix peer");
    assert!(
        sticky.to_string().contains("cannot be removed at runtime"),
        "{sticky}"
    );

    let newbie = SecretKey::generate();
    let newbie_id = newbie.public().to_string();
    let approved = admin_call(
        &ticket,
        state,
        admin_key.clone(),
        false,
        "approve_join",
        json!({"block": {"username": "newbie", "endpoint_id": newbie_id, "owner": "Ada"}}),
    )
    .await
    .expect("approve_join");
    let password = approved["password"].as_str().unwrap().to_string();

    let mut session = tokio::time::timeout(
        Duration::from_secs(20),
        Session::connect(settings(
            &ticket,
            newbie,
            &state.join("newbie"),
            "newbie",
            &password,
        )),
    )
    .await
    .expect("approved peer timed out")
    .expect("approved peer should reach NNTP");
    let groups = session.groups().await.expect("groups");
    assert!(groups.iter().any(|group| group.name == "bots.general"));
    session.close().await.unwrap();

    let extra = SecretKey::generate();
    let extra_id = extra.public().to_string();
    admin_call(
        &ticket,
        state,
        admin_key.clone(),
        false,
        "add_peer",
        json!({"endpoint_id": extra_id, "label": "extra"}),
    )
    .await
    .unwrap();
    let greeted = Session::connect(settings(
        &ticket,
        extra.clone(),
        &state.join("extra"),
        "bot",
        "wrong",
    ))
    .await;
    match greeted {
        Err(err) => {
            let err = format!("{err:#}");
            assert!(
                !err.contains("not allowed"),
                "runtime peer was rejected before NNTP: {err}"
            );
        }
        Ok(session) => {
            let _ = session.close().await;
        }
    }

    admin_call(
        &ticket,
        state,
        admin_key,
        false,
        "remove_peer",
        json!({"endpoint_id": extra_id, "confirm": true}),
    )
    .await
    .unwrap();
    let removed = Session::connect(settings(
        &ticket,
        extra,
        &state.join("extra2"),
        "bot",
        "secret",
    ))
    .await;
    match removed {
        Err(err) => {
            let err = format!("{err:#}");
            assert!(err.contains("not allowed"), "{err}");
        }
        Ok(_) => panic!("removed runtime peer was accepted"),
    }

    server.shutdown().await.expect("shutdown");
}
