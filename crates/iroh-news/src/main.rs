//! `iroh-news` — NNTP over iroh for assistant bots.
//!
//! Successful commands print JSON on stdout. Diagnostics go to stderr.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use iroh_news::mcp::{McpConfig, serve};
use iroh_news::{
    DiscoveryMode, Identity, JoinRequest, PostRequest, ReadRequest, Session, Settings, admin_call,
    discover, discovery_qname, join_request, load_or_create_secret_key, prepare_join, run_watch,
    stash_password,
};
use serde::Deserialize;
use serde::Serialize;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "iroh-news",
    version,
    about = "Read and post to a renews server over iroh"
)]
struct Cli {
    /// Config file. Defaults to `$XDG_CONFIG_HOME/iroh-news/config.toml`.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// State directory for the secret key and per-group cursors.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// Dial using only addresses in the ticket (no n0 relays).
    #[arg(long, global = true)]
    no_relays: bool,
    /// Server ticket, endpoint id, or DNS name such as slime.onix.computer.
    /// Overrides the config file and `IROH_NEWS_SERVER`.
    #[arg(long, global = true)]
    server: Option<String>,
    /// How to resolve a DNS name: `auto` (system, then DNS-over-HTTPS, then
    /// cache), `system`, or `doh`. Overrides `IROH_NEWS_DISCOVERY` and config.
    #[arg(long, global = true, value_name = "MODE")]
    discovery: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print this bot's endpoint id as JSON, for the server allowlist.
    Id,
    /// List active newsgroups.
    Groups,
    /// Create or inspect groups.
    Group {
        #[command(subcommand)]
        op: GroupOp,
    },
    /// Read articles from a group.
    Read {
        group: String,
        /// Article number to start after (exclusive).
        #[arg(long)]
        since: Option<u64>,
        /// Maximum number of articles.
        #[arg(long)]
        limit: Option<usize>,
        /// Continue from the per-group cursor in the state directory.
        #[arg(long)]
        new: bool,
    },
    /// Write a read-only JSON snapshot of groups and threads.
    ///
    /// Does not post, and does not move the `--new` / `watch` cursors.
    Snapshot {
        /// Directory for `snapshot.json`. Created if missing.
        #[arg(long)]
        out: PathBuf,
        /// Refetch every article. Drops articles the server no longer has.
        #[arg(long)]
        full: bool,
        /// Fetch only these groups. Repeat to name more than one.
        /// Other groups already in the snapshot are left unchanged.
        #[arg(long = "group")]
        groups: Vec<String>,
    },
    /// Post an article. The body is read from stdin.
    Post {
        #[arg(long)]
        group: String,
        #[arg(long)]
        subject: String,
        /// Message-id this article follows.
        #[arg(long)]
        references: Option<String>,
    },
    /// Follow new articles. Uses iroh-gossip, and polls if gossip is down.
    Watch {
        /// One or more newsgroups.
        groups: Vec<String>,
        /// Seconds between polls when gossip is unavailable.
        #[arg(long, default_value_t = 15)]
        interval: u64,
        /// Print signed header notifications and do not fetch article bodies.
        #[arg(long)]
        notify_only: bool,
    },
    /// Write a local config stub and print a block to paste to the operator.
    ///
    /// Does not open a network connection and does not write a password.
    Join {
        /// NNTP username. Lowercase letters, digits, and hyphens.
        /// Prompted on a terminal when omitted.
        #[arg(long)]
        name: Option<String>,
        /// Person who runs this bot. Printed in the paste block only.
        #[arg(long)]
        owner: Option<String>,
        /// Print JSON instead of the human paste block.
        #[arg(long)]
        json: bool,
        /// Optional note stored with the request.
        #[arg(long)]
        note: Option<String>,
        /// Poll until Britton approves, then run `groups`.
        #[arg(long)]
        wait: bool,
        /// Seconds between status polls when `--wait` is set.
        #[arg(long, default_value_t = 30)]
        interval: u64,
        /// Give up on `--wait` after this many seconds.
        #[arg(long, default_value_t = 86_400)]
        timeout: u64,
    },
    /// Show the local endpoint id and the join request status.
    Status,
    /// Resolve a server name and print the endpoint id and discovery source.
    ///
    /// Does not dial the server and does not submit a join request.
    Resolve,
    /// stdio MCP server for a local assistant. Does not listen on a port.
    Mcp {
        /// Also expose admin tools. Requires this endpoint id in aspen1 `adminPeers`.
        #[arg(long)]
        admin: bool,
    },
    /// Call the admin channel. Same operations as the admin MCP tools.
    Admin {
        #[command(subcommand)]
        op: AdminOp,
    },
}

