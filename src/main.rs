use anyhow::Result;

use clap::{Parser, Subcommand};
use tokio::runtime::Runtime;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use renews::auth;
use renews::config::{Config, DEFAULT_LOG_FILTER, parse_duration_secs, parse_size};
use renews::limits::UserLimits;
use renews::server;
use renews::storage;

#[derive(Parser)]
struct Args {
    /// Path to the configuration file
    #[arg(long, env = "RENEWS_CONFIG", default_value = "/etc/renews.toml")]
    config: String,
    /// Initialize databases and exit
    #[arg(long)]
    init: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Administrative actions
    #[command(subcommand)]
    Admin(AdminCommand),
}

#[derive(Subcommand)]
enum AdminCommand {
    /// Add newsgroups
    AddGroup {
        /// First group name
        group: String,
        /// Additional group names
        #[arg(required = false)]
        groups: Vec<String>,
    },
    /// Remove newsgroups matching a wildmat pattern
    RemoveGroup {
        /// Wildmat pattern for groups to remove
        wildmat: String,
    },
    /// Add a user with optional PGP key
    AddUser {
        user: String,
        /// Password. Omit when `--password-file` is set.
        #[arg(required_unless_present = "password_file")]
        pass: Option<String>,
        /// Read the password from a file. The trailing newline is stripped.
        /// The file is not copied anywhere; use this for agenix/sops paths.
        #[arg(long)]
        password_file: Option<std::path::PathBuf>,
        /// Optional PGP public key
        #[arg(long)]
        pgp_key: Option<String>,
    },
    /// Update user password
    UpdatePassword {
        user: String,
        /// New password. Omit when `--password-file` is set.
        #[arg(required_unless_present = "password_file")]
        new_pass: Option<String>,
        /// Read the new password from a file. The trailing newline is stripped.
        #[arg(long)]
        password_file: Option<std::path::PathBuf>,
    },
    /// Remove a user
    RemoveUser { user: String },
    /// Print the DNS TXT record for this server's current iroh ticket.
    ///
    /// The name is the public DNS name, for example `slime.onix.computer`.
    /// Publish `_iroh-news.<name>` in the onix.computer zone.
    DiscoveryTxt {
        /// Public name, without the `_iroh-news` label.
        name: String,
    },
    /// Update user's PGP key
    UpdateKey { user: String, pgp_key: String },
    /// Set moderation status for a group
    SetModerated { group: String, moderated: String },
    /// Grant admin privileges to a user
    AddAdmin { user: String },
    /// Revoke admin privileges from a user
    RemoveAdmin { user: String },
    /// Add a moderator for a group
    AddModerator { user: String, group: String },
    /// Remove a moderator for a group
    RemoveModerator { user: String, group: String },
    /// Set per-user limits (posting permission, bandwidth, connections)
    SetLimits {
        /// Username to set limits for
        user: String,
        /// Allow/disallow posting (true/false)
        #[arg(long)]
        allow_posting: Option<String>,
        /// Max simultaneous connections (0 = unlimited)
        #[arg(long)]
        max_connections: Option<u32>,
        /// Bandwidth limit (e.g., "10G", "500M", 0 = unlimited)
        #[arg(long)]
        bandwidth_limit: Option<String>,
        /// Bandwidth period (e.g., "30d", "1w", empty = absolute/lifetime)
        #[arg(long)]
        bandwidth_period: Option<String>,
    },
    /// Clear per-user limits (revert to defaults)
    ClearLimits {
        /// Username to clear limits for
        user: String,
    },
    /// Show current usage for a user
    ShowUsage {
        /// Username to show usage for
        user: String,
    },
    /// Reset usage counters for a user
    ResetUsage {
        /// Username to reset usage for
        user: String,
    },
    /// Import newsgroups from a file (ISC format: group<whitespace>description). Use '-' for stdin.
    ImportGroups {
        /// Path to the newsgroups file, or '-' for stdin
        file: std::path::PathBuf,
    },
    /// Export newsgroups to stdout (ISC format: group<tab>description)
    ExportGroups,
}

