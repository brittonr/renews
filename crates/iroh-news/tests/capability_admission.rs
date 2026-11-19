//! A capability token admits its endpoint without allowed_peers.

use std::sync::Arc;
use std::time::Duration;

use iroh::SecretKey;
use iroh::endpoint::RelayMode;
use iroh_news::{ALPN, DiscoveryMode, PostRequest, ReadRequest, Session, Settings};
use renews::capabilities::{Grant, encode_hex, expires_never, mint, signing_key_from_hex};
use renews::config::Config;
use renews::iroh::{ALPN as SERVER_ALPN, IrohBind, IrohParts};
use renews::limits::UsageTracker;
use renews::queue::{ArticleQueue, WorkerPool};
use tokio::sync::RwLock;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_admits_its_endpoint_and_only_its_groups() {
    assert_eq!(ALPN, SERVER_ALPN);

    let dir = tempfile::tempdir().unwrap();
    let storage = renews::storage::open("sqlite::memory:").await.unwrap();
    let auth = renews::auth::open("sqlite::memory:").await.unwrap();
    storage.add_group("bots.general", false).await.unwrap();
    storage.add_group("bots.secret", false).await.unwrap();
    // A secret article exists, so an empty read proves filtering, not a miss.
    let (_, secret_article) = renews::parse_message(
        "From: bot@bots.local\r\nSubject: hidden\r\nNewsgroups: bots.secret\r\n\
         Message-ID: <hidden@bots.local>\r\nDate: Thu, 01 Jan 2026 00:00:00 +0000\r\n\r\nsecret\r\n",
    )
    .unwrap();
    storage.store_article(&secret_article).await.unwrap();

    let client_key = SecretKey::generate();
    let issuer = signing_key_from_hex(&encode_hex(&[7u8; 32])).unwrap();
    let token = mint(
        &issuer,
        client_key.public(),
        Grant {
            read: vec!["bots.general".into()],
            post: vec!["bots.general".into()],
        },
        expires_never(),
    )
    .unwrap();

    // Empty allowed_peers: only the token may admit this endpoint.
    let cfg: Config = toml::from_str(
        r#"
        addr = "127.0.0.1:119"
        site_name = "bots.local"
        db_path = "sqlite::memory:"
        auth_db_path = "sqlite::memory:"
        [iroh]
        enabled = true
        "#,
    )
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
            capability_issuers: vec![encode_hex(issuer.verifying_key().as_bytes())],
            capability_tokens: vec![token],
        },
    )
    .await
    .expect("iroh listener");

    let settings = Settings {
        server: server.ticket.clone(),
        username: "bot".into(),
        // An empty password skips AUTHINFO, so the token is the only identity.
        password: String::new(),
        from: "bot@bots.local".into(),
        secret_key: client_key,
        relays: false,
        state_dir: dir.path().join("bot-state"),
        discovery: DiscoveryMode::Auto,
        server_id: None,
    };
    let mut session = tokio::time::timeout(Duration::from_secs(30), Session::connect(settings))
        .await
        .expect("token holder timed out waiting for the NNTP greeting")
        .expect("token holder should connect");

    session
        .post(PostRequest {
            group: "bots.general",
            subject: "hello",
            references: None,
            from: "bot@bots.local",
            message_id: None,
            body: "hi from a capability\n",
        })
        .await
        .expect("post inside the grant");

    let denied = session
        .read(ReadRequest {
            group: "bots.secret".into(),
            since: None,
            limit: Some(10),
            update_cursor: false,
        })
        .await;
    session.close().await.ok();
    match denied {
        Ok(articles) => assert!(
            articles.is_empty(),
            "read outside the grant leaked articles"
        ),
        Err(_) => {}
    }

    let stranger = Settings {
        server: server.ticket.clone(),
        username: "bot".into(),
        password: String::new(),
        from: "bot@bots.local".into(),
        secret_key: SecretKey::generate(),
        relays: false,
        state_dir: dir.path().join("stranger-state"),
        discovery: DiscoveryMode::Auto,
        server_id: None,
    };
    match tokio::time::timeout(Duration::from_secs(20), Session::connect(stranger)).await {
        Ok(Err(_)) => {}
        Ok(Ok(_)) => panic!("peer without a token was accepted"),
        Err(_) => panic!("peer without a token hung instead of being rejected"),
    }

    server.shutdown().await.expect("shutdown");
}
