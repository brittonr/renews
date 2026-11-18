//! With no `secretspec` setting, a provider from `SECRETSPEC_PROVIDER` or
//! SecretSpec's global config holds the endpoint key; without one, the key
//! stays a state file and SecretSpec is never loaded.

use std::path::Path;
use std::process::Command;

/// Run iroh-news in `home`; returns stdout as JSON and stderr.
fn run(home: &Path, provider: Option<&str>, args: &[&str]) -> (serde_json::Value, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_iroh-news"));
    cmd.env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("IROH_NEWS_CONFIG")
        .env_remove("IROH_NEWS_STATE")
        .env_remove("IROH_NEWS_SERVER")
        .env_remove("IROH_NEWS_PASSWORD")
        .env_remove("IROH_NEWS_PASSWORD_FILE")
        .env_remove("SECRETSPEC_PROVIDER")
        .env_remove("SECRETSPEC_PROFILE");
    if let Some(provider) = provider {
        cmd.env("SECRETSPEC_PROVIDER", provider);
    }
    let out = cmd.args(args).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "iroh-news {args:?}: {stderr}");
    (serde_json::from_slice(&out.stdout).unwrap(), stderr)
}

fn key_file(home: &Path) -> std::path::PathBuf {
    home.join(".local/state/iroh-news/secret_key")
}

#[test]
fn env_provider_holds_the_key_and_status_says_so() {
    let home = tempfile::tempdir().unwrap();
    let secrets = home.path().join("secrets.env");
    let provider = format!("dotenv://{}", secrets.display());

    let (first, _) = run(home.path(), Some(&provider), &["id"]);
    let (again, _) = run(home.path(), Some(&provider), &["id"]);
    assert_eq!(first["endpoint_id"], again["endpoint_id"]);
    assert!(
        !key_file(home.path()).exists(),
        "no key file alongside SecretSpec"
    );
    assert!(
        std::fs::read_to_string(&secrets)
            .unwrap()
            .contains("IROH_NEWS_SECRET_KEY=")
    );

    let (status, _) = run(home.path(), Some(&provider), &["status"]);
    assert_eq!(status["endpoint_id"], first["endpoint_id"]);
    assert_eq!(status["credentials"]["mode"], "auto");
    assert_eq!(status["credentials"]["key"], "secretspec");
    assert_eq!(status["password_configured"], false);
}

#[test]
fn global_default_provider_holds_the_key() {
    let home = tempfile::tempdir().unwrap();
    let secrets = home.path().join("secrets.env");
    let config_dir = home.path().join(".config/secretspec");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[defaults]\nprovider = \"dotenv://{}\"\n",
            secrets.display()
        ),
    )
    .unwrap();

    let (status, _) = run(home.path(), None, &["status"]);
    assert_eq!(status["credentials"]["key"], "secretspec");
    assert!(!key_file(home.path()).exists());
    assert!(
        std::fs::read_to_string(&secrets)
            .unwrap()
            .contains("IROH_NEWS_SECRET_KEY=")
    );
}

#[test]
fn without_a_provider_the_key_is_a_quiet_state_file() {
    let home = tempfile::tempdir().unwrap();
    let (first, stderr) = run(home.path(), None, &["id"]);
    assert!(key_file(home.path()).is_file());
    let (status, status_stderr) = run(home.path(), None, &["status"]);
    assert_eq!(status["endpoint_id"], first["endpoint_id"]);
    assert_eq!(status["credentials"]["key"], "state_file");
    for stderr in [stderr, status_stderr] {
        assert!(
            !stderr.contains("provider"),
            "SecretSpec was consulted: {stderr}"
        );
    }
}