/// Import newsgroups from a file in ISC format (group<whitespace>description).
///
/// Lines starting with '#' are treated as comments.
/// Empty lines are skipped.
/// If the description ends with "(Moderated)" (case-insensitive), the group
/// is marked as moderated and the suffix is stripped from the description.
/// If file is "-", reads from stdin.
async fn import_groups(storage: &storage::DynStorage, file: &std::path::Path) -> Result<()> {
    use std::io::BufRead;

    let reader: Box<dyn BufRead> = if file.as_os_str() == "-" {
        Box::new(std::io::BufReader::new(std::io::stdin()))
    } else {
        let file_handle = std::fs::File::open(file)
            .map_err(|e| anyhow::anyhow!("Failed to open file '{}': {}", file.display(), e))?;
        Box::new(std::io::BufReader::new(file_handle))
    };

    let mut imported = 0u64;
    let mut skipped = 0u64;

    for (line_num, line_result) in reader.lines().enumerate() {
        let line = line_result
            .map_err(|e| anyhow::anyhow!("Failed to read line {}: {}", line_num + 1, e))?;

        // Trim trailing whitespace/newlines
        let line = line.trim_end();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Split on first whitespace to get group name and description
        let (group_name, description) = if let Some(pos) = line.find([' ', '\t']) {
            let name = line[..pos].trim();
            let desc = line[pos..].trim();
            (name, desc)
        } else {
            // No whitespace - entire line is the group name, empty description
            (line.trim(), "")
        };

        // Validate group name
        if group_name.is_empty() {
            eprintln!("Warning: skipping line {}: empty group name", line_num + 1);
            skipped += 1;
            continue;
        }

        // Check for (Moderated) suffix (case-insensitive)
        let (description, moderated) = if description.to_lowercase().ends_with("(moderated)") {
            let desc = description[..description.len() - 11].trim_end();
            (desc, true)
        } else {
            (description, false)
        };

        // Add or update the group
        storage
            .add_group_with_description(group_name, moderated, description)
            .await?;
        imported += 1;
    }

    println!("Imported {imported} groups, skipped {skipped} lines");
    Ok(())
}

/// Export newsgroups to stdout in ISC format (group<tab>description).
///
/// For moderated groups, appends "(Moderated)" to the description if not already present.
/// Groups with empty descriptions are output as just the group name.
async fn export_groups(storage: &storage::DynStorage) -> Result<()> {
    use futures_util::StreamExt;

    let mut groups_stream = storage.list_groups_with_descriptions();
    while let Some(result) = groups_stream.next().await {
        let (group, description) = result?;
        let moderated = storage.is_group_moderated(&group).await?;

        let line = if description.is_empty() {
            if moderated {
                format!("{group}\t(Moderated)")
            } else {
                group
            }
        } else if moderated && !description.to_lowercase().ends_with("(moderated)") {
            format!("{group}\t{description} (Moderated)")
        } else {
            format!("{group}\t{description}")
        };

        println!("{line}");
    }

    Ok(())
}

/// Prefer a password file over a command-line value so secrets stay out of the Nix store.
fn read_secret(value: Option<String>, file: Option<std::path::PathBuf>) -> Result<String> {
    if let Some(path) = file {
        let text = std::fs::read_to_string(&path).map_err(|err| {
            anyhow::anyhow!("failed to read password file {}: {err}", path.display())
        })?;
        let trimmed = text.trim_end_matches(['\r', '\n']).to_string();
        if trimmed.is_empty() {
            return Err(anyhow::anyhow!("password file {} is empty", path.display()));
        }
        return Ok(trimmed);
    }
    value.ok_or_else(|| anyhow::anyhow!("password is required"))
}

