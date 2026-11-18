//! Where iroh-news keeps its endpoint key and passwords.
//!
//! Explicit sources win: a `secret_key` file already in the state directory,
//! `IROH_NEWS_PASSWORD`, `IROH_NEWS_PASSWORD_FILE`, `password_file`,
//! `password`, and `secrets_dir`. Otherwise the values live in SecretSpec when
//! a provider is configured: the config's `secretspec` setting,
//! `SECRETSPEC_PROVIDER`, or the user-global default written by
//! `secretspec config global init`. With no provider, iroh-news keeps using
//! files in the state directory.
//!
//! Each server identity should use its own SecretSpec project
//! (`secretspec_project`), so two networks never share a key or password.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use iroh::SecretKey;
use secretspec::{
    Generation, NamedResolution, ResolvedSecret, ResolvedSource, Secret, SecretSpecError, Secrets,
    Spec,
};
use serde::Deserialize;
use serde_json::Value;

use crate::admin_client::stash_password_file;

/// SecretSpec name of the 32-byte endpoint key, stored as 64 hex digits.
pub const KEY_SECRET: &str = "IROH_NEWS_SECRET_KEY";
/// SecretSpec name of the NNTP password.
pub const PASSWORD_SECRET: &str = "IROH_NEWS_PASSWORD";
/// SecretSpec project used when the config names none.
pub const DEFAULT_PROJECT: &str = "iroh-news";
/// Name of the key file in the state directory.
pub const KEY_FILE: &str = "secret_key";

const PROFILE: &str = "default";
const ACCOUNT_PREFIX: &str = "IROH_NEWS_ACCOUNT_";
const KEY_BYTES: usize = 32;

/// The config's `secretspec` value.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum SecretSpecSetting {
    /// `true` requires a configured provider. `false` keeps everything in
    /// files.
    Enabled(bool),
    /// A provider name or URI, such as `keyring://` or `onepassword://Bots`.
    Provider(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    /// `secretspec = false`.
    Files,
    /// No setting: SecretSpec when `SECRETSPEC_PROVIDER` or SecretSpec's
    /// global config names a provider, files otherwise.
    Auto { provider_found: bool },
    /// `secretspec = true` or a provider: SecretSpec, and an error without one.
    Required { provider: Option<String> },
}

/// Where the endpoint key came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// An existing `secret_key` file in the state directory.
    StateFile,
    /// A new `secret_key` file, because no SecretSpec provider is configured.
    NewStateFile,
    /// SecretSpec. `generated` is true when this call created and stored it.
    SecretSpec {
        provider: Option<String>,
        generated: bool,
    },
}

impl KeySource {
    /// Short label for `status` output.
    pub fn label(&self) -> &'static str {
        match self {
            Self::StateFile | Self::NewStateFile => "state_file",
            Self::SecretSpec { .. } => "secretspec",
        }
    }
}

/// Selects and talks to the store for one config.
#[derive(Clone, Debug)]
pub struct CredentialStore {
    mode: Mode,
    project: String,
}

impl CredentialStore {
    /// Build the store from the config's `secretspec` and `secretspec_project`.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty provider or an invalid project name.
    pub fn new(setting: Option<&SecretSpecSetting>, project: Option<&str>) -> anyhow::Result<Self> {
        let mode = match setting {
            None => Mode::Auto {
                provider_found: ambient_provider_configured(),
            },
            Some(SecretSpecSetting::Enabled(false)) => Mode::Files,
            Some(SecretSpecSetting::Enabled(true)) => Mode::Required { provider: None },
            Some(SecretSpecSetting::Provider(provider)) => {
                let provider = provider.trim();
                if provider.is_empty() {
                    bail!("secretspec must be true, false, or a provider name or URI");
                }
                Mode::Required {
                    provider: Some(provider.to_string()),
                }
            }
        };
        let project = project.unwrap_or(DEFAULT_PROJECT);
        validate_project(project)?;
        Ok(Self {
            mode,
            project: project.to_string(),
        })
    }

    /// SecretSpec project this store reads and writes.
    pub fn project(&self) -> &str {
        &self.project
    }

    /// Provider named by the config, if any. Otherwise SecretSpec picks
    /// `SECRETSPEC_PROVIDER` or the global default.
    pub fn configured_provider(&self) -> Option<&str> {
        match &self.mode {
            Mode::Required { provider } => provider.as_deref(),
            Mode::Files | Mode::Auto { .. } => None,
        }
    }

