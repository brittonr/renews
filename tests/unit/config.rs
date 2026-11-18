use renews::config::Config;

#[test]
fn retention_rules_match() {
    let toml = r#"addr = ":119"
[[group_settings]]
pattern = "foo.*"
retention_days = 1
[[group_settings]]
group = "foo.bar"
retention_days = 5
max_article_bytes = "20K"
[[group_settings]]
pattern = "*"
retention_days = 10
max_article_bytes = "1K"
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.retention_for_group("misc").unwrap().num_days(), 10);
    assert_eq!(cfg.retention_for_group("foo.test").unwrap().num_days(), 1);
    assert_eq!(cfg.retention_for_group("foo.bar").unwrap().num_days(), 5);
    assert_eq!(cfg.max_size_for_group("misc"), Some(1024));
    assert_eq!(cfg.max_size_for_group("foo.test"), Some(1024));
    assert_eq!(cfg.max_size_for_group("foo.bar"), Some(20480));
}

#[test]
fn runtime_update_preserves_immutable_fields() {
    let initial = r#"addr = ":119"
db_path = "/tmp/db1"
auth_db_path = "sqlite:///tmp/auth1"
peer_db_path = "/tmp/peer1"

idle_timeout_secs = 600
tls_addr = ":563"
tls_cert = "old.pem"
tls_key = "old.key"
[[group_settings]]
group = "misc.news"
retention_days = 5
"#;
    let mut cfg: Config = toml::from_str(initial).unwrap();

    let updated = r#"addr = ":42"
db_path = "/tmp/db2"
auth_db_path = "sqlite:///tmp/auth2"
peer_db_path = "/tmp/peer2"

idle_timeout_secs = 1200
tls_addr = ":9999"
tls_cert = "new.pem"
tls_key = "new.key"
[[group_settings]]
group = "misc.news"
retention_days = 1
"#;
    let new_cfg: Config = toml::from_str(updated).unwrap();
    cfg.update_runtime(new_cfg);

    assert_eq!(cfg.addr, ":119");
    assert_eq!(cfg.db_path, "/tmp/db1");
    assert_eq!(cfg.auth_db_path, "sqlite:///tmp/auth1");
    assert_eq!(cfg.peer_db_path, "/tmp/peer1");
    assert_eq!(cfg.idle_timeout_secs, 1200);
    assert_eq!(cfg.tls_addr.as_deref(), Some(":563"));
    assert_eq!(cfg.tls_cert.as_deref(), Some("new.pem"));
    assert_eq!(cfg.tls_key.as_deref(), Some("new.key"));

    assert_eq!(cfg.group_settings[0].retention_days, Some(1));
}

#[test]
fn default_paths() {
    let cfg: Config = toml::from_str("addr=\":119\"").unwrap();
    assert_eq!(cfg.db_path, "sqlite:///var/lib/renews/news.db");
    assert_eq!(cfg.auth_db_path, "sqlite:///var/lib/renews/auth.db");
    assert_eq!(cfg.peer_db_path, "sqlite:///var/lib/renews/peers.db");
    assert_eq!(cfg.idle_timeout_secs, 600);
}

#[test]
fn idle_timeout_configuration() {
    let toml = r#"addr = ":119"
idle_timeout_secs = 300
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.idle_timeout_secs, 300);
}

#[test]
fn idle_timeout_runtime_update() {
    let initial = r#"addr = ":119"
idle_timeout_secs = 600
"#;
    let mut cfg: Config = toml::from_str(initial).unwrap();

    let updated = r#"addr = ":42"
idle_timeout_secs = 1200
"#;
    let new_cfg: Config = toml::from_str(updated).unwrap();
    cfg.update_runtime(new_cfg);

    // Addr should be preserved (immutable)
    assert_eq!(cfg.addr, ":119");
    // Idle timeout should be updated (runtime-adjustable)
    assert_eq!(cfg.idle_timeout_secs, 1200);
}

#[test]
fn peer_connection_string_allows_credentials() {
    let toml = r#"addr = ":119"
[[peers]]
sitename = "u:p@news.example.com:563"
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.peers.len(), 1);
    assert_eq!(cfg.peers[0].sitename, "u:p@news.example.com:563");
}

#[test]
fn env_substitution() {
    use std::fs::write;
    use tempfile::tempdir;

    unsafe { std::env::set_var("TEST_ADDR", ":4242") };
    let dir = tempdir().unwrap();
    let cfg_path = dir.path().join("cfg.toml");
    write(&cfg_path, "addr = \"$ENV{TEST_ADDR}\"").unwrap();
    let cfg = Config::from_file(cfg_path.to_str().unwrap()).unwrap();
    assert_eq!(cfg.addr, ":4242");
}

#[test]
fn file_substitution() {
    use std::fs::{File, write};
    use std::io::Write as _;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let val_path = dir.path().join("val");
    write(&val_path, ":5050").unwrap();
    let cfg_path = dir.path().join("cfg.toml");
    let mut f = File::create(&cfg_path).unwrap();
    write!(f, "addr = \"$FILE{{{}}}\"", val_path.display()).unwrap();
    let cfg = Config::from_file(cfg_path.to_str().unwrap()).unwrap();
    assert_eq!(cfg.addr, ":5050");
}

