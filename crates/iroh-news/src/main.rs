//! `iroh-news` — NNTP over iroh for assistant bots.
//!
//! Successful commands print JSON on stdout. Diagnostics go to stderr.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use iroh_news::{
    Identity, JoinRequest, PostRequest, ReadRequest, Session, Settings, load_or_create_secret_key,
    prepare_join,
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
