//! Capability tokens for iroh peers.
//!
//! A token is an `rcan`: an ed25519-signed, audience-bound, expiring grant.
//! Its capability is a [`Grant`], wildmat patterns for reading and posting.
//!
//! The server trusts tokens whose issuer is listed in
//! `[iroh] capability_issuers`; the token's audience must be the connecting
//! endpoint id. Such an endpoint is admitted without an account and gets
//! exactly the groups its grant names. Account passwords and `allowed_peers`
//! are unchanged and take precedence when they match.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{SigningKey, VerifyingKey};
use iroh::EndpointId;
use rcan::{Authorizer, Capability, CapabilityOrigin, Expires, Rcan};
use serde::{Deserialize, Serialize};

use crate::wildmat::wildmat;

/// What a token allows: read and post patterns per newsgroup.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// Wildmat patterns a peer may read.
    pub read: Vec<String>,
    /// Wildmat patterns a peer may post to.
    pub post: Vec<String>,
}

impl crate::session::ReadPostPolicy for CapabilitySession {
    fn can_read(&self, group: &str) -> bool {
        CapabilitySession::can_read(self, group)
    }

    fn can_post(&self, group: &str) -> bool {
        CapabilitySession::can_post(self, group)
    }

    fn label(&self) -> String {
        CapabilitySession::label(self)
    }
}

impl Grant {
    /// A grant that may read one concrete group.
    #[must_use]
    pub fn read(group: &str) -> Self {
        Self {
            read: vec![group.to_string()],
            post: Vec::new(),
        }
    }

    /// A grant that may post to one concrete group.
    #[must_use]
    pub fn post(group: &str) -> Self {
        Self {
            read: Vec::new(),
            post: vec![group.to_string()],
        }
    }

    fn covers(patterns: &[String], groups: &[String]) -> bool {
        groups
            .iter()
            .all(|group| patterns.iter().any(|pattern| wildmat(pattern, group)))
    }

    /// Whether this grant can do everything `other` asks for.
    #[must_use]
    pub fn permits_grant(&self, other: &Self) -> bool {
        Self::covers(&self.read, &other.read) && Self::covers(&self.post, &other.post)
    }
}

impl Capability for Grant {
    fn permits(&self, other: &Self) -> bool {
        self.permits_grant(other)
    }
}

/// One verified token, ready to answer per-group questions for its endpoint.
pub struct CapabilitySession {
    authorizer: Authorizer,
    token: Rcan<Grant>,
    endpoint: VerifyingKey,
    grant: Grant,
    issuer: VerifyingKey,
    expires: Expires,
}

impl std::fmt::Debug for CapabilitySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilitySession")
            .field("endpoint", &self.endpoint_hex())
            .field("issuer", &hex(&self.issuer))
            .field("grant", &self.grant)
            .field("expires", &self.expires)
            .finish()
    }
}

impl CapabilitySession {
    /// The endpoint this token was issued to, lowercase hex.
    #[must_use]
    pub fn endpoint_hex(&self) -> String {
        hex(&self.endpoint)
    }

    /// The issuer key, lowercase hex.
    #[must_use]
    pub fn issuer_hex(&self) -> String {
        hex(&self.issuer)
    }

    /// The grant as written in the token.
    #[must_use]
    pub fn grant(&self) -> &Grant {
        &self.grant
    }

    /// When this token stops working.
    #[must_use]
    pub fn expires(&self) -> &Expires {
        &self.expires
    }

    /// Short log label: endpoint, issuer, patterns, expiry. Never the token.
    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "capability {} from {} (read {:?}, post {:?}, expires {})",
            &self.endpoint_hex()[..16],
            &self.issuer_hex()[..16],
            self.grant.read,
            self.grant.post,
            self.expires
        )
    }

    /// Whether the token covers reading `group`.
    #[must_use]
    pub fn can_read(&self, group: &str) -> bool {
        self.permits(Grant::read(group))
    }

    /// Whether the token covers posting to `group`.
    #[must_use]
    pub fn can_post(&self, group: &str) -> bool {
        self.permits(Grant::post(group))
    }

    /// The full chain check: signature, expiry, issuer, audience, coverage.
    fn permits(&self, request: Grant) -> bool {
        self.authorizer
            .check_invocation_from(self.endpoint, request, &[&self.token])
            .is_ok()
    }
}

/// Tokens from configured issuers, indexed by audience.
#[derive(Default)]
pub struct CapabilityStore {
    tokens: Vec<CapabilitySession>,
}

impl std::fmt::Debug for CapabilityStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityStore")
            .field("tokens", &self.tokens.len())
            .finish()
    }
}