#[test]
fn secret_substitution_uses_the_secretspec_provider() {
    use std::fs::write;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let secrets = dir.path().join("secrets.env");
    write(&secrets, "RENEWS_TEST_ADDR=':6060'\n").unwrap();
    let cfg_path = dir.path().join("cfg.toml");
    write(&cfg_path, "addr = \"$SECRET{RENEWS_TEST_ADDR}\"").unwrap();
    let missing_path = dir.path().join("missing.toml");
    write(&missing_path, "addr = \"$SECRET{RENEWS_TEST_ABSENT}\"").unwrap();

    unsafe {
        std::env::set_var(
            "SECRETSPEC_PROVIDER",
            format!("dotenv://{}", secrets.display()),
        );
    }
    let cfg = Config::from_file(cfg_path.to_str().unwrap());
    let missing = Config::from_file(missing_path.to_str().unwrap());
    unsafe { std::env::remove_var("SECRETSPEC_PROVIDER") };

    assert_eq!(cfg.unwrap().addr, ":6060");
    let err = missing
        .err()
        .expect("an unset $SECRET fails the load")
        .to_string();
    assert!(err.contains("RENEWS_TEST_ABSENT"), "{err}");
}

#[test]
fn peer_cron_schedule_configuration() {
    let cfg_str = r#"addr = ":119"
peer_sync_schedule = "0 0 * * * *"
[[peers]]
sitename = "peer1.example.com"
patterns = ["*"]
sync_schedule = "0 */30 * * * *"

[[peers]]
sitename = "peer2.example.com"
patterns = ["misc.*"]
# Uses default schedule
"#;
    let cfg: Config = toml::from_str(cfg_str).unwrap();
    assert_eq!(cfg.peer_sync_schedule, "0 0 * * * *");
    assert_eq!(cfg.peers.len(), 2);
    assert_eq!(cfg.peers[0].sitename, "peer1.example.com");
    assert_eq!(
        cfg.peers[0].sync_schedule,
        Some("0 */30 * * * *".to_string())
    );
    assert_eq!(cfg.peers[1].sitename, "peer2.example.com");
    assert_eq!(cfg.peers[1].sync_schedule, None);
}

#[test]
fn group_pattern_specificity_with_non_overlapping_settings() {
    let toml = r#"addr = ":119"
[[group_settings]]
pattern = "comp.*"
retention_days = 30

[[group_settings]]
pattern = "comp.lang.*"
max_article_bytes = "5M"

[[group_settings]]
group = "comp.lang.rust"
retention_days = 90
"#;
    let cfg: Config = toml::from_str(toml).unwrap();

    // Test that comp.misc gets only the broad pattern settings
    assert_eq!(cfg.retention_for_group("comp.misc").unwrap().num_days(), 30);
    assert_eq!(cfg.max_size_for_group("comp.misc"), None);

    // Test that comp.lang.python gets the more specific pattern settings for both
    // retention (from comp.*) and size (from comp.lang.*)
    assert_eq!(
        cfg.retention_for_group("comp.lang.python")
            .unwrap()
            .num_days(),
        30
    );
    assert_eq!(
        cfg.max_size_for_group("comp.lang.python"),
        Some(5 * 1024 * 1024)
    );

    // Test that comp.lang.rust gets the most specific exact match for retention
    // and inherits size from comp.lang.* pattern
    assert_eq!(
        cfg.retention_for_group("comp.lang.rust")
            .unwrap()
            .num_days(),
        90
    );
    assert_eq!(
        cfg.max_size_for_group("comp.lang.rust"),
        Some(5 * 1024 * 1024)
    );
}

#[test]
fn default_runtime_threads() {
    let cfg: Config = toml::from_str("addr=\":119\"").unwrap();
    assert_eq!(cfg.runtime_threads, 1);
}

#[test]
fn runtime_threads_configuration() {
    let toml = r#"addr = ":119"
runtime_threads = 4
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.runtime_threads, 4);
}

#[test]
fn runtime_threads_zero_means_all_cores() {
    let toml = r#"addr = ":119"
runtime_threads = 0
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.runtime_threads, 0);

    // Test that get_runtime_threads returns the number of cores
    let actual_threads = cfg.get_runtime_threads().unwrap();
    assert!(actual_threads > 0);
    // Should be equal to the number of cores on the system
    let expected_cores = std::thread::available_parallelism().unwrap().get();
    assert_eq!(actual_threads, expected_cores);
}

#[test]
fn runtime_threads_runtime_update() {
    let initial = r#"addr = ":119"
runtime_threads = 1
"#;
    let mut cfg: Config = toml::from_str(initial).unwrap();

    let updated = r#"addr = ":42"
runtime_threads = 8
"#;
    let new_cfg: Config = toml::from_str(updated).unwrap();
    cfg.update_runtime(new_cfg);

    // Addr should be preserved (immutable)
    assert_eq!(cfg.addr, ":119");
    // Runtime threads should be updated (runtime-adjustable)
    assert_eq!(cfg.runtime_threads, 8);
}
