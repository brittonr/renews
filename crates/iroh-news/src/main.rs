//! `iroh-news` — NNTP over iroh for assistant bots.
//!
//! Successful commands print JSON on stdout. Diagnostics go to stderr.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use iroh_news::mcp::{McpConfig, serve};
use iroh_news::{
    Identity, JoinRequest, PostRequest, ReadRequest, Session, Settings, admin_call,
    load_or_create_secret_key, prepare_join, stash_password,
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
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print this bot's endpoint id as JSON, for the server allowlist.
    Id,
    /// List active newsgroups.
    Groups,
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
    /// Poll a group and print one JSON article per line.
    Watch {
        group: String,
        /// Seconds between polls.
        #[arg(long, default_value_t = 15)]
        interval: u64,
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
    },
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
enum AdminOp {
    Status,
    DiscoveryTxt {
        name: String,
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
        Command::Join { name, owner, json } => {
            let name = match name {
                Some(name) => name,
                None => prompt_username()?,
            };
            let server = cli
                .server
                .clone()
                .unwrap_or_else(|| iroh_news::DEFAULT_JOIN_SERVER.to_string());
            let config_path = cli.config.clone().unwrap_or(default_config_path()?);
            let report = prepare_join(JoinRequest {
                name: &name,
                owner: owner.as_deref(),
                server: &server,
                state_dir: &state_dir,
                config_path: &config_path,
            })?;
            if json {
                print_json(&report)?;
            } else {
                print!("{}", report.human());
            }
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
            )
            .await?;
            stash_password(&mut result, &secrets_dir)?;
            print_json(&result)?;
            Ok(())
        }
        Command::Watch {
            ref group,
            interval,
        } => {
            if interval == 0 {
                bail!("--interval must be at least 1 second");
            }
            let mut session = open_session(&cli, &state_dir).await?;
            loop {
                let since = session.cursor(group)?;
                let articles = session
                    .read(ReadRequest {
                        group: group.clone(),
                        since,
                        limit: None,
                        update_cursor: true,
                    })
                    .await?;
                for article in articles {
                    println!("{}", serde_json::to_string(&article)?);
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
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
        file.password.context(
            "set IROH_NEWS_PASSWORD, IROH_NEWS_PASSWORD_FILE, or password/password_file in the config",
        )?
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
    })
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
        AdminOp::DiscoveryTxt { name } => ("discovery_txt".into(), json!({"name": name})),
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
