//! Live notifications: an allowlisted watcher hears a post over gossip, ignores
//! a note that is not signed by the server, and a stranger cannot join.

use std::sync::Arc;
use std::time::{Duration, Instant};

use iroh::SecretKey;
use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::{RelayMode, presets};
use iroh::protocol::Router;
use iroh_gossip::Gossip;
use iroh_news::{DiscoveryMode, PostRequest, Session, Settings, WatchSession};
use renews::config::Config;
use renews::iroh::{IrohBind, IrohParts};
use renews::limits::UsageTracker;
use renews::queue::{ArticleQueue, WorkerPool};
use tokio::sync::RwLock;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_hears_a_post_ignores_a_spoof_and_rejects_a_stranger() {
    let dir = tempfile::tempdir().unwrap();
    let storage = renews::storage::open("sqlite::memory:").await.unwrap();
    let auth = renews::auth::open("sqlite::memory:").await.unwrap();
    storage.add_group("bots.general", false).await.unwrap();
    auth.add_user("bot", "secret").await.unwrap();

    let poster = SecretKey::generate();
    let watcher_key = SecretKey::generate();
    let spoofer = SecretKey::generate();
    let stranger = SecretKey::generate();
    let allowed = format!(
        r#"["{}", "{}", "{}"]"#,
        poster.public(),
        watcher_key.public(),
        spoofer.public()
    );
    let cfg: Config = toml::from_str(&format!(
        r#"
        addr = "127.0.0.1:119"
        site_name = "bots.local"
        db_path = "sqlite::memory:"
        auth_db_path = "sqlite::memory:"
        [iroh]
        enabled = true
        allowed_peers = {allowed}
        [iroh.gossip]
        enable = true
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
    .expect("listener");

    let watch_settings = |key: SecretKey, name: &str| Settings {
        server: server.ticket.clone(),
        username: "bot".into(),
        password: "secret".into(),
        from: "bot@bots.local".into(),
        secret_key: key,
        relays: false,
        state_dir: dir.path().join(name),
        discovery: DiscoveryMode::Auto,
    };

    let stranger_join = WatchSession::open(
        watch_settings(stranger, "stranger"),
        &["bots.general".into()],
        Duration::from_secs(2),
    )
    .await;
    if let Ok(_joined) = stranger_join {
        panic!("unapproved peer joined gossip");
    }

    let mut watch = tokio::time::timeout(
        Duration::from_secs(8),
        WatchSession::open(
            watch_settings(watcher_key, "watcher"),
            &["bots.general".into()],
            Duration::from_secs(5),
        ),
    )
    .await
    .expect("watcher join timed out")
    .expect("watcher join");

    let spoof_endpoint = iroh::Endpoint::builder(presets::N0)
        .secret_key(spoofer)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    let lookup = MemoryLookup::new();
    lookup.add_endpoint_info(server.addr());
    spoof_endpoint.address_lookup().unwrap().add(lookup);
    let spoof_gossip = Gossip::builder().spawn(spoof_endpoint.clone());
    let _spoof_router = Router::builder(spoof_endpoint)
        .accept(iroh_gossip::ALPN, spoof_gossip.clone())
        .spawn();
    let topic_id = renews::gossip::topic_id(&server.endpoint_id, "bots.general");
    let mut spoof_topic = spoof_gossip
        .subscribe(topic_id, vec![server.endpoint_id])
        .await
        .expect("spoofer subscribe");
    tokio::time::timeout(Duration::from_secs(5), spoof_topic.joined())
        .await
        .expect("spoofer did not join")
        .expect("spoofer join error");
    let fake = br#"{"group":"bots.general","number":99,"message_id":"<spoof@x>","subject":"spoofed","from":"x","date":"x","sig":"00"}"#;
    spoof_topic
        .broadcast(bytes::Bytes::copy_from_slice(fake))
        .await
        .expect("spoof broadcast");

    let ignored = tokio::time::timeout(Duration::from_millis(800), watch.recv()).await;
    if let Ok(note) = ignored {
        panic!(
            "spoofed notification was accepted: {}",
            note.unwrap().subject
        );
    }

    let mut poster = Session::connect(watch_settings(poster, "poster"))
        .await
        .expect("poster connect");
    let started = Instant::now();
    poster
        .post(PostRequest {
            group: "bots.general",
            subject: "live",
            references: None,
            from: "bot@bots.local",
            body: "via gossip\n",
        })
        .await
        .expect("post");
    let note = tokio::time::timeout(Duration::from_secs(2), watch.recv())
        .await
        .expect("notification slower than 2s")
        .expect("watch ended");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "notification took {elapsed:?}"
    );
    assert_eq!(note.subject, "live");
    assert_eq!(note.group, "bots.general");
    assert!(note.number >= 1);
    eprintln!("gossip latency ms: {}", elapsed.as_millis());

    poster.close().await.unwrap();
    watch.close().await.unwrap();
    server.shutdown().await.unwrap();
}
