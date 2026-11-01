//! DNS discovery for the slime network.
//!
//! A bot may set `server` to a DNS name such as `slime.onix.computer`.
//! The client looks up TXT `_iroh-news.<name>` and expects either
//! `ticket=<EndpointTicket>` or `id=<EndpointId>`. A raw endpoint ticket
//! or a bare endpoint id is used as-is and does not touch DNS.
//!
//! [`DiscoveryMode::Auto`] tries the system resolver, then DNS-over-HTTPS
//! JSON at Cloudflare and Google, then the last good answer cached under
//! the state directory. An error and a NOERROR response with no usable
//! `ticket=` or `id=` record are both misses. Grok Bot hosts answer TXT
//! lookups with NOERROR and zero records, so the HTTPS resolvers are what
//! make `server = "slime.onix.computer"` work there.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, bail};
use iroh::{EndpointAddr, EndpointId};
use iroh_tickets::endpoint::EndpointTicket;

/// Label placed in front of the public DNS name.
pub const DISCOVERY_LABEL: &str = "_iroh-news";

/// How a DNS name is resolved.
///
/// `auto` is the system resolver, then Cloudflare DoH, then Google DoH,
/// then the cached record. `system` skips the public resolvers. `doh`
/// skips the system resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiscoveryMode {
    /// System DNS, then DNS-over-HTTPS, then the cached record.
    #[default]
    Auto,
    /// System DNS, then the cached record.
    System,
    /// DNS-over-HTTPS, then the cached record.
    Doh,
}

impl DiscoveryMode {
    /// Parse `auto`, `system`, or `doh`.
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is not one of those three names.
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "system" => Ok(Self::System),
            "doh" => Ok(Self::Doh),
            other => bail!("discovery must be auto, system, or doh (got {other})"),
        }
    }

    fn uses_system(self) -> bool {
        matches!(self, Self::Auto | Self::System)
    }

    fn uses_doh(self) -> bool {
        matches!(self, Self::Auto | Self::Doh)
    }
}

/// Which lookup produced the endpoint address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoverySource {
    /// `server` was already a ticket or endpoint id.
    Direct,
    /// The system resolver returned a usable TXT record.
    System,
    /// `https://cloudflare-dns.com/dns-query`.
    DohCloudflare,
    /// `https://dns.google/resolve`.
    DohGoogle,
    /// The last good record stored in the state directory.
    Cache,
}

impl DiscoverySource {
    /// Stable name printed by `iroh-news resolve`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::System => "system",
            Self::DohCloudflare => "doh-cloudflare",
            Self::DohGoogle => "doh-google",
            Self::Cache => "cache",
        }
    }
}

/// An endpoint address plus the lookup that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEndpoint {
    /// Address to dial.
    pub addr: EndpointAddr,
    /// Where `addr` came from.
    pub source: DiscoverySource,
}

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

/// Public DNS-over-HTTPS JSON resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DohProvider {
    Cloudflare,
    Google,
}

impl DohProvider {
    fn url(self) -> &'static str {
        match self {
            Self::Cloudflare => "https://cloudflare-dns.com/dns-query",
            Self::Google => "https://dns.google/resolve",
        }
    }
}

impl std::fmt::Display for DohProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cloudflare => "DoH Cloudflare",
            Self::Google => "DoH Google",
        })
    }
}