impl CapabilityStore {
    /// Parse `[iroh] capability_issuers` and `capability_tokens`.
    ///
    /// Every token must decode, carry a supported issuer, and be an
    /// issuer-origin grant; delegation chains are not accepted yet.
    ///
    /// # Errors
    ///
    /// Returns an error naming the bad issuer or token.
    pub fn from_config(issuers: &[String], tokens: &[String]) -> anyhow::Result<Self> {
        if tokens.is_empty() {
            return Ok(Self::default());
        }
        if issuers.is_empty() {
            bail!("iroh.capability_tokens needs at least one iroh.capability_issuers key");
        }
        let mut trusted = Vec::with_capacity(issuers.len());
        for issuer in issuers {
            trusted.push(parse_key(issuer).with_context(|| {
                format!("iroh.capability_issuers entry {issuer} is not a 64-hex ed25519 key")
            })?);
        }
        let mut sessions = Vec::with_capacity(tokens.len());
        for (index, token) in tokens.iter().enumerate() {
            sessions.push(
                parse_token(token, &trusted)
                    .with_context(|| format!("iroh.capability_tokens[{index}] is not usable"))?,
            );
        }
        Ok(Self { tokens: sessions })
    }

    /// True when no token is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// The unexpired token issued to `remote`, if there is one.
    #[must_use]
    pub fn session_for(&self, remote: EndpointId) -> Option<Arc<CapabilitySession>> {
        let remote = key_of(remote);
        let now = std::time::SystemTime::now();
        self.tokens
            .iter()
            .find(|session| session.endpoint == remote && session.expires.is_valid_at(now))
            .map(|session| Arc::new(session.clone()))
    }
}

impl Clone for CapabilitySession {
    fn clone(&self) -> Self {
        Self {
            authorizer: self.authorizer,
            token: self.token.clone(),
            endpoint: self.endpoint,
            grant: self.grant.clone(),
            issuer: self.issuer,
            expires: self.expires.clone(),
        }
    }
}

/// A token that never expires.
#[must_use]
pub fn expires_never() -> Expires {
    Expires::Never
}

/// A token that expires `secs` from minting.
#[must_use]
pub fn expires_in(secs: u64) -> Expires {
    Expires::valid_for(std::time::Duration::from_secs(secs))
}

/// Mint a token for `audience` and print it in the form the config takes.
///
/// # Errors
///
/// Returns an error if the grant contains no patterns.
pub fn mint(
    signing_key: &SigningKey,
    audience: EndpointId,
    grant: Grant,
    expires: Expires,
) -> anyhow::Result<String> {
    if grant.read.is_empty() && grant.post.is_empty() {
        bail!("a capability grant needs at least one read or post pattern");
    }
    let token = Rcan::issuing_builder(signing_key, key_of(audience), grant).sign(expires);
    Ok(BASE64.encode(token.encode()))
}

/// Lowercase hex, the form both key fields use.
#[must_use]
pub fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// The endpoint id for a raw ed25519 public key.
///
/// # Errors
///
/// Returns an error when the bytes are not a valid ed25519 point.
pub fn endpoint_from_bytes(bytes: &[u8; 32]) -> anyhow::Result<EndpointId> {
    EndpointId::from_bytes(bytes).map_err(|_| anyhow::anyhow!("not a valid endpoint id"))
}

/// Parse a 64-hex ed25519 private key, the form `renews admin
/// generate-capability-key` writes.
///
/// # Errors
///
/// Returns an error for text that is not 64 hex characters.
pub fn signing_key_from_hex(text: &str) -> anyhow::Result<SigningKey> {
    let bytes = hex_decode(text)?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected 64 hex characters (32 bytes)"))?;
    Ok(SigningKey::from_bytes(&bytes))
}

fn parse_key(text: &str) -> anyhow::Result<VerifyingKey> {
    let bytes = hex_decode(text)?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected 32 bytes"))?;
    VerifyingKey::from_bytes(&bytes).map_err(|err| anyhow::anyhow!("invalid ed25519 key: {err}"))
}

fn parse_token(text: &str, trusted: &[VerifyingKey]) -> anyhow::Result<CapabilitySession> {
    let bytes = BASE64.decode(text.trim()).context("token is not base64")?;
    let token: Rcan<Grant> = Rcan::decode(&bytes).context("token does not verify")?;
    let issuer = *token.issuer();
    let authorizer = trusted
        .iter()
        .find(|key| **key == issuer)
        .map(|key| Authorizer::new(*key))
        .context("token issuer is not in iroh.capability_issuers")?;
    if !matches!(token.capability_origin(), CapabilityOrigin::Issuer) {
        bail!("delegated tokens are not supported");
    }
    let expires = token.expires().clone();
    if !expires.is_valid_at(std::time::SystemTime::now()) {
        bail!("token expired ({expires})");
    }
    Ok(CapabilitySession {
        authorizer,
        endpoint: *token.audience(),
        grant: token.capability().clone(),
        issuer,
        expires,
        token,
    })
}