async fn run_admin(cmd: AdminCommand, cfg: &Config) -> Result<()> {
    let storage = storage::open(&cfg.db_path).await?;
    let auth = auth::open(&cfg.auth_db_path).await?;
    match cmd {
        AdminCommand::AddGroup { group, groups } => {
            // Add the first group
            storage.add_group(&group, false).await?;
            // Add any additional groups
            for g in groups {
                storage.add_group(&g, false).await?;
            }
        }
        AdminCommand::RemoveGroup { wildmat } => {
            storage.remove_groups_by_pattern(&wildmat).await?;
        }
        AdminCommand::AddUser {
            user,
            pass,
            password_file,
            pgp_key,
        } => {
            let pass = read_secret(pass, password_file)?;
            auth.add_user_with_key(&user, &pass, pgp_key.as_deref())
                .await?;
        }
        AdminCommand::UpdatePassword {
            user,
            new_pass,
            password_file,
        } => {
            let new_pass = read_secret(new_pass, password_file)?;
            auth.update_password(&user, &new_pass).await?;
        }
        AdminCommand::RemoveUser { user } => {
            auth.remove_user(&user).await?;
        }
        AdminCommand::DiscoveryTxt { name } => {
            let path = std::path::Path::new(&cfg.iroh.key_path).with_extension("ticket");
            let ticket = std::fs::read_to_string(&path).map_err(|err| {
                anyhow::anyhow!(
                    "failed to read {} ({err}). Start the server once so it can write the ticket.",
                    path.display()
                )
            })?;
            let ticket = ticket.trim();
            if !ticket.starts_with("endpoint") {
                return Err(anyhow::anyhow!(
                    "{} does not contain an endpoint ticket",
                    path.display()
                ));
            }
            println!("_iroh-news.{name}. IN TXT \"ticket={ticket}\"");
            println!("ticket={ticket}");
        }
        AdminCommand::UpdateKey { user, pgp_key } => {
            auth.update_pgp_key(&user, &pgp_key).await?;
        }
        AdminCommand::SetModerated { group, moderated } => {
            let is_moderated = match moderated.to_lowercase().as_str() {
                "true" | "yes" | "1" => true,
                "false" | "no" | "0" => false,
                _ => {
                    return Err(anyhow::anyhow!(
                        "Invalid boolean value: '{moderated}'. Use 'true' or 'false'."
                    ));
                }
            };
            storage.set_group_moderated(&group, is_moderated).await?;
        }
        AdminCommand::AddAdmin { user } => {
            auth.add_admin_without_key(&user).await?;
        }
        AdminCommand::RemoveAdmin { user } => {
            auth.remove_admin(&user).await?;
        }
        AdminCommand::AddModerator { user, group } => {
            auth.add_moderator(&user, &group).await?;
        }
        AdminCommand::RemoveModerator { user, group } => {
            auth.remove_moderator(&user, &group).await?;
        }
        AdminCommand::SetLimits {
            user,
            allow_posting,
            max_connections,
            bandwidth_limit,
            bandwidth_period,
        } => {
            // Get existing limits or create new ones
            let existing = auth.get_user_limits(&user).await?.unwrap_or_default();

            // Parse allow_posting
            let can_post = if let Some(ref val) = allow_posting {
                match val.to_lowercase().as_str() {
                    "true" | "yes" | "1" => true,
                    "false" | "no" | "0" => false,
                    _ => {
                        return Err(anyhow::anyhow!(
                            "Invalid boolean value: '{val}'. Use 'true' or 'false'."
                        ));
                    }
                }
            } else {
                existing.can_post
            };

            // Parse max_connections (0 = unlimited)
            let max_conn = max_connections
                .map(|c| if c == 0 { None } else { Some(c) })
                .unwrap_or(existing.max_connections);

            // Parse bandwidth_limit
            let bw_limit = if let Some(ref val) = bandwidth_limit {
                if val == "0" || val.is_empty() {
                    None
                } else {
                    parse_size(val)
                }
            } else {
                existing.bandwidth_limit
            };

            // Parse bandwidth_period
            let bw_period = if let Some(ref val) = bandwidth_period {
                parse_duration_secs(val)
            } else {
                existing.bandwidth_period_secs
            };

            let limits = UserLimits {
                can_post,
                max_connections: max_conn,
                bandwidth_limit: bw_limit,
                bandwidth_period_secs: bw_period,
            };

            auth.set_user_limits(&user, &limits).await?;
            println!("Limits set for user '{user}':");
            println!("  can_post: {can_post}");
            println!(
                "  max_connections: {}",
                max_conn.map_or("unlimited".to_string(), |c| c.to_string())
            );
            println!(
                "  bandwidth_limit: {}",
                bw_limit.map_or("unlimited".to_string(), format_bytes)
            );
            println!(
                "  bandwidth_period: {}",
                bw_period.map_or("absolute".to_string(), format_duration)
            );
        }
        AdminCommand::ClearLimits { user } => {
            auth.clear_user_limits(&user).await?;
            println!("Limits cleared for user '{user}' (will use defaults)");
        }
        AdminCommand::ShowUsage { user } => {
            let usage = auth.get_user_usage(&user).await?;
            println!("Usage for user '{user}':");
            println!("  uploaded: {}", format_bytes(usage.bytes_uploaded));
            println!("  downloaded: {}", format_bytes(usage.bytes_downloaded));
            println!("  total: {}", format_bytes(usage.total_bandwidth()));
            if let Some(ws) = usage.window_start {
                println!("  window_start: {ws}");
            } else {
                println!("  window_start: (not set)");
            }
        }
        AdminCommand::ResetUsage { user } => {
            auth.reset_user_usage(&user).await?;
            println!("Usage counters reset for user '{user}'");
        }
        AdminCommand::ImportGroups { file } => {
            import_groups(&storage, &file).await?;
        }
        AdminCommand::ExportGroups => {
            export_groups(&storage).await?;
        }
    }
    Ok(())
}

