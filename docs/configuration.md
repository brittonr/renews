# Configuration Guide

This document provides comprehensive configuration guidance for Renews NNTP server.

## Configuration File Format

Renews uses TOML format for configuration. The default location is `/etc/renews.toml`, but can be specified with `--config` or the `RENEWS_CONFIG` environment variable.

## Basic Configuration

### Minimal Setup

```toml
# Basic server settings
addr = ":119"                    # Listen address for NNTP
site_name = "news.example.com"   # Server hostname
db_path = "sqlite:///var/lib/renews/news.db"     # Article database
auth_db_path = "sqlite:///var/lib/renews/auth.db" # User database
```

### Complete Example

```toml
# Network settings
addr = ":119"                    # NNTP listen address
site_name = "news.example.com"   # Server hostname

# Database paths
db_path = "sqlite:///var/lib/renews/news.db"
auth_db_path = "sqlite:///var/lib/renews/auth.db"  
peer_db_path = "sqlite:///var/lib/renews/peers.db"

# Connection settings
idle_timeout_secs = 600          # Client idle timeout (10 minutes)
peer_sync_secs = 3600           # Default peer sync interval (1 hour)

# TLS configuration (optional)
tls_addr = ":563"               # NNTPS listen address
tls_cert = "/etc/ssl/news.crt"  # TLS certificate file
tls_key = "/etc/ssl/news.key"   # TLS private key file

# WebSocket bridge (optional, requires websocket feature)
ws_addr = ":8080"               # WebSocket listen address

# Article retention defaults
default_retention_days = 30     # Keep articles for 30 days
default_max_article_bytes = "1M" # 1 megabyte article limit

# Per-group settings
[[group_settings]]
pattern = "announce.*"          # Groups matching this pattern
retention_days = 90             # Keep announcements longer
max_article_bytes = "500K"      # Smaller size limit

[[group_settings]]  
group = "alt.binaries.test"     # Specific group
retention_days = 7              # Short retention for test group
max_article_bytes = "10M"       # Larger files allowed

# Peer servers for synchronization
[[peers]]
sitename = "user:pass@peer1.example.com:119"
patterns = ["comp.*", "misc.*"] # Only sync these hierarchies
sync_interval_secs = 1800       # Sync every 30 minutes

[[peers]]
sitename = "peer2.example.com"  # No authentication required
patterns = ["*"]                # Sync all groups
sync_interval_secs = 7200       # Sync every 2 hours
```

## Configuration Sections

### Network Settings

| Setting | Description | Default |
|---------|-------------|---------|
| `addr` | NNTP listen address | Required |
| `site_name` | Server hostname | `$HOSTNAME` or `localhost` |
| `tls_addr` | NNTPS listen address | None |
| `ws_addr` | WebSocket listen address | None |
| `idle_timeout_secs` | Client connection timeout | 600 |

### Database Settings

| Setting | Description | Default |
|---------|-------------|---------|
| `db_path` | Article database URI | `sqlite:///var/lib/renews/news.db` |
| `auth_db_path` | Authentication database URI | `sqlite:///var/lib/renews/auth.db` |
| `peer_db_path` | Peer state database URI | `sqlite:///var/lib/renews/peers.db` |

#### Database URI Formats

**SQLite:**
```toml
db_path = "sqlite:///absolute/path/to/database.db"
db_path = "sqlite://relative/path/to/database.db"  
```

**PostgreSQL:**
```toml
db_path = "postgres://user:password@localhost/renews"
db_path = "postgres://user@localhost/renews"  # No password
```

### TLS Configuration

All three settings must be provided to enable TLS:

```toml
tls_addr = ":563"                    # Standard NNTPS port
tls_cert = "/path/to/certificate.pem" # PEM format certificate
tls_key = "/path/to/private.key"      # PEM format private key
```

### Security Settings

Control authentication and posting security:

| Setting | Description | Default |
|---------|-------------|---------|
| `allow_auth_insecure_connections` | Allow AUTHINFO on non-TLS connections | `false` |
| `allow_anonymous_posting` | Allow posting without authentication | `false` |

**Security behavior:**

By default, Renews requires TLS for authentication to prevent credential leakage. The server will reject `AUTHINFO` commands on non-TLS connections with response code 483 (Secure connection required).

Posting requires authentication by default. The initial greeting and `MODE READER` response reflect the current posting ability:
- `200` - Posting allowed (authenticated or anonymous posting enabled)
- `201` - Posting not allowed (not authenticated, anonymous posting disabled)

The `CAPABILITIES` response dynamically reflects available features:
- `POST` capability shown only when the session can currently post
- `AUTHINFO USER` capability shown only when authentication is available and user is not yet authenticated