trait DohTxt {
    fn lookup_txt(
        &self,
        provider: DohProvider,
        name: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<String>>> + Send;
}

struct ReqwestDoh;

impl DohTxt for ReqwestDoh {
    async fn lookup_txt(&self, provider: DohProvider, name: &str) -> anyhow::Result<Vec<String>> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .context("building DoH client")?;
        let url =
            reqwest::Url::parse_with_params(provider.url(), [("name", name), ("type", "TXT")])
                .context("building DoH URL")?;
        let response = client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/dns-json")
            .header(reqwest::header::USER_AGENT, "iroh-news")
            .send()
            .await
            .with_context(|| format!("{provider} request failed"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("{provider} response body"))?;
        if !status.is_success() {
            bail!("{provider} HTTP {status}");
        }
        parse_doh_json(&body).with_context(|| format!("{provider} JSON"))
    }
}

/// Resolve `server` with one injected lookup. Tests use this to check the
/// cache without DNS-over-HTTPS. Production code calls [`discover`].
///
/// # Errors
///
/// Returns an error when a ticket or id is malformed, or when a DNS name
/// cannot be resolved and no cached answer exists.
#[cfg(test)]
async fn resolve_server(
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

/// Resolve `server` to an endpoint address.
///
/// Direct tickets and endpoint ids skip DNS. A DNS name follows `mode`:
/// system DNS, DNS-over-HTTPS, then the cached last-good record.
///
/// # Errors
///
/// Returns an error when a ticket or id is malformed, or when every lookup
/// misses and no cached answer exists.
pub async fn discover(
    server: &str,
    state_dir: &Path,
    mode: DiscoveryMode,
) -> anyhow::Result<ResolvedEndpoint> {
    discover_with(server, state_dir, mode, &HickoryTxt, &ReqwestDoh).await
}

async fn discover_with(
    server: &str,
    state_dir: &Path,
    mode: DiscoveryMode,
    system: &impl TxtLookup,
    doh: &impl DohTxt,
) -> anyhow::Result<ResolvedEndpoint> {
    let server = server.trim();
    if let Some(addr) = parse_direct(server)? {
        return Ok(ResolvedEndpoint {
            addr,
            source: DiscoverySource::Direct,
        });
    }
    let qname = discovery_qname(server);
    let mut misses = Vec::new();

    if mode.uses_system() {
        match lookup_usable(system.lookup_txt(&qname).await, "system DNS") {
            Ok(Some(record)) => {
                return finish(state_dir, server, record, DiscoverySource::System);
            }
            Ok(None) => misses.push("system DNS returned no ticket= or id= record".to_string()),
            Err(err) => misses.push(err),
        }
    }

    if mode.uses_doh() {
        for (provider, source) in [
            (DohProvider::Cloudflare, DiscoverySource::DohCloudflare),
            (DohProvider::Google, DiscoverySource::DohGoogle),
        ] {
            match lookup_usable(
                doh.lookup_txt(provider, &qname).await,
                &provider.to_string(),
            ) {
                Ok(Some(record)) => return finish(state_dir, server, record, source),
                Ok(None) => {
                    misses.push(format!("{provider} returned no ticket= or id= record"));
                }
                Err(err) => misses.push(err),
            }
        }
    }

    if let Some(cached) = read_discovery_cache(state_dir, server)? {
        tracing::warn!(name = server, "discovery missed; using the cached record");
        return Ok(ResolvedEndpoint {
            addr: cached.into_addr()?,
            source: DiscoverySource::Cache,
        });
    }

    bail!(
        "no records found for TXT {qname} ({}); there is no cached record",
        misses.join("; ")
    )
}

fn lookup_usable(
    result: anyhow::Result<Vec<String>>,
    label: &str,
) -> Result<Option<DiscoveryRecord>, String> {
    match result {
        Ok(records) => Ok(parse_txt_records(&records).ok()),
        Err(err) => Err(format!("{label}: {err:#}")),
    }
}

fn finish(
    state_dir: &Path,
    server: &str,
    record: DiscoveryRecord,
    source: DiscoverySource,
) -> anyhow::Result<ResolvedEndpoint> {
    if let Err(err) = write_discovery_cache(state_dir, server, &record) {
        tracing::warn!(error = %err, "failed to cache DNS discovery");
    }
    Ok(ResolvedEndpoint {
        addr: record.into_addr()?,
        source,
    })
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

/// Parse a DNS-over-HTTPS JSON response into one string per TXT record.
///
/// Character-strings inside a single TXT `data` field are concatenated, which
/// is how a value longer than 255 bytes is published. Non-TXT answers are
/// ignored. `Status` other than `NOERROR` (0) is an error.
///
/// # Errors
///
/// Returns an error when `body` is not the expected JSON object, or the
/// resolver status is not zero.
pub fn parse_doh_json(body: &str) -> anyhow::Result<Vec<String>> {
    let parsed: DohResponse = serde_json::from_str(body).context("DoH response is not JSON")?;
    if parsed.status != 0 {
        bail!("DoH status {}", parsed.status);
    }
    let mut records = Vec::new();
    for answer in parsed.answer {
        if answer.rr_type != 16 {
            continue;
        }
        records.push(decode_txt_rdata(&answer.data)?);
    }
    Ok(records)
}

#[derive(serde::Deserialize)]
struct DohResponse {
    #[serde(rename = "Status")]
    status: u16,
    #[serde(default, rename = "Answer")]
    answer: Vec<DohAnswer>,
}

#[derive(serde::Deserialize)]
struct DohAnswer {
    #[serde(rename = "type")]
    rr_type: u16,
    data: String,
}

/// Concatenate the character-strings of one TXT presentation (`data`).
///
/// `"ticket=abc" "def"` becomes `ticket=abcdef`. Decimal escapes (`\032`)
/// follow RFC 1035.
///
/// # Errors
///
/// Returns an error when a quoted string is not closed or an escape is truncated.
pub fn decode_txt_rdata(data: &str) -> anyhow::Result<String> {
    let bytes = data.as_bytes();
    let mut index = 0;
    let mut out = Vec::new();
    let mut any = false;
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        any = true;
        if bytes[index] == b'"' {
            index += 1;
            loop {
                if index >= bytes.len() {
                    bail!("unterminated TXT string in {data:?}");
                }
                if bytes[index] == b'"' {
                    index += 1;
                    break;
                }
                if bytes[index] == b'\\' {
                    index = push_txt_escape(bytes, index, &mut out)?;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
        } else {
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                if bytes[index] == b'\\' {
                    index = push_txt_escape(bytes, index, &mut out)?;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
        }
    }
    if !any {
        bail!("empty TXT rdata");
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn push_txt_escape(bytes: &[u8], index: usize, out: &mut Vec<u8>) -> anyhow::Result<usize> {
    if index + 1 >= bytes.len() {
        bail!("truncated TXT escape");
    }
    let next = bytes[index + 1];
    if next.is_ascii_digit() {
        let mut value: u32 = 0;
        let mut consumed = 0;
        while consumed < 3
            && index + 1 + consumed < bytes.len()
            && bytes[index + 1 + consumed].is_ascii_digit()
        {
            value = value * 10 + u32::from(bytes[index + 1 + consumed] - b'0');
            consumed += 1;
        }
        if value > 255 {
            bail!("TXT decimal escape {value} is out of range");
        }
        out.push(u8::try_from(value).expect("escape value is at most 255"));
        return Ok(index + 1 + consumed);
    }
    out.push(next);
    Ok(index + 2)
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
                bail!("no records found");
            }
            Ok(self.records.clone())
        }
    }

    struct FakeDoh {
        cloudflare: Mutex<Result<Vec<String>, String>>,
        google: Mutex<Result<Vec<String>, String>>,
        calls: Mutex<Vec<&'static str>>,
    }

    impl DohTxt for FakeDoh {
        async fn lookup_txt(
            &self,
            provider: DohProvider,
            name: &str,
        ) -> anyhow::Result<Vec<String>> {
            assert_eq!(name, "_iroh-news.slime.onix.computer");
            let (label, slot) = match provider {
                DohProvider::Cloudflare => ("cloudflare", &self.cloudflare),
                DohProvider::Google => ("google", &self.google),
            };
            self.calls.lock().unwrap().push(label);
            match slot.lock().unwrap().as_ref() {
                Ok(records) => Ok(records.clone()),
                Err(err) => bail!("{err}"),
            }
        }
    }

    fn fake_doh(
        cloudflare: Result<Vec<String>, String>,
        google: Result<Vec<String>, String>,
    ) -> FakeDoh {
        FakeDoh {
            cloudflare: Mutex::new(cloudflare),
            google: Mutex::new(google),
            calls: Mutex::new(Vec::new()),
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

    #[test]
    fn doh_json_concatenates_quoted_chunks() {
        let ticket = sample_ticket();
        let split = ticket.len() / 2;
        let body = format!(
            r#"{{"Status":0,"TC":false,"Answer":[{{"name":"_iroh-news.slime.onix.computer","type":16,"TTL":300,"data":"\"ticket={}\" \"{}\""}}]}}"#,
            &ticket[..split],
            &ticket[split..]
        );
        let records = parse_doh_json(&body).unwrap();
        assert_eq!(records, vec![format!("ticket={ticket}")]);
        assert_eq!(
            parse_txt_records(&records).unwrap(),
            DiscoveryRecord::Ticket(ticket)
        );
    }

    #[test]
    fn doh_json_keeps_separate_records_and_skips_other_types() {
        let id = SecretKey::generate().public().to_string();
        let body = format!(
            r#"{{"Status":0,"Answer":[
                {{"type":16,"data":"\"v=spf1 -all\""}},
                {{"type":1,"data":"1.2.3.4"}},
                {{"type":16,"data":"\"id=\" \"{id}\""}}
            ]}}"#
        );
        let records = parse_doh_json(&body).unwrap();
        assert_eq!(records, vec!["v=spf1 -all".to_string(), format!("id={id}")]);
        assert_eq!(
            parse_txt_records(&records).unwrap(),
            DiscoveryRecord::Id(id)
        );
    }

    #[test]
    fn doh_json_decodes_decimal_escapes_inside_quotes() {
        let data = r#""ticket=ab\032c""#;
        assert_eq!(decode_txt_rdata(data).unwrap(), "ticket=ab c");
        let body = r#"{"Status":0,"Answer":[{"type":16,"data":"\"hello\\\"world\""}]}"#;
        assert_eq!(
            parse_doh_json(body).unwrap(),
            vec!["hello\"world".to_string()]
        );
    }

    #[test]
    fn doh_json_empty_noerror_is_a_miss_and_nonzero_status_is_an_error() {
        assert!(parse_doh_json(r#"{"Status":0}"#).unwrap().is_empty());
        let err = parse_doh_json(r#"{"Status":3,"Answer":[]}"#).unwrap_err();
        assert!(err.to_string().contains("status 3"), "{err}");
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

    /// Live `_iroh-news.slime.onix.computer` TXT value. Public DNS data.
    const SLIME_ID: &str = "a06dd91a400564f6b9ad8638ec339986748a06d8b5fb51143dbe5f030aff9fe7";

    #[tokio::test]
    async fn doh_id_record_resolves_when_system_txt_is_empty() {
        let body = format!(
            r#"{{"Status":0,"Answer":[{{"name":"_iroh-news.slime.onix.computer","type":16,"TTL":300,"data":"\"id={SLIME_ID}\""}}]}}"#
        );
        let records = parse_doh_json(&body).unwrap();
        assert_eq!(records, vec![format!("id={SLIME_ID}")]);
        assert_eq!(
            parse_txt_records(&records).unwrap(),
            DiscoveryRecord::Id(SLIME_ID.to_string())
        );

        let system = MapLookup {
            records: vec![],
            calls: Mutex::new(0),
            fail: false,
        };
        let doh = fake_doh(Ok(records), Err("unused".into()));
        let dir = tempfile::tempdir().unwrap();
        let resolved = discover_with(
            "slime.onix.computer",
            dir.path(),
            DiscoveryMode::Auto,
            &system,
            &doh,
        )
        .await
        .unwrap();
        assert_eq!(resolved.source, DiscoverySource::DohCloudflare);
        assert_eq!(resolved.addr.id.to_string(), SLIME_ID);
        assert!(resolved.addr.addrs.is_empty());
        assert_eq!(*system.calls.lock().unwrap(), 1);
        assert_eq!(doh.calls.lock().unwrap().as_slice(), ["cloudflare"]);
        let cached =
            std::fs::read_to_string(dir.path().join("discovery/slime.onix.computer")).unwrap();
        assert_eq!(cached, format!("id={SLIME_ID}"));
    }

    #[tokio::test]
    async fn empty_system_answer_falls_through_to_cloudflare() {
        let ticket = sample_ticket();
        let system = MapLookup {
            records: vec!["v=spf1 -all".into()],
            calls: Mutex::new(0),
            fail: false,
        };
        let doh = fake_doh(Ok(vec![format!("ticket={ticket}")]), Err("unused".into()));
        let dir = tempfile::tempdir().unwrap();
        let resolved = discover_with(
            "slime.onix.computer",
            dir.path(),
            DiscoveryMode::Auto,
            &system,
            &doh,
        )
        .await
        .unwrap();
        assert_eq!(resolved.source, DiscoverySource::DohCloudflare);
        assert_eq!(resolved.addr.id.to_string().len(), 64);
        assert_eq!(doh.calls.lock().unwrap().as_slice(), ["cloudflare"]);
        let cached =
            std::fs::read_to_string(dir.path().join("discovery/slime.onix.computer")).unwrap();
        assert!(cached.starts_with("ticket="));
    }

    #[tokio::test]
    async fn system_error_then_cloudflare_error_uses_google() {
        let ticket = sample_ticket();
        let system = MapLookup {
            records: vec![],
            calls: Mutex::new(0),
            fail: true,
        };
        let doh = fake_doh(
            Err("HTTP 500".into()),
            Ok(vec![format!("\"ticket={ticket}\"")]),
        );
        let dir = tempfile::tempdir().unwrap();
        let resolved = discover_with(
            "slime.onix.computer",
            dir.path(),
            DiscoveryMode::Auto,
            &system,
            &doh,
        )
        .await
        .unwrap();
        assert_eq!(resolved.source, DiscoverySource::DohGoogle);
        assert_eq!(
            doh.calls.lock().unwrap().as_slice(),
            ["cloudflare", "google"]
        );
    }

    #[tokio::test]
    async fn all_misses_use_the_cached_record() {
        let ticket = sample_ticket();
        let system = MapLookup {
            records: vec![],
            calls: Mutex::new(0),
            fail: true,
        };
        let doh = fake_doh(Ok(vec![]), Err("status 2".into()));
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("discovery")).unwrap();
        std::fs::write(
            dir.path().join("discovery/slime.onix.computer"),
            format!("ticket={ticket}"),
        )
        .unwrap();
        let resolved = discover_with(
            "slime.onix.computer",
            dir.path(),
            DiscoveryMode::Auto,
            &system,
            &doh,
        )
        .await
        .unwrap();
        assert_eq!(resolved.source, DiscoverySource::Cache);
    }

    #[tokio::test]
    async fn system_mode_does_not_call_doh_and_doh_mode_skips_the_system_resolver() {
        let ticket = sample_ticket();
        let system = MapLookup {
            records: vec![],
            calls: Mutex::new(0),
            fail: true,
        };
        let doh = fake_doh(Ok(vec![format!("ticket={ticket}")]), Err("unused".into()));
        let dir = tempfile::tempdir().unwrap();
        let err = discover_with(
            "slime.onix.computer",
            dir.path(),
            DiscoveryMode::System,
            &system,
            &doh,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no records found"), "{err}");
        assert!(doh.calls.lock().unwrap().is_empty());

        let resolved = discover_with(
            "slime.onix.computer",
            dir.path(),
            DiscoveryMode::Doh,
            &system,
            &doh,
        )
        .await
        .unwrap();
        assert_eq!(resolved.source, DiscoverySource::DohCloudflare);
        assert_eq!(*system.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn direct_ticket_reports_direct_and_skips_lookups() {
        let ticket = sample_ticket();
        let system = MapLookup {
            records: vec![],
            calls: Mutex::new(0),
            fail: true,
        };
        let doh = fake_doh(Err("unused".into()), Err("unused".into()));
        let dir = tempfile::tempdir().unwrap();
        let resolved = discover_with(&ticket, dir.path(), DiscoveryMode::Auto, &system, &doh)
            .await
            .unwrap();
        assert_eq!(resolved.source, DiscoverySource::Direct);
        assert_eq!(*system.calls.lock().unwrap(), 0);
        assert!(doh.calls.lock().unwrap().is_empty());
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

    #[test]
    fn discovery_mode_names() {
        assert_eq!(DiscoveryMode::parse("Auto").unwrap(), DiscoveryMode::Auto);
        assert_eq!(
            DiscoveryMode::parse("system").unwrap(),
            DiscoveryMode::System
        );
        assert_eq!(DiscoveryMode::parse("doh").unwrap(), DiscoveryMode::Doh);
        assert!(DiscoveryMode::parse("doh-google").is_err());
    }
}
