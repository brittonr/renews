//! `iroh-news join` writes a key-auth config. A dead server still leaves that file.

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_iroh-news"))
}

fn isolated(cmd: &mut Command, home: &std::path::Path) {
    cmd.env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("IROH_NEWS_CONFIG")
        .env_remove("IROH_NEWS_STATE")
        .env_remove("IROH_NEWS_SERVER")
        .env_remove("IROH_NEWS_USERNAME")
        .env_remove("IROH_NEWS_PASSWORD")
        .env_remove("IROH_NEWS_PASSWORD_FILE");
}

#[test]
fn join_output_and_stub() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = bin();
    isolated(&mut cmd, home.path());
    let out = cmd
        .args([
            "--no-relays",
            "--server",
            "abababababababababababababababababababababababababababababababab",
            "join",
            "--name",
            "ada-grok",
            "--owner",
            "Ada",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a closed port should not approve a join"
    );
    let config_path = home.path().join(".config/iroh-news/config.toml");
    let stub = std::fs::read_to_string(&config_path).unwrap();
    let password = home.path().join(".config/iroh-news/password");
    assert!(stub.contains("username = \"ada-grok\"\n"));
    assert!(stub.starts_with("server = \"abababab"));
    assert!(stub.contains("username = \"ada-grok\"\n"));
    assert!(!stub.contains("password_file"));
    assert!(!password.exists());
    assert!(
        home.path()
            .join(".local/state/iroh-news/secret_key")
            .is_file()
    );
}

#[test]
fn join_rejects_unsafe_username() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = bin();
    isolated(&mut cmd, home.path());
    let out = cmd.args(["join", "--name", "Ada Grok"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("[a-z0-9-]"));
    assert!(!home.path().join(".config/iroh-news/config.toml").exists());
}
