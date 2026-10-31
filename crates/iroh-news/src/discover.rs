//! DNS discovery for the slime network.
//!
//! A bot may set `server` to a DNS name such as `slime.onix.computer`.
//! The client looks up TXT `_iroh-news.<name>` and expects either
//! `ticket=<EndpointTicket>` or `id=<EndpointId>`. A raw endpoint ticket
//! or a bare endpoint id is used as-is and does not touch DNS.
//! The last successful TXT answer is cached under the state directory.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, bail};
use iroh::{EndpointAddr, EndpointId};
use iroh_tickets::endpoint::EndpointTicket;

/// Label placed in front of the public DNS name.
pub const DISCOVERY_LABEL: &str = "_iroh-news";

/// What a discovery TXT record resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryRecord {
    /// Full endpoint ticket, including any direct addresses and relays.
    Ticket(String),
    /// Endpoint id only. iroh's n0 address lookup fills in the path.
    Id(String),
}

impl DiscoveryRecord {
    /// Encode the record the way it is published and cached.
    #[must_use]
    pub fn publish_value(&self) -> String {
        match self {
            Self::Ticket(ticket) => format!("ticket={ticket}"),
            Self::Id(id) => format!("id={id}"),
        }
    }

    fn into_addr(self) -> anyhow::Result<EndpointAddr> {
        match self {
            Self::Ticket(ticket) => {
                let parsed = EndpointTicket::from_str(&ticket)
                    .map_err(|err| anyhow::anyhow!("invalid endpoint ticket: {err}"))?;
                Ok(parsed.endpoint_addr().clone())
            }
            Self::Id(id) => {
                let id: EndpointId = id
                    .parse()
                    .map_err(|err| anyhow::anyhow!("invalid endpoint id: {err}"))?;
                Ok(EndpointAddr::new(id))
            }
        }
    }
}

/// TXT lookup. Tests supply a fake. The CLI uses [`HickoryTxt`].
pub trait TxtLookup {
    /// Return the character-strings of each TXT record at `name`.
    fn lookup_txt(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<String>>> + Send;
}

/// System resolver via hickory.
pub struct HickoryTxt;

impl TxtLookup for HickoryTxt {
    async fn lookup_txt(&self, name: &str) -> anyhow::Result<Vec<String>> {
        let resolver = hickory_resolver::TokioResolver::builder_tokio()
            .context("building DNS resolver")?
            .build();
        let lookup = resolver
            .txt_lookup(name)
            .await
            .with_context(|| format!("TXT lookup {name}"))?;
        Ok(lookup
            .iter()
            .map(|txt| {
                txt.txt_data()
                    .iter()
                    .map(|chunk| String::from_utf8_lossy(chunk))
                    .collect::<String>()
            })
            .collect())
    }
}

/// Resolve `server` to an [`EndpointAddr`].
///
/// # Errors
///
/// Returns an error when a ticket or id is malformed, or when a DNS name
/// cannot be resolved and no cached answer exists.
pub async fn resolve_server(
    server: &str,
    state_dir: &Path,
    dns: &impl TxtLookup,
) -> anyhow::Result<EndpointAddr> {
    let server = server.trim();
    if let Some(addr) = parse_direct(server)? {
        return Ok(addr);
    }
    let qname = discovery_qname(server);
    match dns.lookup_txt(&qname).await {
        Ok(records) => {
            let record =
                parse_txt_records(&records).with_context(|| format!("parsing TXT {qname}"))?;
            if let Err(err) = write_discovery_cache(state_dir, server, &record) {
                tracing::warn!(error = %err, "failed to cache DNS discovery");
            }
            record.into_addr()
        }
        Err(err) => {
            if let Some(cached) = read_discovery_cache(state_dir, server)? {
                tracing::warn!(
                    error = %err,
                    name = server,
                    "DNS discovery failed; using the cached record"
                );
                return cached.into_addr();
            }
            Err(err).context(format!(
                "DNS TXT {qname} failed and there is no cached record"
            ))
        }
    }
}

/// Query name for a public server name (`slime.onix.computer` -> `_iroh-news.slime.onix.computer`).
#[must_use]
pub fn discovery_qname(name: &str) -> String {
    format!("{DISCOVERY_LABEL}.{name}")
}

/// The line an operator pastes into Cloudflare.
#[must_use]
pub fn cloudflare_txt_line(name: &str, record: &DiscoveryRecord) -> String {
    format!(
        "{}. IN TXT \"{}\"",
        discovery_qname(name),
        record.publish_value()
    )
}

/// Parse TXT RDATA. Accepts `ticket=` and `id=`, with optional surrounding quotes.
///
/// # Errors
///
/// Returns an error when no record carries a recognized prefix, or the value
/// is not a ticket or endpoint id.
pub fn parse_txt_records(records: &[impl AsRef<str>]) -> anyhow::Result<DiscoveryRecord> {
    for record in records {
        let text = record.as_ref().trim().trim_matches('"').trim();
        if let Some(value) = text.strip_prefix("ticket=") {
            let value = value.trim().trim_matches('"');
            if value.is_empty() {
                bail!("ticket= is empty");
            }
            EndpointTicket::from_str(value)
                .map_err(|err| anyhow::anyhow!("invalid endpoint ticket: {err}"))?;
            return Ok(DiscoveryRecord::Ticket(value.to_string()));
        }
        if let Some(value) = text.strip_prefix("id=") {
            let value = value.trim().trim_matches('"');
            let _: EndpointId = value
                .parse()
                .map_err(|err| anyhow::anyhow!("invalid endpoint id: {err}"))?;
            return Ok(DiscoveryRecord::Id(value.to_string()));
        }
    }
    bail!("TXT records need ticket=<EndpointTicket> or id=<EndpointId>");
}

fn parse_direct(server: &str) -> anyhow::Result<Option<EndpointAddr>> {
    if server.starts_with("endpoint") {
        let ticket = EndpointTicket::from_str(server)
            .map_err(|err| anyhow::anyhow!("invalid endpoint ticket: {err}"))?;
        return Ok(Some(ticket.endpoint_addr().clone()));
    }
    if is_endpoint_id(server) {
        let id: EndpointId = server
            .parse()
            .map_err(|err| anyhow::anyhow!("invalid endpoint id: {err}"))?;
        return Ok(Some(EndpointAddr::new(id)));
    }
    Ok(None)
}

fn is_endpoint_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn cache_path(state_dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        bail!("discovery name {name:?} cannot be used as a cache file");
    }
    Ok(state_dir.join("discovery").join(name))
}

