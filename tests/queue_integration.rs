//! Test for full queue integration with server

mod utils;

use renews::{
    auth::{AuthProvider, sqlite::SqliteAuth},
    config::Config,
    limits::UsageTracker,
    queue::{ArticleQueue, WorkerPool},
    storage::{Storage, sqlite::SqliteStorage},
};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::RwLock;

async fn setup_queue_enabled_server() -> (std::net::SocketAddr, Arc<dyn Storage>) {
    // Create storage and auth
    let storage = Arc::new(SqliteStorage::new("sqlite::memory:").await.unwrap());
    let auth = Arc::new(SqliteAuth::new("sqlite::memory:").await.unwrap());

    // Add test user and group
    auth.add_user("testuser", "password").await.unwrap();
    storage.add_group("test.group", false).await.unwrap();

    // Create config with queue settings
    let config = Config {
        addr: "127.0.0.1:0".to_string(),
        site_name: "test".to_string(),
        db_path: "sqlite::memory:".to_string(),
        auth_db_path: "sqlite::memory:".to_string(),
        peer_db_path: "sqlite::memory:".to_string(),
        peer_sync_schedule: "0 0 * * * *".to_string(),
        idle_timeout_secs: 600,
        peers: vec![],
        tls_addr: Some("127.0.0.1:0".to_string()),
        tls_cert: None,
        tls_key: None,
        ws_addr: None,
        article_queue_capacity: 100,
        article_worker_count: 2,
        runtime_threads: 1,
        group_settings: vec![],
        filters: vec![],
        pgp_key_servers: renews::config::default_pgp_key_servers(),
        allow_auth_insecure_connections: false,
        allow_anonymous_posting: false,
        logging: Default::default(),
        user_limits: Default::default(),
        iroh: Default::default(),
    };

    // Since we can't easily test with TLS in this setup, we'll create a simplified server
    // that demonstrates the queue functionality
    let queue = ArticleQueue::new(100);
    let config_arc = Arc::new(RwLock::new(config));

    // Create worker pool and start workers
    let worker_pool = WorkerPool::new(
        queue.clone(),
        storage.clone(),
        auth.clone(),
        config_arc.clone(),
        2,
    );

    let _worker_handles = worker_pool.start().await;

    // Create a simple TCP server that uses our queue
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let storage_clone = storage.clone();
    let auth_clone = auth.clone();
    let config_clone = config_arc.clone();
    let queue_clone = queue;
    let usage_tracker = Arc::new(UsageTracker::new(auth.clone(), Default::default()));

    tokio::spawn(async move {
        if let Ok((socket, _)) = listener.accept().await {
            let _ = renews::handle_client(
                socket,
                storage_clone,
                auth_clone,
                config_clone,
                true, // TLS mode for posting
                queue_clone,
                usage_tracker,
            )
            .await;
        }
    });

    (addr, storage)
}

#[tokio::test]
async fn test_full_queue_integration() {
    let (addr, storage) = setup_queue_enabled_server().await;

    // Connect to server
    let stream = TcpStream::connect(addr).await.unwrap();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    // Read greeting
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("201")); // No posting until authenticated

    // Authenticate
    writer
        .write_all(b"AUTHINFO USER testuser\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("381")); // More auth info required

    writer
        .write_all(b"AUTHINFO PASS password\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("281")); // Authentication accepted

    // Post an article
    writer.write_all(b"POST\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("340")); // Send article

    // Send article
    let article = "From: test@example.com\r\nSubject: Test Article\r\nNewsgroups: test.group\r\nMessage-ID: <queue-test@example.com>\r\n\r\nThis is a test article submitted via queue.\r\n.\r\n";
    writer.write_all(article.as_bytes()).await.unwrap();

    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("240")); // Article received (should be immediate due to queue)

    // Wait for queue processing
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Verify article was stored by worker
    let stored = storage
        .get_article_by_id("<queue-test@example.com>")
        .await
        .unwrap();
    assert!(stored.is_some());
    let article = stored.unwrap();
    assert!(
        article
            .body
            .contains("This is a test article submitted via queue")
    );

    // Quit
    writer.write_all(b"QUIT\r\n").await.unwrap();
}

#[tokio::test]
async fn test_queue_validation_failure() {
    let (addr, _storage) = setup_queue_enabled_server().await;

    // Connect to server
    let stream = TcpStream::connect(addr).await.unwrap();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    // Read greeting and authenticate
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();

    writer
        .write_all(b"AUTHINFO USER testuser\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();

    writer
        .write_all(b"AUTHINFO PASS password\r\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();

    // Try to post an invalid article (missing required headers)
    writer.write_all(b"POST\r\n").await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("340")); // Send article

    // Send invalid article (missing From header)
    let article = "Subject: Test Article\r\nNewsgroups: test.group\r\n\r\nThis should fail validation.\r\n.\r\n";
    writer.write_all(article.as_bytes()).await.unwrap();

    line.clear();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("441")); // Posting failed due to basic validation

    writer.write_all(b"QUIT\r\n").await.unwrap();
}