**Example configurations:**

```toml
# Default secure configuration (recommended)
# - Requires TLS for authentication
# - Requires authentication for posting

# Allow authentication without TLS (NOT RECOMMENDED - credentials sent in plaintext)
allow_auth_insecure_connections = true

# Allow anonymous posting (no authentication required)
allow_anonymous_posting = true
```

### Article Retention

Global defaults:
```toml
default_retention_days = 30      # Days to keep articles
default_max_article_bytes = "1M" # Maximum article size
```

Size format supports suffixes: `K` (kilobytes), `M` (megabytes), `G` (gigabytes).

### Group-Specific Rules

Override defaults for specific groups or patterns:

```toml
[[group_settings]]
group = "alt.test"              # Exact group match
retention_days = 7
max_article_bytes = "500K"

[[group_settings]]
pattern = "comp.lang.*"         # Wildcard pattern
retention_days = 90
```

Pattern matching uses wildmat syntax:
- `*` matches any string
- `?` matches any single character  
- `[abc]` matches any character in brackets
- `[!abc]` matches any character not in brackets

### Peer Synchronization

Configure peer servers for article distribution:

```toml
[[peers]]
sitename = "news.example.com:119"    # Hostname and port
patterns = ["*"]                     # Groups to sync (wildmat)
sync_interval_secs = 3600           # Override default interval

[[peers]]
sitename = "user:pass@secure.example.com:563"  # With credentials
patterns = ["comp.*", "!comp.sys.mac.*"]       # Include/exclude patterns
```

#### Peer Patterns

- `["*"]` - Sync all groups
- `["comp.*"]` - Only comp.* hierarchy  
- `["comp.*", "misc.*"]` - Multiple hierarchies
- `["*", "!alt.*"]` - All except alt.* groups

## Variable Substitution

Configuration supports environment variables and file inclusion:

### Environment Variables

```toml
site_name = "$ENV{HOSTNAME}"
db_path = "sqlite:///$ENV{DATA_DIR}/news.db"
```

### File Inclusion

```toml
tls_cert = "$FILE{/etc/ssl/certs/news.crt}"
```

This replaces the value with the contents of the specified file.

## PostgreSQL Backend

To use PostgreSQL instead of SQLite:

1. Build with PostgreSQL support:
   ```bash
   cargo build --release --features postgres
   ```

2. Configure database URI:
   ```toml
   db_path = "postgres://renews:password@localhost/renews"
   auth_db_path = "postgres://renews:password@localhost/renews_auth"
   ```

3. Ensure PostgreSQL server is running and databases exist.

## WebSocket Bridge

For web-based NNTP clients:

1. Build with WebSocket support:
   ```bash
   cargo build --release --features websocket
   ```

2. Configure WebSocket address:
   ```toml
   ws_addr = ":8080"
   ```

Web clients can connect via WebSocket and use NNTP protocol over the connection.

## iroh

Build with `--features iroh`. The `[iroh]` table is ignored by binaries built without that feature (a warning is logged if `enabled` is set).

```toml
addr = "127.0.0.1:119"

[iroh]
enabled = true
key_path = "/var/lib/renews/iroh.key"
localhost_tcp = true
allowed_peers = ["<endpoint id from iroh-news id>"]
```

- `key_path` — 32-byte endpoint secret key. Created on first start, mode `0600`. The endpoint id is stable for the life of this file.
- `allowed_peers` — endpoint ids permitted to open a QUIC connection. Empty rejects every peer. Reloaded on `SIGHUP`.
- `localhost_tcp` — when `enabled` (the default is true), a host-less `addr` such as `:119` binds to `127.0.0.1`. An explicit host (`0.0.0.0:119`, `127.0.0.1:119`) is unchanged. systemd socket URLs are unchanged.

On startup the server prints `iroh endpoint id:` and `iroh ticket:`. Bots put the ticket in `~/.config/iroh-news/config.toml` as `server`. See the README section "Bots over iroh" for the `iroh-news` commands.

## Runtime Configuration Reload

Send `SIGHUP` to reload configuration:

```bash
systemctl reload renews
# or
kill -HUP $(pidof renews)
```

**Reloadable settings:**
- Retention policies
- Group settings  
- TLS certificates
- Peer configurations

**Non-reloadable settings:**
- Listen addresses
- Database paths
- WebSocket settings

## Configuration Validation

Test configuration without starting server:

```bash
renews --config /path/to/config.toml --check
```

Initialize databases:

```bash
renews --config /path/to/config.toml --init
```