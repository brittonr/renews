//! `$SECRET{NAME}` configuration placeholders.
//!
//! Each name is declared as a required secret in the SecretSpec project
//! `RENEWS_SECRETSPEC_PROJECT` (default `renews`), profile `default`, and
//! resolved through the provider in `SECRETSPEC_PROVIDER` or the user-global
//! default. Under systemd, `SECRETSPEC_PROVIDER=systemd-credential://` reads
//! `$CREDENTIALS_DIRECTORY/NAME`. A config without the placeholder never
//! loads SecretSpec.

use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow, bail};
use regex::{Captures, Regex};
use secretspec::{NamedResolution, Secret, SecretSpecError, Secrets, Spec};

/// Environment variable naming the SecretSpec project.
pub const PROJECT_ENV: &str = "RENEWS_SECRETSPEC_PROJECT";
/// Project used when [`PROJECT_ENV`] is unset.
pub const DEFAULT_PROJECT: &str = "renews";

/// Replace every `$SECRET{NAME}` in `text`. `provider` overrides the
/// SecretSpec provider; `None` lets SecretSpec choose.
///
/// Values are inserted as-is and are not scanned for further placeholders.
pub(crate) fn expand(text: &str, provider: Option<&str>) -> Result<String> {
    let pattern = Regex::new(r"\$SECRET\{([^}]*)\}")?;
    let mut names: Vec<String> = Vec::new();
    for caps in pattern.captures_iter(text) {
        let name = &caps[1];
        if !valid_name(name) {
            bail!("invalid placeholder $SECRET{{{name}}}: names match [A-Za-z_][A-Za-z0-9_]*");
        }
        if !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    if names.is_empty() {
        return Ok(text.to_string());
    }

    let project = std::env::var(PROJECT_ENV).unwrap_or_else(|_| DEFAULT_PROJECT.to_string());
    let mut builder = Spec::builder(project.as_str());
    for name in &names {
        builder = builder.secret(
            name.as_str(),
            Secret::required(format!("renews configuration placeholder {name}")),
        );
    }
    let spec = builder.build().with_context(|| {
        format!("declaring $SECRET placeholders in SecretSpec project {project}")
    })?;
    let mut secrets = Secrets::from_spec(spec)
        .context("loading SecretSpec")?
        .with_reason("renews: load the server configuration");
    secrets.set_profile("default");
    secrets.set_ignore_ambient_scope(true);
    if let Some(provider) = provider {
        secrets.set_provider(provider);
    }

    let mut values = HashMap::with_capacity(names.len());
    for name in names {
        let value = match secrets.resolve_named(&name) {
            Ok(NamedResolution::Resolved(resolved)) => resolved
                .value
                .ok_or_else(|| anyhow!("$SECRET{{{name}}} resolved to a file path, not a value"))?,
            Ok(NamedResolution::Missing { .. }) => {
                bail!("$SECRET{{{name}}} has no value in SecretSpec project {project}")
            }
            Ok(NamedResolution::Undeclared) => {
                bail!("$SECRET{{{name}}} is not declared in SecretSpec project {project}")
            }
            Err(SecretSpecError::NoProviderConfigured) => bail!(
                "$SECRET{{{name}}} needs a SecretSpec provider: set SECRETSPEC_PROVIDER \
                 or run `secretspec config global init`"
            ),
            Err(err) => {
                return Err(anyhow::Error::new(err).context(format!("resolving $SECRET{{{name}}}")));
            }
        };
        values.insert(name, value);
    }
    Ok(pattern
        .replace_all(text, |caps: &Captures<'_>| values[&caps[1]].clone())
        .into_owned())
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(dir: &tempfile::TempDir, contents: &str) -> String {
        let path = dir.path().join("secrets.env");
        std::fs::write(&path, contents).unwrap();
        format!("dotenv://{}", path.display())
    }

    #[test]
    fn resolves_each_placeholder_once_and_inserts_values_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let provider = provider(&dir, "DB_PASSWORD='pa$1ss$ENV{X}'\nPEER=peer:pw\n");
        let text = "db = \"postgres://u:$SECRET{DB_PASSWORD}@h/db\"\n\
                    again = \"$SECRET{DB_PASSWORD}\"\n\
                    peer = \"$SECRET{PEER}@news\"\n";
        let out = expand(text, Some(&provider)).unwrap();
        assert_eq!(
            out,
            "db = \"postgres://u:pa$1ss$ENV{X}@h/db\"\nagain = \"pa$1ss$ENV{X}\"\npeer = \"peer:pw@news\"\n"
        );
    }

    #[test]
    fn text_without_placeholders_is_untouched() {
        let text = "addr = \"$ENV{ADDR}\"\n";
        assert_eq!(
            expand(text, Some("dotenv:///nonexistent/.env")).unwrap(),
            text
        );
    }

    #[test]
    fn missing_and_malformed_secrets_fail_with_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let provider = provider(&dir, "PRESENT=yes\n");
        let err = expand("x = \"$SECRET{ABSENT}\"", Some(&provider)).unwrap_err();
        assert!(err.to_string().contains("ABSENT"), "{err:#}");
        let err = expand("x = \"$SECRET{not-a-name}\"", Some(&provider)).unwrap_err();
        assert!(err.to_string().contains("not-a-name"), "{err:#}");
    }

    #[test]
    fn names_follow_environment_variable_rules() {
        assert!(valid_name("RENEWS_DB_PASSWORD"));
        assert!(valid_name("_x1"));
        assert!(!valid_name(""));
        assert!(!valid_name("1X"));
        assert!(!valid_name("A-B"));
    }
}