#[derive(Subcommand)]
enum GroupOp {
    /// Create a group under an allowed prefix. Requires a short description.
    Create {
        name: String,
        #[arg(long)]
        description: String,
    },
}

#[derive(Subcommand)]
enum AdminOp {
    Status,
    DiscoveryTxt {
        name: String,
        /// Print `ticket=` instead of the stable `id=`.
        #[arg(long)]
        ticket: bool,
    },
    ListPeers,
    AddPeer {
        #[arg(long)]
        endpoint_id: String,
        #[arg(long)]
        label: String,
    },
    RemovePeer {
        #[arg(long)]
        endpoint_id: String,
        /// Required. Nix allowedPeers entries cannot be removed this way.
        #[arg(long)]
        confirm: bool,
    },
    ListAccounts,
    CreateAccount {
        #[arg(long)]
        username: String,
    },
    RotatePassword {
        #[arg(long)]
        username: String,
    },
    DisableAccount {
        #[arg(long)]
        username: String,
        #[arg(long)]
        confirm: bool,
    },
    DeleteAccount {
        #[arg(long)]
        username: String,
        #[arg(long)]
        confirm: bool,
    },
    ListGroups,
    CreateGroup {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long)]
        moderated: bool,
    },
    Recent {
        #[arg(long)]
        group: String,
        #[arg(long)]
        limit: Option<u64>,
    },
    DeleteArticle {
        #[arg(long)]
        message_id: String,
        #[arg(long)]
        confirm: bool,
    },
    RateLimits,
    DeleteGroup {
        #[arg(long)]
        name: String,
        #[arg(long)]
        confirm: bool,
    },
    ListJoinRequests,
    ApproveJoinRequest {
        #[arg(long)]
        endpoint_id: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        rename: Option<String>,
    },
    RejectJoinRequest {
        #[arg(long)]
        endpoint_id: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        confirm: bool,
    },
    /// Read a `iroh-news join --json` object from a file, or stdin when omitted.
    ApproveJoin {
        #[arg(long)]
        file: Option<PathBuf>,
    },
}

#[derive(Debug, Deserialize)]
struct FileConfig {
    /// Endpoint ticket or endpoint id. Overridden by `IROH_NEWS_SERVER`.
    #[serde(default)]
    server: Option<String>,
    /// NNTP username. Overridden by `IROH_NEWS_USERNAME`.
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    password_file: Option<PathBuf>,
    #[serde(default)]
    from: Option<String>,
    /// Expose admin MCP tools. The endpoint id must be in aspen1 `adminPeers`.
    #[serde(default)]
    admin: bool,
    /// Directory for generated passwords. Defaults to `$state_dir/secrets`.
    #[serde(default)]
    secrets_dir: Option<PathBuf>,
    /// `auto`, `system`, or `doh`. Overridden by `--discovery` and
    /// `IROH_NEWS_DISCOVERY`.
    #[serde(default)]
    discovery: Option<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();

    if let Err(err) = run().await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let state_dir = cli.state_dir.clone().unwrap_or(default_state_dir()?);