/// Format bytes into a human-readable string.
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

/// Format seconds into a human-readable duration string.
fn format_duration(secs: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;

    if secs >= WEEK && secs % WEEK == 0 {
        format!("{} week(s)", secs / WEEK)
    } else if secs >= DAY && secs % DAY == 0 {
        format!("{} day(s)", secs / DAY)
    } else if secs >= HOUR && secs % HOUR == 0 {
        format!("{} hour(s)", secs / HOUR)
    } else if secs >= MINUTE && secs % MINUTE == 0 {
        format!("{} minute(s)", secs / MINUTE)
    } else {
        format!("{secs} second(s)")
    }
}

async fn run_init(cfg: &Config) -> Result<()> {
    storage::open(&cfg.db_path).await?;
    auth::open(&cfg.auth_db_path).await?;
    let peer_db = renews::peers::PeerDb::new(&cfg.peer_db_path).await?;
    let names: Vec<String> = cfg.peers.iter().map(|p| p.sitename.clone()).collect();
    peer_db.sync_config(&names).await?;
    Ok(())
}

/// Initialize the tracing subscriber based on configuration.
///
/// Priority for log level: config file > RUST_LOG env var > default
/// Priority for log format: config file (default: "json")
fn init_tracing(config: &Config) {
    // Determine log level filter
    let filter = config
        .logging
        .level
        .clone()
        .or_else(|| std::env::var("RUST_LOG").ok())
        .unwrap_or_else(|| DEFAULT_LOG_FILTER.to_string());

    let env_filter = EnvFilter::new(&filter);

    // Build subscriber based on configured format
    if config.logging.format == "json" {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer())
            .init();
    }

    tracing::info!(
        format = %config.logging.format,
        filter = %filter,
        "Logging initialized"
    );
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<()> {
    // Parse args first to get config path
    let args = Args::parse();
    let cfg_path = args.config.clone();

    // Load configuration
    let cfg_initial = match Config::from_file(&cfg_path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    // Initialize tracing based on configuration
    init_tracing(&cfg_initial);

    // Initialize systemd socket support
    if let Err(e) = systemd_socket::init() {
        tracing::warn!(error = %e, "Failed to initialize systemd socket support");
    };

    // Create the runtime based on the configuration
    let runtime_threads = match cfg_initial.get_runtime_threads() {
        Ok(threads) => threads,
        Err(e) => {
            eprintln!("Error determining runtime threads: {e}");
            std::process::exit(1);
        }
    };

    let runtime = if runtime_threads == 1 {
        Runtime::new()?
    } else {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(runtime_threads)
            .enable_all()
            .build()?
    };

    runtime.block_on(async {
        if args.init {
            if let Err(e) = run_init(&cfg_initial).await {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
            return Ok(());
        }

        if let Some(cmd) = args.command {
            match cmd {
                Command::Admin(c) => {
                    if let Err(e) = run_admin(c, &cfg_initial).await {
                        eprintln!("Error: {e}");
                        std::process::exit(1);
                    }
                    return Ok(());
                }
            }
        }

        if let Err(e) = server::run(cfg_initial, cfg_path).await {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }

        Ok(())
    })
}