    /// `files`, `auto`, or `secretspec`, for `status` output.
    pub fn mode_label(&self) -> &'static str {
        match self.mode {
            Mode::Files => "files",
            Mode::Auto { .. } => "auto",
            Mode::Required { .. } => "secretspec",
        }
    }

    /// Whether every value stays in files and SecretSpec is never loaded.
    fn files_only(&self) -> bool {
        matches!(
            self.mode,
            Mode::Files
                | Mode::Auto {
                    provider_found: false
                }
        )
    }

    /// Load the endpoint key.
    ///
    /// An existing `secret_key` file in `state_dir` wins. Otherwise SecretSpec
    /// returns the key, creating and storing one on first use. Without a
    /// provider, a new `secret_key` file is created.
    ///
    /// # Errors
    ///
    /// Returns an error if the key file is unreadable or malformed, if
    /// SecretSpec fails, or if the provider cannot keep a generated key.
    pub fn secret_key(&self, state_dir: &Path) -> anyhow::Result<(SecretKey, KeySource)> {
        let path = state_dir.join(KEY_FILE);
        if let Some(key) = read_key_file(&path)? {
            return Ok((key, KeySource::StateFile));
        }
        if self.files_only() {
            return Ok((
                crate::load_or_create_secret_key(state_dir)?,
                KeySource::NewStateFile,
            ));
        }
        let secrets = self.session(
            &[(KEY_SECRET, key_declaration())],
            "iroh-news: load the endpoint key",
        )?;
        let resolved = self
            .resolved(secrets.resolve_named(KEY_SECRET), KEY_SECRET)?
            .with_context(|| {
                format!(
                    "SecretSpec returned no value for {KEY_SECRET} in project {}",
                    self.project
                )
            })?;
        let value = inline_value(&resolved, KEY_SECRET)?;
        let key = parse_key_hex(value)?;
        let generated = resolved.source == ResolvedSource::Generated;
        if generated {
            self.confirm_stored_key(value)?;
        }
        Ok((
            key,
            KeySource::SecretSpec {
                provider: resolved.source_provider.clone(),
                generated,
            },
        ))
    }

    /// The NNTP password from SecretSpec, for callers with no explicit source.
    /// `None` without a provider in automatic mode.
    ///
    /// # Errors
    ///
    /// Returns an error if SecretSpec fails.
    pub fn password(&self) -> anyhow::Result<Option<String>> {
        if self.files_only() {
            return Ok(None);
        }
        let secrets = self.session(
            &[(PASSWORD_SECRET, password_declaration())],
            "iroh-news: authenticate to the news server",
        )?;
        Ok(self
            .resolved(secrets.resolve_named(PASSWORD_SECRET), PASSWORD_SECRET)?
            .and_then(|resolved| resolved.value)
            .filter(|password| !password.is_empty()))
    }

    /// Store the NNTP password, prompting on a terminal or reading stdin.
    ///
    /// # Errors
    ///
    /// Returns an error when SecretSpec is off, unconfigured, or cannot write.
    pub fn prompt_and_set_password(&self) -> anyhow::Result<()> {
        let secrets = self.writable_session(
            &[(PASSWORD_SECRET, password_declaration())],
            "iroh-news: store the NNTP password",
        )?;
        secrets
            .prompt_and_set(PASSWORD_SECRET)
            .map_err(|err| self.write_error(err, PASSWORD_SECRET))
    }

    /// Move the state directory's `secret_key` file into SecretSpec, check
    /// that the stored copy reads back, and delete the file.
    ///
    /// # Errors
    ///
    /// Returns an error when there is no key file, when SecretSpec already
    /// holds a different key, or when the provider cannot store the key.
    pub fn import_key(&self, state_dir: &Path) -> anyhow::Result<(SecretKey, PathBuf)> {
        let path = state_dir.join(KEY_FILE);
        let key = read_key_file(&path)?
            .with_context(|| format!("{} does not exist; nothing to import", path.display()))?;
        let hex = hex_encode(&key.to_bytes());
        let secrets = self.writable_session(
            &[(
                KEY_SECRET,
                Secret::optional("iroh-news endpoint secret key, 64 hex digits"),
            )],
            "iroh-news: import the endpoint key",
        )?;
        match self.resolved(secrets.resolve_named(KEY_SECRET), KEY_SECRET)? {
            Some(resolved) => {
                if inline_value(&resolved, KEY_SECRET)? != hex {
                    bail!(
                        "SecretSpec project {} already holds a different {KEY_SECRET}; \
                         delete it there first or keep using {}",
                        self.project,
                        path.display()
                    );
                }
            }
            None => secrets
                .set_text(KEY_SECRET, &hex)
                .map_err(|err| self.write_error(err, KEY_SECRET))?,
        }
        self.confirm_stored_key(&hex)?;
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        Ok((key, path))
    }

    /// Move a generated `password` out of an admin result.
    ///
    /// An explicit `secrets_dir`, a `secretspec = false` config, or an
    /// unconfigured provider writes `<secrets dir>/<username>.password` and
    /// returns `password_file`. Otherwise the password is stored in SecretSpec
    /// as `IROH_NEWS_ACCOUNT_<USERNAME>` and the result names that secret.
    ///
    /// # Errors
    ///
    /// Returns an error if the username is unusable or the store fails.
    pub fn stash_account_password(
        &self,
        result: &mut Value,
        explicit_dir: Option<&Path>,
        default_dir: &Path,
    ) -> anyhow::Result<()> {
        let Some(password) = result
            .get("password")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return Ok(());
        };
        if let Some(dir) = explicit_dir {
            stash_password_file(result, dir)?;
            return Ok(());
        }
        if self.files_only() {
            stash_password_file(result, default_dir)?;
            return Ok(());
        }
        let username = result
            .get("username")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = account_secret_name(&username)?;
        let secrets = self.session(
            &[(name.as_str(), account_declaration(&username))],
            "iroh-news: store a generated account password",
        )?;
        secrets
            .set_text(&name, &password)
            .map_err(|err| self.write_error(err, &name))?;
        if let Some(object) = result.as_object_mut() {
            object.remove("password");
            object.insert("password_secret".into(), Value::String(name));
            object.insert(
                "secretspec_project".into(),
                Value::String(self.project.clone()),
            );
        }
        Ok(())
    }

    /// An account password that [`Self::stash_account_password`] stored.
    ///
    /// # Errors
    ///
    /// Returns an error if SecretSpec is off or unconfigured, or holds no
    /// password for that account.
    pub fn account_password(&self, username: &str) -> anyhow::Result<String> {
        let name = account_secret_name(username)?;
        let secrets = self.writable_session(
            &[(name.as_str(), account_declaration(username))],
            "iroh-news: deliver a generated account password",
        )?;
        self.resolved(secrets.resolve_named(&name), &name)?
            .and_then(|resolved| resolved.value)
            .with_context(|| format!("SecretSpec project {} has no {name}", self.project))
    }

    fn session(&self, declarations: &[(&str, Secret)], reason: &str) -> anyhow::Result<Secrets> {
        let mut builder = Spec::builder(self.project.as_str());
        for (name, secret) in declarations {
            builder = builder.secret(*name, secret.clone());
        }
        let spec = builder
            .build()
            .context("declaring the iroh-news secrets for SecretSpec")?;
        let mut secrets = Secrets::from_spec(spec)
            .context("loading SecretSpec")?
            .with_reason(reason);
        secrets.set_profile(PROFILE);
        secrets.set_ignore_ambient_scope(true);
        if let Some(provider) = self.configured_provider() {
            secrets.set_provider(provider);
        }
        Ok(secrets)
    }

    /// A session for commands whose whole point is SecretSpec.
    fn writable_session(
        &self,
        declarations: &[(&str, Secret)],
        reason: &str,
    ) -> anyhow::Result<Secrets> {
        match self.mode {
            Mode::Files => bail!("the config sets secretspec = false; remove it to use SecretSpec"),
            Mode::Auto {
                provider_found: false,
            } => bail!(no_provider_message()),
            Mode::Auto { .. } | Mode::Required { .. } => self.session(declarations, reason),
        }
    }

    /// Unwrap a named resolution. `None` means declared but unset.
    fn resolved(
        &self,
        result: secretspec::Result<NamedResolution>,
        name: &str,
    ) -> anyhow::Result<Option<ResolvedSecret>> {
        match result {
            Ok(NamedResolution::Resolved(resolved)) => Ok(Some(resolved)),
            Ok(NamedResolution::Missing { .. }) => Ok(None),
            Ok(NamedResolution::Undeclared) => {
                bail!("{name} is not declared in the iroh-news SecretSpec project")
            }
            Err(SecretSpecError::NoProviderConfigured) => bail!(no_provider_message()),
            Err(err) => Err(anyhow::Error::new(err).context(format!(
                "reading {name} from SecretSpec project {}",
                self.project
            ))),
        }
    }

    fn write_error(&self, err: SecretSpecError, name: &str) -> anyhow::Error {
        match err {
            SecretSpecError::NoProviderConfigured => anyhow::anyhow!(no_provider_message()),
            err => anyhow::Error::new(err).context(format!(
                "storing {name} in SecretSpec project {}",
                self.project
            )),
        }
    }

    /// A provider that cannot store values hands out a fresh generated key on
    /// every resolution. Read the key again and refuse anything but the
    /// stored copy, so the endpoint id never changes behind the operator.
    fn confirm_stored_key(&self, expected: &str) -> anyhow::Result<()> {
        let secrets = self.session(
            &[(
                KEY_SECRET,
                Secret::optional("iroh-news endpoint secret key, 64 hex digits"),
            )],
            "iroh-news: confirm the stored endpoint key",
        )?;
        match self.resolved(secrets.resolve_named(KEY_SECRET), KEY_SECRET)? {
            Some(resolved)
                if resolved.source == ResolvedSource::Provider
                    && resolved.value.as_deref() == Some(expected) =>
            {
                Ok(())
            }
            _ => bail!(
                "the SecretSpec provider did not keep {KEY_SECRET}; choose a provider that \
                 stores values, or set secretspec = false to keep the key in the state directory"
            ),
        }
    }
}

