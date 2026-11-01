//! In-process renews iroh listener, dialed with the same client the CLI uses.

use std::sync::Arc;
use std::time::Duration;

use iroh::SecretKey;
use iroh::endpoint::RelayMode;
use iroh_news::{ALPN, DiscoveryMode, PostRequest, ReadRequest, Session, Settings};
use renews::config::Config;
use renews::iroh::{ALPN as SERVER_ALPN, IrohBind, IrohParts};
use renews::limits::UsageTracker;
use renews::queue::{ArticleQueue, WorkerPool};
use tokio::sync::RwLock;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allowed_bot_posts_and_reads_stranger_is_rejected() {
    assert_eq!(ALPN, SERVER_ALPN);

    let dir = tempfile::tempdir().unwrap();
    // Shared in-memory databases. A file URI failed inside the Nix build
    // sandbox (`no such table: groups`) even though migrations reported success.
    let storage = renews::storage::open("sqlite::memory:").await.unwrap();
    let auth = renews::auth::open("sqlite::memory:").await.unwrap();
    storage.add_group("bots.general", false).await.unwrap();
    auth.add_user("bot", "secret").await.unwrap();

    let client_key = SecretKey::generate();
    let client_id = client_key.public().to_string();
    let cfg: Config = toml::from_str(&format!(
        r#"
        addr = "127.0.0.1:119"
        site_name = "bots.local"
        db_path = "sqlite::memory:"
        auth_db_path = "sqlite::memory:"
        [iroh]
        enabled = true
        allowed_peers = ["{client_id}"]
        "#
    ))
    .unwrap();
    let cfg = Arc::new(RwLock::new(cfg));
    let queue = ArticleQueue::new(16);
    let usage = Arc::new(UsageTracker::new(
        auth.clone(),
        cfg.read().await.user_limits.clone(),
    ));
    let workers = WorkerPool::new(queue.clone(), storage.clone(), auth.clone(), cfg.clone(), 2);
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

    let state_dir = dir.path().join("bot-state");
    let settings = Settings {
        server: server.ticket.clone(),
        username: "bot".into(),
        password: "secret".into(),
        from: "bot@bots.local".into(),
        secret_key: client_key,
        relays: false,
        state_dir: state_dir.clone(),
        discovery: DiscoveryMode::Auto,
    };

    let mut session = tokio::time::timeout(Duration::from_secs(30), Session::connect(settings))
        .await
        .expect("allowed bot timed out waiting for the NNTP greeting")
        .expect("allowed bot should connect");

    session
        .post(PostRequest {
            group: "bots.general",
            subject: "hello",
            references: None,
            from: "bot@bots.local",
            body: "hi from iroh\n",
        })
        .await
        .expect("post");

    let mut articles = Vec::new();
    for _ in 0..50 {
        articles = session
            .read(ReadRequest {
                group: "bots.general".into(),
                since: None,
                limit: Some(10),
                update_cursor: false,
            })
            .await
            .expect("read");
        if !articles.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(articles.len(), 1, "posted article was not stored");
    assert_eq!(articles[0].subject, "hello");
    assert_eq!(articles[0].from, "bot@bots.local");
    assert!(articles[0].body.contains("hi from iroh"));
    assert!(articles[0].message_id.contains("@bots.local"));
    session.close().await.expect("quit");

    let stranger = Settings {
        server: server.ticket.clone(),
        username: "bot".into(),
        password: "secret".into(),
        from: "bot@bots.local".into(),
        secret_key: SecretKey::generate(),
        relays: false,
        state_dir: dir.path().join("stranger-state"),
        discovery: DiscoveryMode::Auto,
    };
    let rejected = tokio::time::timeout(Duration::from_secs(20), Session::connect(stranger)).await;
    match rejected {
        Ok(Err(_)) => {}
        Ok(Ok(_)) => panic!("peer absent from the allowlist was accepted"),
        Err(_) => panic!("peer absent from the allowlist hung instead of being rejected"),
    }

    server.shutdown().await.expect("shutdown");
}
