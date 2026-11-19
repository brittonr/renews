//! Capability tokens are admission and a per-group policy, not an account.

mod utils;

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use rcan::Expires;
use renews::capabilities::{CapabilityStore, Grant, encode_hex, mint};
use renews::config::Config;
use renews::identity::UserEndpoint;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;

const GENERAL: &str = "bots.general";
const SECRET: &str = "bots.secret";

fn key(byte: u8) -> SigningKey {
    SigningKey::from_bytes(&[byte; 32])
}

#[tokio::test]
async fn a_capability_session_sees_only_its_patterns() {
    let storage = utils::create_test_storage().await;
    storage.add_group(GENERAL, false).await.unwrap();
    storage.add_group(SECRET, false).await.unwrap();
    for (group, id) in [
        (GENERAL, "<general@example.com>"),
        (SECRET, "<secret@example.com>"),
    ] {
        let text = format!(
            "From: bot@example.com\r\nSubject: {group}\r\nNewsgroups: {group}\r\n\
             Message-ID: {id}\r\nDate: Thu, 01 Jan 2026 00:00:00 +0000\r\n\r\nbody\r\n"
        );
        utils::store_test_article(storage.as_ref(), &text).await;
    }

    let issuer = key(7);
    let audience = renews::capabilities::endpoint_from_bytes(&[9; 32]).expect("endpoint");
    let token = mint(
        &issuer,
        audience,
        Grant {
            read: vec![GENERAL.into()],
            post: vec![GENERAL.into()],
        },
        Expires::Never,
    )
    .expect("mint");
    let store =
        CapabilityStore::from_config(&[encode_hex(issuer.verifying_key().as_bytes())], &[token])
            .expect("store");
    let capability = store.session_for(audience).expect("session for audience");

    let auth = utils::create_test_auth().await;
    let bindings: Arc<dyn UserEndpoint> = Arc::new(
        renews::admin::RuntimeStore::open("sqlite::memory:")
            .await
            .unwrap(),
    );
    let key_auth = renews::KeyAuth {
        endpoint_id: audience.to_string(),
        username: None,
        bindings,
        capability: Some(capability),
    };

    let (server, client) = tokio::io::duplex(1 << 16);
    let cfg: Arc<RwLock<Config>> = Arc::new(RwLock::new(toml::from_str("addr=\":119\"").unwrap()));
    let usage_tracker = utils::create_test_usage_tracker(auth.clone(), &*cfg.read().await);
    let handle = tokio::spawn(renews::handle_client_key(
        server,
        storage.clone(),
        auth,
        cfg,
        true,
        utils::create_test_queue(),
        usage_tracker,
        Some(key_auth),
    ));

    let mut client = BufReader::new(client);
    assert_eq!(code(&read_line(&mut client).await), "200");

    // Unpermitted group is indistinguishable from a missing one.
    client
        .get_mut()
        .write_all(b"GROUP bots.secret\r\n")
        .await
        .unwrap();
    assert_eq!(code(&read_line(&mut client).await), "411");

    client
        .get_mut()
        .write_all(b"GROUP bots.general\r\n")
        .await
        .unwrap();
    assert_eq!(code(&read_line(&mut client).await), "211");

    // LIST hides unreadable groups; the permitted one stays.
    client
        .get_mut()
        .write_all(b"LIST ACTIVE\r\n")
        .await
        .unwrap();
    assert_eq!(code(&read_line(&mut client).await), "215");
    let mut active = Vec::new();
    loop {
        let line = read_line(&mut client).await;
        if line == "." {
            break;
        }
        active.push(line);
    }
    assert!(active.iter().any(|line| line.starts_with(GENERAL)));
    assert!(!active.iter().any(|line| line.starts_with(SECRET)));

    // A message-id read outside the grant is a miss, not a body.
    client
        .get_mut()
        .write_all(b"ARTICLE <secret@example.com>\r\n")
        .await
        .unwrap();
    assert_eq!(code(&read_line(&mut client).await), "430");
    client
        .get_mut()
        .write_all(b"ARTICLE <general@example.com>\r\n")
        .await
        .unwrap();
    assert_eq!(code(&read_line(&mut client).await), "220");
    loop {
        if read_line(&mut client).await == "." {
            break;
        }
    }

    // Posting follows the grant, not the connection.
    client.get_mut().write_all(b"POST\r\n").await.unwrap();
    assert_eq!(code(&read_line(&mut client).await), "340");
    client
        .get_mut()
        .write_all(
            b"From: bot@example.com\r\nNewsgroups: bots.secret\r\nSubject: nope\r\n\r\nbody\r\n.\r\n",
        )
        .await
        .unwrap();
    assert_eq!(code(&read_line(&mut client).await), "441");

    client.get_mut().write_all(b"POST\r\n").await.unwrap();
    assert_eq!(code(&read_line(&mut client).await), "340");
    client
        .get_mut()
        .write_all(
            b"From: bot@example.com\r\nNewsgroups: bots.general\r\nSubject: allowed\r\n\r\nbody\r\n.\r\n",
        )
        .await
        .unwrap();
    assert_eq!(code(&read_line(&mut client).await), "240");

    drop(client);
    let _ = handle.await;
}

fn code(line: &str) -> &str {
    line.split(' ').next().unwrap_or("")
}

async fn read_line(client: &mut BufReader<tokio::io::DuplexStream>) -> String {
    let mut line = String::new();
    client.read_line(&mut line).await.unwrap();
    line.trim_end_matches(['\r', '\n']).to_string()
}