fn key_declaration() -> Secret {
    Secret::required("iroh-news endpoint secret key, 64 hex digits").generate(Generation::Hex {
        bytes: Some(KEY_BYTES),
    })
}

fn password_declaration() -> Secret {
    Secret::optional("iroh-news NNTP password")
}

fn account_declaration(username: &str) -> Secret {
    Secret::optional(format!("generated NNTP password for account {username}"))
}

fn no_provider_message() -> String {
    "no SecretSpec provider is configured; run `secretspec config global init`, set \
     SECRETSPEC_PROVIDER, or set secretspec = \"<provider>\" in the iroh-news config"
        .to_string()
}

/// Whether SecretSpec finds a provider without one from the iroh-news config:
/// a non-blank `SECRETSPEC_PROVIDER`, or `[defaults] provider` in SecretSpec's
/// global config. Mirrors SecretSpec's own lookup, so a machine without
/// SecretSpec never loads it; a failed lookup prints a warning on every call.
/// A config file that cannot be read or parsed counts as configured, so
/// SecretSpec reports the problem.
fn ambient_provider_configured() -> bool {
    if std::env::var("SECRETSPEC_PROVIDER").is_ok_and(|value| !value.trim().is_empty()) {
        return true;
    }
    global_config_paths()
        .iter()
        .any(|path| match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<toml::Table>(&text) {
                Ok(config) => config
                    .get("defaults")
                    .and_then(|defaults| defaults.get("provider"))
                    .and_then(toml::Value::as_str)
                    .is_some_and(|provider| !provider.trim().is_empty()),
                Err(_) => true,
            },
            Err(err) => err.kind() != std::io::ErrorKind::NotFound,
        })
}