fn key_of(endpoint: EndpointId) -> VerifyingKey {
    // iroh endpoint ids and rcan audiences are the same ed25519 key bytes.
    VerifyingKey::from_bytes(endpoint.as_bytes()).expect("endpoint id is a valid ed25519 key")
}

fn hex(bytes: &VerifyingKey) -> String {
    bytes
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hex_decode(text: &str) -> anyhow::Result<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        bail!("odd-length hex");
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16).context("not hex")?;
        let lo = (pair[1] as char).to_digit(16).context("not hex")?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(seed: u8) -> (SigningKey, EndpointId) {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        let verifier = signing.verifying_key();
        let id = EndpointId::from_bytes(verifier.as_bytes()).expect("endpoint id");
        (signing, id)
    }

    fn store(issuer: &SigningKey, token: String) -> CapabilityStore {
        let issuer_hex = hex(&issuer.verifying_key());
        CapabilityStore::from_config(&[issuer_hex], &[token]).expect("store")
    }

    #[test]
    fn grants_compare_with_wildmat_patterns() {
        let broad = Grant {
            read: vec!["bots.*".into()],
            post: vec![],
        };
        assert!(broad.permits_grant(&Grant::read("bots.claims")));
        assert!(!broad.permits_grant(&Grant::read("other.group")));
        assert!(broad.permits_grant(&Grant {
            read: vec![],
            post: vec![]
        }));
        assert!(!Grant::default().permits_grant(&Grant::read("bots.claims")));
    }

    #[test]
    fn minted_token_admits_its_audience_with_its_patterns() {
        let (issuer, _issuer_id) = endpoint(1);
        let (_, audience) = endpoint(2);
        let token = mint(
            &issuer,
            audience,
            Grant {
                read: vec!["bots.*".into()],
                post: vec!["bots.general".into()],
            },
            Expires::At(u64::MAX - 1),
        )
        .expect("mint");
        let store = store(&issuer, token);
        assert!(!store.is_empty());

        let session = store.session_for(audience).expect("audience admitted");
        assert!(session.can_read("bots.claims"));
        assert!(!session.can_read("other.group"));
        assert!(session.can_post("bots.general"));
        assert!(!session.can_post("bots.claims"));
        assert_eq!(session.issuer_hex(), hex(&issuer.verifying_key()));

        let (_, stranger) = endpoint(3);
        assert!(store.session_for(stranger).is_none());
    }

    #[test]
    fn expired_unknown_issuer_and_delegated_tokens_are_refused() {
        let (issuer, _) = endpoint(4);
        let (_, audience) = endpoint(5);
        let short = mint(
            &issuer,
            audience,
            Grant::read("bots.general"),
            Expires::At(1),
        )
        .expect("mint");
        let error = CapabilityStore::from_config(&[hex(&issuer.verifying_key())], &[short])
            .expect_err("an expired token must be refused");
        assert!(format!("{error:#}").contains("expired"), "{error:#}");

        let (other, _) = endpoint(6);
        let token = mint(
            &issuer,
            audience,
            Grant::read("bots.general"),
            Expires::Never,
        )
        .expect("mint");
        let error = CapabilityStore::from_config(&[hex(&other.verifying_key())], &[token])
            .expect_err("issuer must be trusted");
        assert!(format!("{error:#}").contains("issuer"), "{error:#}");

        let (_delegator, owner) = endpoint(7);
        let delegated = Rcan::delegating_builder(
            &issuer,
            key_of(audience),
            key_of(owner),
            Grant::read("bots.general"),
        )
        .sign(Expires::Never);
        let error = CapabilityStore::from_config(
            &[hex(&issuer.verifying_key())],
            &[BASE64.encode(delegated.encode())],
        )
        .expect_err("delegations are not supported");
        assert!(format!("{error:#}").contains("delegat"), "{error:#}");
    }

    #[test]
    fn a_tampered_token_fails_to_decode() {
        let (issuer, _) = endpoint(8);
        let (_, audience) = endpoint(9);
        let token = mint(
            &issuer,
            audience,
            Grant::read("bots.general"),
            Expires::Never,
        )
        .expect("mint");
        let mut bytes = BASE64.decode(token).expect("base64");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        let error =
            CapabilityStore::from_config(&[hex(&issuer.verifying_key())], &[BASE64.encode(bytes)])
                .expect_err("tampered token");
        assert!(format!("{error:#}").contains("verify"), "{error:#}");
    }
}