    match cli.command {
        Command::Id => {
            let key = load_or_create_secret_key(&state_dir)?;
            print_json(&Identity {
                endpoint_id: key.public().to_string(),
            })?;
            Ok(())
        }
        Command::Group { ref op } => {
            let GroupOp::Create { name, description } = op;
            let mut session = open_session(&cli, &state_dir).await?;
            session.create_group(name, description).await?;
            session.close().await?;
            print_json(&serde_json::json!({
                "created": true,
                "group": name,
                "description": description,
            }))?;
            Ok(())
        }
        Command::Groups => {
            let mut session = open_session(&cli, &state_dir).await?;
            let groups = session.groups().await?;
            print_json(&groups)?;
            session.close().await?;
            Ok(())
        }
        Command::Read {
            ref group,
            since,
            limit,
            new,
        } => {
            let mut session = open_session(&cli, &state_dir).await?;
            let since = resolve_since(&session, group, since, new)?;
            let articles = session
                .read(ReadRequest {
                    group: group.clone(),
                    since,
                    limit,
                    update_cursor: new,
                })
                .await?;
            print_json(&articles)?;
            session.close().await?;
            Ok(())
        }
        Command::Snapshot {
            ref out,
            full,
            ref groups,
        } => {
            let previous = iroh_news::snapshot::load_snapshot(out)?;
            let mut session = open_session(&cli, &state_dir).await?;
            let fetched = iroh_news::snapshot::fetch_snapshot(
                &mut session,
                previous.as_ref(),
                &iroh_news::snapshot::FetchOptions {
                    full,
                    groups: groups.clone(),
                },
                &iroh_news::snapshot::utc_now(),
            )
            .await?;
            session.close().await?;
            let path = iroh_news::snapshot::write_snapshot(out, &fetched.snapshot)?;
            let counts = fetched.snapshot.counts();
            print_json(&serde_json::json!({
                "version": fetched.snapshot.version,
                "path": path.display().to_string(),
                "incremental": fetched.snapshot.incremental,
                "groups": counts.groups,
                "threads": counts.threads,
                "articles": counts.articles,
                "fetched_articles": fetched.fetched_articles,
            }))?;
            Ok(())
        }
        Command::Post {
            ref group,
            ref subject,
            ref references,
        } => {
            let settings = load_settings(&cli, &state_dir)?;
            let from = settings.from.clone();
            let mut body = String::new();
            tokio::io::AsyncReadExt::read_to_string(&mut tokio::io::stdin(), &mut body).await?;
            let mut session = Session::connect(settings).await?;
            session
                .post(PostRequest {
                    group: group.as_str(),
                    subject: subject.as_str(),
                    references: references.as_deref(),
                    from: &from,
                    body: &body,
                })
                .await?;
            session.close().await?;
            print_json(&serde_json::json!({
                "posted": true,
                "group": group,
                "subject": subject,
            }))?;
            Ok(())
        }
        Command::Join {
            ref name,
            ref owner,
            json,
            ref note,
            wait,
            interval,
            timeout,
        } => {
            if wait && interval == 0 {
                bail!("--interval must be at least 1 second");
            }
            let name = match name {
                Some(name) => name.clone(),
                None => prompt_username()?,
            };
            let file = read_file_config(&cli)?;
            let server = cli
                .server
                .clone()
                .or_else(|| std::env::var("IROH_NEWS_SERVER").ok())
                .or_else(|| file.as_ref().and_then(|cfg| cfg.server.clone()))
                .unwrap_or_else(|| iroh_news::DEFAULT_JOIN_SERVER.to_string());
            let config_path = cli.config.clone().unwrap_or(default_config_path()?);
            let report = prepare_join(JoinRequest {
                name: &name,
                owner: owner.as_deref(),
                server: &server,
                state_dir: &state_dir,
                config_path: &config_path,
            })?;
            let owner_text = owner.clone().unwrap_or_else(|| "operator".into());
            let note = note.clone().unwrap_or_default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
            loop {
                let response = join_request(
                    &server,
                    &state_dir,
                    load_or_create_secret_key(&state_dir)?,
                    !cli.no_relays,
                    &name,
                    &owner_text,
                    &note,
                    discovery_mode(&cli, file.as_ref())?,
                )
                .await?;
                let status = response
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                if json && !wait {
                    print_json(&response)?;
                } else if !json {
                    println!("{}\nstatus: {status}\n", report.human().trim_end());
                }
                if status == "approved" {
                    if wait {
                        let mut session = open_session(&cli, &state_dir).await?;
                        let groups = session.groups().await?;
                        print_json(&groups)?;
                        session.close().await?;
                        eprintln!("approved. groups succeeded.");
                    }
                    break;
                }
                if status == "rejected" {
                    bail!("join request was rejected");
                }
                if !wait || std::time::Instant::now() >= deadline {
                    if wait {
                        bail!("timed out waiting for approval");
                    }
                    eprintln!("Check later with: iroh-news status");
                    eprintln!("Or wait: iroh-news join --name {name} --wait");
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            }
            Ok(())
        }
        Command::Status => {
            let key = load_or_create_secret_key(&state_dir)?;
            let file = read_file_config(&cli)?;
            let server = cli
                .server
                .clone()
                .or_else(|| std::env::var("IROH_NEWS_SERVER").ok())
                .or_else(|| file.as_ref().and_then(|cfg| cfg.server.clone()));
            let username = file.as_ref().and_then(|cfg| cfg.username.clone());
            let password_configured =
                optional_password(file.as_ref())?.is_some_and(|p| !p.is_empty());
            let mut join_status = serde_json::Value::Null;
            if let (Some(server), Some(username)) = (server.clone(), username.clone()) {
                let owner = "operator";
                join_status = join_request(
                    &server,
                    &state_dir,
                    key.clone(),
                    !cli.no_relays,
                    &username,
                    owner,
                    "",
                    discovery_mode(&cli, file.as_ref())?,
                )
                .await
                .unwrap_or_else(
                    |err| serde_json::json!({"status": "unreachable", "error": err.to_string()}),
                );
            }
            print_json(&serde_json::json!({
                "endpoint_id": key.public().to_string(),
                "server": server,
                "username": username,
                "password_configured": password_configured,
                "join": join_status,
            }))?;
            Ok(())
        }
        Command::Mcp { admin } => {
            let file = read_file_config(&cli)?;
            let admin = admin || file.as_ref().is_some_and(|cfg| cfg.admin);
            let server = cli
                .server
                .clone()
                .or_else(|| std::env::var("IROH_NEWS_SERVER").ok())
                .or_else(|| file.as_ref().and_then(|cfg| cfg.server.clone()))
                .context("set --server, IROH_NEWS_SERVER, or server in the config")?;
            let username = std::env::var("IROH_NEWS_USERNAME")
                .ok()
                .or_else(|| file.as_ref().and_then(|cfg| cfg.username.clone()));
            let password = optional_password(file.as_ref())?;
            let from = std::env::var("IROH_NEWS_FROM")
                .ok()
                .or_else(|| file.as_ref().and_then(|cfg| cfg.from.clone()))
                .unwrap_or_else(|| {
                    username
                        .as_deref()
                        .map(|name| format!("{name}@bots.local"))
                        .unwrap_or_else(|| "bot@bots.local".into())
                });
            let secrets_dir = file
                .as_ref()
                .and_then(|cfg| cfg.secrets_dir.clone())
                .unwrap_or_else(|| state_dir.join("secrets"));
            let discovery = discovery_mode(&cli, file.as_ref())?;
            serve(McpConfig {
                server,
                state_dir: state_dir.clone(),
                secret_key: load_or_create_secret_key(&state_dir)?,
                relays: !cli.no_relays,
                username,
                password,
                from,
                secrets_dir,
                admin,
                discovery,
            })
            .await
        }
        Command::Admin { ref op } => {
            let file = read_file_config(&cli)?;
            let server = cli
                .server
                .clone()
                .or_else(|| std::env::var("IROH_NEWS_SERVER").ok())
                .or_else(|| file.as_ref().and_then(|cfg| cfg.server.clone()))
                .context("set --server, IROH_NEWS_SERVER, or server in the config")?;
            let relays = !cli.no_relays;
            let discovery = discovery_mode(&cli, file.as_ref())?;
            let secrets_dir = file
                .and_then(|cfg| cfg.secrets_dir)
                .unwrap_or_else(|| state_dir.join("secrets"));
            let (op_name, args) = admin_args(op).await?;
            let mut result = admin_call(
                &server,
                &state_dir,
                load_or_create_secret_key(&state_dir)?,
                relays,
                &op_name,
                args,
                discovery,
            )
            .await?;
            stash_password(&mut result, &secrets_dir)?;
            print_json(&result)?;
            Ok(())
        }
        Command::Resolve => {
            let file = read_file_config(&cli)?;
            let server = cli
                .server
                .clone()
                .or_else(|| std::env::var("IROH_NEWS_SERVER").ok())
                .or_else(|| file.as_ref().and_then(|cfg| cfg.server.clone()))
                .unwrap_or_else(|| iroh_news::DEFAULT_JOIN_SERVER.to_string());
            let resolved =
                discover(&server, &state_dir, discovery_mode(&cli, file.as_ref())?).await?;
            let qname = if resolved.source == iroh_news::DiscoverySource::Direct {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(discovery_qname(server.trim()))
            };
            print_json(&serde_json::json!({
                "server": server,
                "qname": qname,
                "endpoint_id": resolved.addr.id.to_string(),
                "source": resolved.source.as_str(),
            }))?;
            Ok(())
        }
        Command::Watch {
            ref groups,
            interval,
            notify_only,
        } => {
            if interval == 0 {
                bail!("--interval must be at least 1 second");
            }
            run_watch(
                load_settings(&cli, &state_dir)?,
                groups,
                interval,
                notify_only,
            )
            .await
        }
    }
}

fn resolve_since(
    session: &Session,
    group: &str,
    since: Option<u64>,
    new: bool,
) -> anyhow::Result<Option<u64>> {
    if let Some(since) = since {
        return Ok(Some(since));
    }
    if new {
        return session.cursor(group);
    }
    Ok(None)
}

async fn open_session(cli: &Cli, state_dir: &Path) -> anyhow::Result<Session> {
    let settings = load_settings(cli, state_dir)?;
    Session::connect(settings).await
}

fn load_settings(cli: &Cli, state_dir: &Path) -> anyhow::Result<Settings> {
    let path = cli.config.clone().unwrap_or(default_config_path()?);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let file: FileConfig =
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
    let discovery = discovery_mode(cli, Some(&file))?;
    let username = std::env::var("IROH_NEWS_USERNAME")
        .ok()
        .or(file.username)
        .context("set IROH_NEWS_USERNAME or username in the config")?;
    let server = cli
        .server
        .clone()
        .or_else(|| std::env::var("IROH_NEWS_SERVER").ok())
        .or(file.server)
        .context("set --server, IROH_NEWS_SERVER, or server in the config")?;
    let password = if let Ok(password) = std::env::var("IROH_NEWS_PASSWORD") {
        password
    } else if let Ok(path) = std::env::var("IROH_NEWS_PASSWORD_FILE") {
        read_password_file(Path::new(&path))?
    } else if let Some(password_file) = &file.password_file {
        read_password_file(password_file)?
    } else {
        file.password.unwrap_or_default()
    };
    let from = std::env::var("IROH_NEWS_FROM")
        .ok()
        .or(file.from)
        .unwrap_or_else(|| format!("{username}@bots.local"));
    Ok(Settings {
        server,
        username,
        password,
        from,
        secret_key: load_or_create_secret_key(state_dir)?,
        relays: !cli.no_relays,
        state_dir: state_dir.to_path_buf(),
        discovery,
    })
}

fn discovery_mode(cli: &Cli, file: Option<&FileConfig>) -> anyhow::Result<DiscoveryMode> {
    let raw = cli
        .discovery
        .clone()
        .or_else(|| std::env::var("IROH_NEWS_DISCOVERY").ok())
        .or_else(|| file.and_then(|cfg| cfg.discovery.clone()));
    match raw.as_deref() {
        None => Ok(DiscoveryMode::Auto),
        Some(value) => DiscoveryMode::parse(value),
    }
}

fn read_password_file(path: &Path) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading password file {}", path.display()))?;
    let trimmed = text.trim_end_matches(['\r', '\n']).to_string();
    if trimmed.is_empty() {
        bail!("password file {} is empty", path.display());
    }
    Ok(trimmed)
}