/// Where SecretSpec reads its global config (`GlobalConfig::path`), plus the
/// pre-XDG macOS location it still migrates from.
fn global_config_paths() -> Vec<PathBuf> {
    use etcetera::app_strategy::{AppStrategy, AppStrategyArgs, choose_app_strategy};

    let mut paths = Vec::new();
    if let Ok(strategy) = choose_app_strategy(AppStrategyArgs {
        top_level_domain: String::new(),
        author: String::new(),
        app_name: "secretspec".into(),
    }) {
        paths.push(strategy.config_dir().join("config.toml"));
    }
    #[cfg(target_os = "macos")]
    if let Ok(home) = etcetera::home_dir() {
        paths.push(home.join("Library/Application Support/secretspec/config.toml"));
    }
    paths
}

fn inline_value<'a>(resolved: &'a ResolvedSecret, name: &str) -> anyhow::Result<&'a str> {
    resolved
        .value
        .as_deref()
        .with_context(|| format!("SecretSpec returned {name} as a file path, not a value"))
}

/// `IROH_NEWS_ACCOUNT_<USERNAME>` with hyphens as underscores. Usernames are
/// `[a-z0-9-]`, so the mapping cannot collide.
///
/// # Errors
///
/// Returns an error for a username outside `[a-z0-9-]`.
pub fn account_secret_name(username: &str) -> anyhow::Result<String> {
    crate::validate_username(username)
        .with_context(|| format!("refusing to store a password for username {username:?}"))?;
    Ok(format!(
        "{ACCOUNT_PREFIX}{}",
        username.to_ascii_uppercase().replace('-', "_")
    ))
}