fn write_discovery_cache(
    state_dir: &Path,
    name: &str,
    record: &DiscoveryRecord,
) -> anyhow::Result<()> {
    let path = cache_path(state_dir, name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, record.publish_value())?;
    Ok(())
}

fn read_discovery_cache(state_dir: &Path, name: &str) -> anyhow::Result<Option<DiscoveryRecord>> {
    let path = cache_path(state_dir, name)?;
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(Some(parse_txt_records(&[text])?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;
    use iroh::TransportAddr;
    use iroh_tickets::Ticket;
    use std::net::SocketAddr;
    use std::sync::Mutex;

    fn sample_ticket() -> String {
        let key = SecretKey::generate();
        let addr = EndpointAddr::from_parts(
            key.public(),
            [TransportAddr::Ip(
                "127.0.0.1:9".parse::<SocketAddr>().unwrap(),
            )],
        );
        EndpointTicket::new(addr).encode_string()
    }

    struct MapLookup {
        records: Vec<String>,
        calls: Mutex<u32>,
        fail: bool,
    }

    impl TxtLookup for MapLookup {
        async fn lookup_txt(&self, name: &str) -> anyhow::Result<Vec<String>> {
            *self.calls.lock().unwrap() += 1;
            assert_eq!(name, "_iroh-news.slime.onix.computer");
            if self.fail {
                bail!("resolver unavailable");
            }
            Ok(self.records.clone())
        }
    }

    #[test]
    fn parses_ticket_and_id_txt() {
        let ticket = sample_ticket();
        let parsed = parse_txt_records(&[format!("\"ticket={ticket}\"")]).unwrap();
        assert_eq!(parsed, DiscoveryRecord::Ticket(ticket));

        let id = SecretKey::generate().public().to_string();
        let parsed = parse_txt_records(&[format!("id={id}")]).unwrap();
        assert_eq!(parsed, DiscoveryRecord::Id(id));
    }

    #[test]
    fn rejects_unrelated_txt() {
        let err = parse_txt_records(&["v=spf1 -all"]).unwrap_err();
        assert!(err.to_string().contains("ticket="));
    }

    #[tokio::test]
    async fn dns_name_uses_txt_and_caches() {
        let ticket = sample_ticket();
        let dns = MapLookup {
            records: vec![format!("ticket={ticket}")],
            calls: Mutex::new(0),
            fail: false,
        };
        let dir = tempfile::tempdir().unwrap();
        let addr = resolve_server("slime.onix.computer", dir.path(), &dns)
            .await
            .unwrap();
        assert_eq!(addr.addrs.len(), 1);
        let cached =
            std::fs::read_to_string(dir.path().join("discovery/slime.onix.computer")).unwrap();
        assert_eq!(cached, format!("ticket={ticket}"));

        let down = MapLookup {
            records: vec![],
            calls: Mutex::new(0),
            fail: true,
        };
        let again = resolve_server("slime.onix.computer", dir.path(), &down)
            .await
            .unwrap();
        assert_eq!(again.id, addr.id);
    }

    #[tokio::test]
    async fn ticket_string_skips_dns() {
        let ticket = sample_ticket();
        let dns = MapLookup {
            records: vec![],
            calls: Mutex::new(0),
            fail: true,
        };
        let dir = tempfile::tempdir().unwrap();
        resolve_server(&ticket, dir.path(), &dns).await.unwrap();
        assert_eq!(*dns.calls.lock().unwrap(), 0);
    }

    #[test]
    fn cloudflare_line_for_slime() {
        let line = cloudflare_txt_line(
            "slime.onix.computer",
            &DiscoveryRecord::Ticket("endpointabc".into()),
        );
        assert_eq!(
            line,
            "_iroh-news.slime.onix.computer. IN TXT \"ticket=endpointabc\""
        );
    }
}
