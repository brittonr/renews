//! `iroh-news join` writes local files and a paste block. No server is dialed.

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
        .args(["join", "--name", "ada-grok", "--owner", "Ada"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("username: ada-grok\n"));
    assert!(stdout.contains("owner: Ada\n"));
    assert!(stdout.contains("server: slime.onix.computer\n"));
    assert!(stdout.contains(
        "services.renews-iroh.bots.\"ada-grok\".passwordFile = \"/run/secrets/slime/ada-grok\";"
    ));
    let id_line = stdout
        .lines()
        .find(|line| line.starts_with("endpoint_id: "))
        .unwrap();
    let endpoint_id = id_line.trim_start_matches("endpoint_id: ");
    assert_eq!(endpoint_id.len(), 64);
    assert!(stdout.contains(&format!(
        "services.renews-iroh.allowedPeers.\"ada-grok\" = \"{endpoint_id}\";"
    )));
    assert!(stdout.contains("Wait for Britton to confirm this account is on aspen1."));
    assert!(stdout.contains("iroh-news groups"));
    assert!(!stdout.contains("password ="));

    let config_path = home.path().join(".config/iroh-news/config.toml");
    let stub = std::fs::read_to_string(&config_path).unwrap();
    let password = home.path().join(".config/iroh-news/password");
    assert!(stub.contains("server = \"slime.onix.computer\"\n"));
    assert!(stub.contains("username = \"ada-grok\"\n"));
    assert!(stub.contains(&format!("password_file = \"{}\"\n", password.display())));
    assert!(!password.exists());
    assert!(
        home.path()
            .join(".local/state/iroh-news/secret_key")
            .is_file()
    );

    let mut id = bin();
    isolated(&mut id, home.path());
    let id_out = id.arg("id").output().unwrap();
    assert!(id_out.status.success());
    let identity: serde_json::Value = serde_json::from_slice(&id_out.stdout).unwrap();
    assert_eq!(identity["endpoint_id"], endpoint_id);

    std::fs::write(&config_path, "keep\n").unwrap();
    let mut again = bin();
    isolated(&mut again, home.path());
    let json = again
        .args(["join", "--name", "ada-grok", "--json"])
        .output()
        .unwrap();
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["config_written"], false);
    assert_eq!(value["username"], "ada-grok");
    assert_eq!(value["endpoint_id"], endpoint_id);
    assert!(
        value["nix_bot"]
            .as_str()
            .unwrap()
            .contains("/run/secrets/slime/ada-grok")
    );
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), "keep\n");
    assert!(!password.exists());
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