fn validate_project(project: &str) -> anyhow::Result<()> {
    let ok = !project.is_empty()
        && project.len() <= 64
        && project
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && project.as_bytes()[0].is_ascii_alphanumeric();
    if !ok {
        bail!("secretspec_project must match [a-z0-9][a-z0-9_-]* (at most 64 characters)");
    }
    Ok(())
}

/// Read `state_dir/secret_key`. A dangling link is an error rather than a
/// reason to create a new key behind it.
fn read_key_file(path: &Path) -> anyhow::Result<Option<SecretKey>> {
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("inspecting {}", path.display())),
        Ok(_) => {}
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let array: [u8; KEY_BYTES] = bytes.as_slice().try_into().map_err(|_| {
        anyhow::anyhow!(
            "{} is {} bytes; expected {KEY_BYTES}",
            path.display(),
            bytes.len()
        )
    })?;
    Ok(Some(SecretKey::from_bytes(&array)))
}

fn parse_key_hex(value: &str) -> anyhow::Result<SecretKey> {
    let value = value.trim();
    if value.len() != KEY_BYTES * 2 {
        bail!("{KEY_SECRET} must be {} hex digits", KEY_BYTES * 2);
    }
    let mut bytes = [0u8; KEY_BYTES];
    for (index, chunk) in value.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).context("key is not ASCII")?;
        bytes[index] = u8::from_str_radix(pair, 16)
            .with_context(|| format!("{KEY_SECRET} must be {} hex digits", KEY_BYTES * 2))?;
    }
    Ok(SecretKey::from_bytes(&bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn env_file(&self) -> PathBuf {
            self.dir.path().join("secrets.env")
        }

        fn state_dir(&self) -> PathBuf {
            self.dir.path().join("state")
        }

        fn store(&self) -> CredentialStore {
            let provider = format!("dotenv://{}", self.env_file().display());
            CredentialStore::new(
                Some(&SecretSpecSetting::Provider(provider)),
                Some("iroh-news-test"),
            )
            .unwrap()
        }

        fn stored(&self) -> String {
            std::fs::read_to_string(self.env_file()).unwrap_or_default()
        }
    }

    #[test]
    fn generated_key_is_stored_and_stable_without_a_state_file() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let (first, source) = store.secret_key(&fixture.state_dir()).unwrap();
        assert!(matches!(
            source,
            KeySource::SecretSpec {
                generated: true,
                ..
            }
        ));
        let hex = hex_encode(&first.to_bytes());
        assert!(fixture.stored().contains(&hex), "dotenv file holds the key");
        assert!(!fixture.state_dir().join(KEY_FILE).exists());

        let (second, source) = store.secret_key(&fixture.state_dir()).unwrap();
        assert_eq!(second.public(), first.public());
        assert!(matches!(
            source,
            KeySource::SecretSpec {
                generated: false,
                ..
            }
        ));
    }

    #[test]
    fn existing_state_file_wins_over_secretspec() {
        let fixture = Fixture::new();
        let file_key = crate::load_or_create_secret_key(&fixture.state_dir()).unwrap();
        let (key, source) = fixture.store().secret_key(&fixture.state_dir()).unwrap();
        assert_eq!(source, KeySource::StateFile);
        assert_eq!(key.public(), file_key.public());
        assert!(!fixture.stored().contains(KEY_SECRET));
    }

    #[test]
    fn disabled_store_creates_a_state_file() {
        let fixture = Fixture::new();
        let store = CredentialStore::new(Some(&SecretSpecSetting::Enabled(false)), None).unwrap();
        let (key, source) = store.secret_key(&fixture.state_dir()).unwrap();
        assert_eq!(source, KeySource::NewStateFile);
        let again = crate::load_or_create_secret_key(&fixture.state_dir()).unwrap();
        assert_eq!(again.public(), key.public());
        assert_eq!(store.password().unwrap(), None);
    }

    #[test]
    fn dangling_key_link_is_an_error_not_a_new_key() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.state_dir()).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                fixture.dir.path().join("missing"),
                fixture.state_dir().join(KEY_FILE),
            )
            .unwrap();
            assert!(fixture.store().secret_key(&fixture.state_dir()).is_err());
            assert!(!fixture.stored().contains(KEY_SECRET));
        }
    }

    #[test]
    fn password_reads_what_was_stored() {
        let fixture = Fixture::new();
        let store = fixture.store();
        assert_eq!(store.password().unwrap(), None);
        std::fs::write(fixture.env_file(), "IROH_NEWS_PASSWORD=hunter2\n").unwrap();
        assert_eq!(store.password().unwrap().as_deref(), Some("hunter2"));
    }

    #[test]
    fn import_moves_the_key_file_and_refuses_a_different_key() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let file_key = crate::load_or_create_secret_key(&fixture.state_dir()).unwrap();
        let (imported, removed) = store.import_key(&fixture.state_dir()).unwrap();
        assert_eq!(imported.public(), file_key.public());
        assert!(!removed.exists());
        let (key, source) = store.secret_key(&fixture.state_dir()).unwrap();
        assert_eq!(key.public(), file_key.public());
        assert!(matches!(
            source,
            KeySource::SecretSpec {
                generated: false,
                ..
            }
        ));

        let other = crate::load_or_create_secret_key(&fixture.state_dir()).unwrap();
        assert_ne!(other.public(), file_key.public());
        let err = store.import_key(&fixture.state_dir()).unwrap_err();
        assert!(err.to_string().contains("different"), "{err:#}");
        assert!(fixture.state_dir().join(KEY_FILE).exists());
    }

    #[test]
    fn account_passwords_go_to_secretspec_and_never_stay_in_the_result() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let mut result = json!({"username": "ada-grok", "password": "generated-pw"});
        store
            .stash_account_password(&mut result, None, &fixture.dir.path().join("secrets"))
            .unwrap();
        assert_eq!(result["password_secret"], "IROH_NEWS_ACCOUNT_ADA_GROK");
        assert_eq!(result["secretspec_project"], "iroh-news-test");
        assert!(!result.to_string().contains("generated-pw"));
        assert!(result.get("password_file").is_none());
        assert_eq!(store.account_password("ada-grok").unwrap(), "generated-pw");
    }

    #[test]
    fn explicit_secrets_dir_keeps_account_passwords_in_files() {
        let fixture = Fixture::new();
        let dir = fixture.dir.path().join("explicit");
        let mut result = json!({"username": "ada-grok", "password": "generated-pw"});
        fixture
            .store()
            .stash_account_password(&mut result, Some(&dir), &fixture.dir.path().join("unused"))
            .unwrap();
        let path = result["password_file"].as_str().unwrap().to_string();
        assert!(path.starts_with(&dir.display().to_string()));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "generated-pw\n");
        assert!(!fixture.stored().contains("generated-pw"));
    }

    #[test]
    fn names_are_checked() {
        assert_eq!(
            account_secret_name("ada-grok-2").unwrap(),
            "IROH_NEWS_ACCOUNT_ADA_GROK_2"
        );
        assert!(account_secret_name("Ada").is_err());
        assert!(account_secret_name("../x").is_err());
        assert!(CredentialStore::new(None, Some("Swarm News")).is_err());
        assert!(CredentialStore::new(None, Some("swarm-news")).is_ok());
        assert!(
            CredentialStore::new(Some(&SecretSpecSetting::Provider("  ".into())), None).is_err()
        );
        assert!(parse_key_hex("zz").is_err());
        let key = SecretKey::generate();
        let parsed = parse_key_hex(&hex_encode(&key.to_bytes())).unwrap();
        assert_eq!(parsed.public(), key.public());
    }
}