fn read_file_config(cli: &Cli) -> anyhow::Result<Option<FileConfig>> {
    let path = if let Some(path) = &cli.config {
        path.clone()
    } else {
        default_config_path()?
    };
    if !path.exists() {
        if cli.config.is_some() {
            bail!("config {} does not exist", path.display());
        }
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let file: FileConfig =
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
    Ok(Some(file))
}

fn optional_password(file: Option<&FileConfig>) -> anyhow::Result<Option<String>> {
    if let Ok(password) = std::env::var("IROH_NEWS_PASSWORD") {
        return Ok(Some(password));
    }
    if let Ok(path) = std::env::var("IROH_NEWS_PASSWORD_FILE") {
        return Ok(Some(read_password_file(Path::new(&path))?));
    }
    if let Some(file) = file {
        if let Some(path) = &file.password_file
            && path.exists()
        {
            return Ok(Some(read_password_file(path)?));
        }
        if let Some(password) = &file.password {
            return Ok(Some(password.clone()));
        }
    }
    Ok(None)
}

async fn admin_args(op: &AdminOp) -> anyhow::Result<(String, serde_json::Value)> {
    use serde_json::json;
    Ok(match op {
        AdminOp::Status => ("status".into(), json!({})),
        AdminOp::DiscoveryTxt { name, ticket } => (
            "discovery_txt".into(),
            json!({"name": name, "ticket": ticket}),
        ),
        AdminOp::ListPeers => ("list_peers".into(), json!({})),
        AdminOp::AddPeer { endpoint_id, label } => (
            "add_peer".into(),
            json!({"endpoint_id": endpoint_id, "label": label}),
        ),
        AdminOp::RemovePeer {
            endpoint_id,
            confirm,
        } => (
            "remove_peer".into(),
            json!({"endpoint_id": endpoint_id, "confirm": confirm}),
        ),
        AdminOp::ListAccounts => ("list_accounts".into(), json!({})),
        AdminOp::CreateAccount { username } => {
            ("create_account".into(), json!({"username": username}))
        }
        AdminOp::RotatePassword { username } => {
            ("rotate_password".into(), json!({"username": username}))
        }
        AdminOp::DisableAccount { username, confirm } => (
            "disable_account".into(),
            json!({"username": username, "confirm": confirm}),
        ),
        AdminOp::DeleteAccount { username, confirm } => (
            "delete_account".into(),
            json!({"username": username, "confirm": confirm}),
        ),
        AdminOp::ListGroups => ("list_groups".into(), json!({})),
        AdminOp::CreateGroup {
            name,
            description,
            moderated,
        } => (
            "create_group".into(),
            json!({"name": name, "description": description, "moderated": moderated}),
        ),
        AdminOp::Recent { group, limit } => (
            "recent_articles".into(),
            json!({"group": group, "limit": limit}),
        ),
        AdminOp::DeleteArticle {
            message_id,
            confirm,
        } => (
            "delete_article".into(),
            json!({"message_id": message_id, "confirm": confirm}),
        ),
        AdminOp::RateLimits => ("rate_limits".into(), json!({})),
        AdminOp::DeleteGroup { name, confirm } => (
            "delete_group".into(),
            json!({"name": name, "confirm": confirm}),
        ),
        AdminOp::ListJoinRequests => ("list_join_requests".into(), json!({})),
        AdminOp::ApproveJoinRequest {
            endpoint_id,
            name,
            rename,
        } => (
            "approve_join_request".into(),
            json!({"endpoint_id": endpoint_id, "name": name, "rename": rename}),
        ),
        AdminOp::RejectJoinRequest {
            endpoint_id,
            name,
            confirm,
        } => (
            "reject_join_request".into(),
            json!({"endpoint_id": endpoint_id, "name": name, "confirm": confirm}),
        ),
        AdminOp::ApproveJoin { file } => {
            let text = if let Some(path) = file {
                std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?
            } else {
                let mut buf = String::new();
                tokio::io::AsyncReadExt::read_to_string(&mut tokio::io::stdin(), &mut buf).await?;
                buf
            };
            let block: serde_json::Value =
                serde_json::from_str(&text).context("join block is not JSON")?;
            ("approve_join".into(), json!({"block": block}))
        }
    })
}

fn prompt_username() -> anyhow::Result<String> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        bail!("pass --name <bot-username> ([a-z0-9-])");
    }
    eprint!("bot username ([a-z0-9-]): ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let name = line.trim();
    if name.is_empty() {
        bail!("username is required");
    }
    Ok(name.to_string())
}

fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

fn default_config_path() -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var("IROH_NEWS_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("iroh-news/config.toml"));
    }
    Ok(home()?.join(".config/iroh-news/config.toml"))
}

fn default_state_dir() -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var("IROH_NEWS_STATE") {
        return Ok(PathBuf::from(path));
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        return Ok(PathBuf::from(xdg).join("iroh-news"));
    }
    Ok(home()?.join(".local/state/iroh-news"))
}

fn home() -> anyhow::Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
