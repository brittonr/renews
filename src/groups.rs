//! Rules for bot-created newsgroups.

use anyhow::bail;

/// Limits that apply when an account creates a group.
#[derive(Clone, Copy)]
pub struct GroupCreatePolicy<'a> {
    pub enabled: bool,
    pub prefixes: &'a [String],
    pub group_count: u64,
    pub creates_today: u64,
    pub max_groups: u32,
    pub creates_per_day: u32,
    pub is_admin: bool,
}

/// Whether this account may create `name` right now.
///
/// # Errors
///
/// Returns an error describing the policy that failed.
pub fn check_new_group(
    policy: &GroupCreatePolicy<'_>,
    name: &str,
    description: &str,
) -> anyhow::Result<()> {
    validate_group_name(name)?;
    if description.is_empty()
        || description.len() > 200
        || description.chars().any(|c| c.is_control() || c == '\t')
    {
        bail!("description must be one line of 1-200 characters");
    }
    if !policy.is_admin {
        if !policy.enabled {
            bail!("group creation is disabled");
        }
        if !policy
            .prefixes
            .iter()
            .any(|prefix| name.starts_with(prefix.as_str()))
        {
            bail!(
                "group name must start with one of: {}",
                policy.prefixes.join(", ")
            );
        }
        if policy.creates_per_day > 0 && policy.creates_today >= u64::from(policy.creates_per_day) {
            bail!("group creation rate limit");
        }
        if policy.max_groups > 0 && policy.group_count >= u64::from(policy.max_groups) {
            bail!("group limit reached");
        }
    }
    Ok(())
}

/// Lowercase newsgroup name: `[a-z0-9.-]`, no empty labels.
///
/// # Errors
///
/// Returns an error when the name is empty, too long, or has an illegal character.
pub fn validate_group_name(name: &str) -> anyhow::Result<()> {
    let bytes = name.as_bytes();
    let ok = !bytes.is_empty()
        && bytes.len() <= 80
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'.' || *b == b'-')
        && bytes[0].is_ascii_alphanumeric()
        && !name.ends_with('.')
        && !name.ends_with('-')
        && !name.contains("..")
        && !name.contains(".-")
        && !name.contains("-.");
    if !ok {
        bail!("group name must match [a-z0-9.-] and be at most 80 characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_length_and_rate() {
        let prefixes = vec!["bots.".to_string()];
        let ok = GroupCreatePolicy {
            enabled: true,
            prefixes: &prefixes,
            group_count: 2,
            creates_today: 0,
            max_groups: 200,
            creates_per_day: 3,
            is_admin: false,
        };
        assert!(check_new_group(&ok, "bots.widgets", "a short description").is_ok());
        assert!(
            check_new_group(&ok, "general", "a short description")
                .unwrap_err()
                .to_string()
                .contains("bots.")
        );
        assert!(check_new_group(&ok, "Bots.widgets", "a short description").is_err());
        assert!(check_new_group(&ok, "bots.", "a short description").is_err());
        assert!(check_new_group(&ok, "bots..x", "a short description").is_err());
        assert!(check_new_group(&ok, "bots.widgets", "").is_err());

        let limited = GroupCreatePolicy {
            creates_today: 3,
            ..ok
        };
        assert!(
            check_new_group(&limited, "bots.widgets", "a short description")
                .unwrap_err()
                .to_string()
                .contains("rate")
        );
        let full = GroupCreatePolicy {
            group_count: 200,
            creates_today: 0,
            ..ok
        };
        assert!(
            check_new_group(&full, "bots.widgets", "a short description")
                .unwrap_err()
                .to_string()
                .contains("limit")
        );

        let disabled = GroupCreatePolicy {
            enabled: false,
            ..ok
        };
        assert!(check_new_group(&disabled, "bots.widgets", "desc").is_err());
        let admin = GroupCreatePolicy {
            enabled: false,
            group_count: 99,
            creates_today: 99,
            max_groups: 1,
            creates_per_day: 1,
            is_admin: true,
            ..ok
        };
        assert!(check_new_group(&admin, "other.widgets", "desc").is_ok());
    }
}
