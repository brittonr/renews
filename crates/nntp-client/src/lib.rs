//! A small async NNTP client over any [`AsyncRead`] + [`AsyncWrite`] stream.
//!
//! The client speaks the commands a bot needs: `CAPABILITIES`, `AUTHINFO`,
//! `LIST ACTIVE`, `GROUP`, `OVER`/`XOVER`, `ARTICLE`, and `POST`.

use anyhow::{Context, bail};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// One row from `LIST ACTIVE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupEntry {
    pub name: String,
    /// Highest article number, `0` when the group is empty.
    pub high: u64,
    /// Lowest article number, `0` when the group is empty.
    pub low: u64,
    /// Posting status (`y`, `n`, or `m`).
    pub posting: String,
}

/// Result of a successful `GROUP` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupStatus {
    pub count: u64,
    pub low: u64,
    pub high: u64,
    pub name: String,
}

/// One overview record (`OVER` / `XOVER`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Overview {
    pub number: u64,
    pub subject: String,
    pub from: String,
    pub date: String,
    pub message_id: String,
    pub references: String,
}

/// An article returned by `ARTICLE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Article {
    pub number: u64,
    pub message_id: String,
    pub from: String,
    pub subject: String,
    pub date: String,
    pub references: String,
    pub body: String,
}

/// NNTP session on an already connected byte stream.
pub struct NntpClient<R, W> {
    reader: BufReader<R>,
    writer: W,
}

impl<S> NntpClient<tokio::io::ReadHalf<S>, tokio::io::WriteHalf<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Read the greeting from `stream`.
    ///
    /// # Errors
    ///
    /// Returns an error if the greeting is missing or is not `200`/`201`.
    pub async fn connect(stream: S) -> anyhow::Result<Self> {
        let (read, write) = tokio::io::split(stream);
        let mut client = Self {
            reader: BufReader::new(read),
            writer: write,
        };
        let greeting = client.read_line().await?;
        let code = status_code(&greeting)?;
        if code != 200 && code != 201 {
            bail!("unexpected NNTP greeting: {greeting}");
        }
        Ok(client)
    }
}

impl<R, W> NntpClient<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// `CAPABILITIES`. Returns the capability lines, without the terminating dot.
    ///
    /// # Errors
    ///
    /// Returns an error on a non-`101` response or a broken stream.
    pub async fn capabilities(&mut self) -> anyhow::Result<Vec<String>> {
        let status = self.command("CAPABILITIES").await?;
        expect_code(&status, 101)?;
        self.read_dot_lines().await
    }

    /// `AUTHINFO USER` followed by `AUTHINFO PASS`.
    ///
    /// # Errors
    ///
    /// Returns an error if the server rejects the credentials.
    pub async fn authinfo(&mut self, username: &str, password: &str) -> anyhow::Result<()> {
        let status = self.command(&format!("AUTHINFO USER {username}")).await?;
        expect_code(&status, 381).with_context(|| format!("AUTHINFO USER: {status}"))?;
        let status = self.command(&format!("AUTHINFO PASS {password}")).await?;
        expect_code(&status, 281).with_context(|| format!("AUTHINFO PASS: {status}"))?;
        Ok(())
    }

    /// `LIST ACTIVE`.
    ///
    /// # Errors
    ///
    /// Returns an error if the server rejects the command or a row cannot be parsed.
    pub async fn list_active(&mut self) -> anyhow::Result<Vec<GroupEntry>> {
        let status = self.command("LIST ACTIVE").await?;
        expect_code(&status, 215)?;
        let lines = self.read_dot_lines().await?;
        lines.iter().map(|line| parse_active(line)).collect()
    }

    /// `GROUP`.
    ///
    /// # Errors
    ///
    /// Returns an error if the group does not exist or the status line is malformed.
    pub async fn group(&mut self, name: &str) -> anyhow::Result<GroupStatus> {
        let status = self.command(&format!("GROUP {name}")).await?;
        expect_code(&status, 211).with_context(|| format!("GROUP {name}: {status}"))?;
        parse_group(&status)
    }

    /// `OVER`, optionally limited to `range` (for example `1-` or `12-40`).
    ///
    /// An empty range (`423`) is returned as an empty list.
    ///
    /// # Errors
    ///
    /// Returns an error on any other failure response.
    pub async fn over(&mut self, range: Option<&str>) -> anyhow::Result<Vec<Overview>> {
        self.overview("OVER", range).await
    }

    /// `XOVER`. Same records as [`Self::over`].
    ///
    /// # Errors
    ///
    /// Returns an error on a failure response other than an empty range.
    pub async fn xover(&mut self, range: Option<&str>) -> anyhow::Result<Vec<Overview>> {
        self.overview("XOVER", range).await
    }

    /// `ARTICLE` by number or message-id (`<id@host>`).
    ///
    /// # Errors
    ///
    /// Returns an error if the article is missing or the response is truncated.
    pub async fn article(&mut self, id: &str) -> anyhow::Result<Article> {
        let status = self.command(&format!("ARTICLE {id}")).await?;
        expect_code(&status, 220).with_context(|| format!("ARTICLE {id}: {status}"))?;
        parse_article(&status, self).await
    }

    /// `POST` a complete article, including the terminating `.\r\n`.
    ///
    /// `article` must already use CRLF and dot-stuffing.
    ///
    /// # Errors
    ///
    /// Returns an error if the server refuses the article.
    pub async fn post(&mut self, article: &str) -> anyhow::Result<()> {
        let status = self.command("POST").await?;
        expect_code(&status, 340).with_context(|| format!("POST: {status}"))?;
        self.writer.write_all(article.as_bytes()).await?;
        self.writer.flush().await?;
        let status = self.read_line().await?;
        expect_code(&status, 240).with_context(|| format!("POST body: {status}"))?;
        Ok(())
    }

    /// `QUIT`.
    ///
    /// # Errors
    ///
    /// Returns an error if the server does not acknowledge the close.
    pub async fn quit(&mut self) -> anyhow::Result<()> {
        let status = self.command("QUIT").await?;
        expect_code(&status, 205)?;
        Ok(())
    }

    async fn overview(
        &mut self,
        command: &str,
        range: Option<&str>,
    ) -> anyhow::Result<Vec<Overview>> {
        let line = match range {
            Some(range) if !range.is_empty() => format!("{command} {range}"),
            _ => command.to_string(),
        };
        let status = self.command(&line).await?;
        let code = status_code(&status)?;
        if code == 423 {
            return Ok(Vec::new());
        }
        expect_code(&status, 224).with_context(|| format!("{command}: {status}"))?;
        let lines = self.read_dot_lines().await?;
        lines.iter().map(|line| parse_overview(line)).collect()
    }

    async fn command(&mut self, line: &str) -> anyhow::Result<String> {
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.write_all(b"\r\n").await?;
        // QUIC send streams hold data until flushed. TCP is unaffected.
        self.writer.flush().await?;
        self.read_line().await
    }

    async fn read_line(&mut self) -> anyhow::Result<String> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            bail!("connection closed");
        }
        if line.ends_with('\n') {
            line.pop();
        }
        if line.ends_with('\r') {
            line.pop();
        }
        Ok(line)
    }

    async fn read_dot_lines(&mut self) -> anyhow::Result<Vec<String>> {
        let mut lines = Vec::new();
        loop {
            let line = self.read_line().await?;
            if line == "." {
                break;
            }
            let line = line.strip_prefix('.').unwrap_or(&line);
            lines.push(line.to_string());
        }
        Ok(lines)
    }
}

fn status_code(line: &str) -> anyhow::Result<u16> {
    let code = line.get(..3).context("short NNTP response")?;
    code.parse()
        .with_context(|| format!("response code in '{line}'"))
}

fn expect_code(line: &str, expected: u16) -> anyhow::Result<()> {
    let code = status_code(line)?;
    if code != expected {
        bail!("expected {expected}, got {line}");
    }
    Ok(())
}

fn parse_active(line: &str) -> anyhow::Result<GroupEntry> {
    let mut parts = line.split_whitespace();
    let name = parts
        .next()
        .context("LIST ACTIVE row missing group")?
        .to_string();
    let high = parts
        .next()
        .context("LIST ACTIVE row missing high")?
        .parse()?;
    let low = parts
        .next()
        .context("LIST ACTIVE row missing low")?
        .parse()?;
    let posting = parts.next().unwrap_or("y").to_string();
    Ok(GroupEntry {
        name,
        high,
        low,
        posting,
    })
}

fn parse_group(line: &str) -> anyhow::Result<GroupStatus> {
    let mut parts = line.split_whitespace();
    let _code = parts.next();
    let count = parts.next().context("GROUP missing count")?.parse()?;
    let low = parts.next().context("GROUP missing low")?.parse()?;
    let high = parts.next().context("GROUP missing high")?.parse()?;
    let name = parts.next().context("GROUP missing name")?.to_string();
    Ok(GroupStatus {
        count,
        low,
        high,
        name,
    })
}

fn parse_overview(line: &str) -> anyhow::Result<Overview> {
    let mut fields = line.split('\t');
    let number = fields
        .next()
        .context("overview missing article number")?
        .parse()?;
    let subject = fields.next().unwrap_or("").to_string();
    let from = fields.next().unwrap_or("").to_string();
    let date = fields.next().unwrap_or("").to_string();
    let message_id = fields.next().unwrap_or("").to_string();
    let references = fields.next().unwrap_or("").to_string();
    Ok(Overview {
        number,
        subject,
        from,
        date,
        message_id,
        references,
    })
}

async fn parse_article<R, W>(status: &str, client: &mut NntpClient<R, W>) -> anyhow::Result<Article>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut parts = status.split_whitespace();
    let _code = parts.next();
    let number = parts
        .next()
        .context("ARTICLE status missing number")?
        .parse()?;
    let status_id = parts.next().unwrap_or("").to_string();

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let line = client.read_line().await?;
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = headers.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    let body_lines = client.read_dot_lines().await?;
    let mut body = body_lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }

    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    let message_id = {
        let from_header = header("Message-ID");
        if from_header.is_empty() {
            status_id
        } else {
            from_header
        }
    };

    Ok(Article {
        number,
        message_id,
        from: header("From"),
        subject: header("Subject"),
        date: header("Date"),
        references: header("References"),
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capabilities_auth_and_post_round_trip_on_a_duplex() {
        let (client_side, mut server) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            server
                .write_all(b"201 NNTP Service Ready - no posting allowed\r\n")
                .await
                .unwrap();
            server.flush().await.unwrap();

            let mut buf = String::new();
            read_until(&mut server, &mut buf, "CAPABILITIES\r\n").await;
            server
                .write_all(b"101 Capability list follows\r\nVERSION 2\r\nREADER\r\n.\r\n")
                .await
                .unwrap();
            server.flush().await.unwrap();

            read_until(&mut server, &mut buf, "AUTHINFO USER bot\r\n").await;
            server
                .write_all(b"381 password required\r\n")
                .await
                .unwrap();
            server.flush().await.unwrap();
            read_until(&mut server, &mut buf, "AUTHINFO PASS secret\r\n").await;
            server
                .write_all(b"281 authentication accepted\r\n")
                .await
                .unwrap();
            server.flush().await.unwrap();

            read_until(&mut server, &mut buf, "POST\r\n").await;
            server
                .write_all(b"340 send article to be posted. End with <CR-LF>.<CR-LF>\r\n")
                .await
                .unwrap();
            server.flush().await.unwrap();
            read_until(&mut server, &mut buf, "\r\n.\r\n").await;
            server.write_all(b"240 article received\r\n").await.unwrap();
            server.flush().await.unwrap();
        });

        let mut client = NntpClient::connect(client_side).await.unwrap();
        let caps = client.capabilities().await.unwrap();
        assert_eq!(caps, vec!["VERSION 2".to_string(), "READER".to_string()]);
        client.authinfo("bot", "secret").await.unwrap();
        client
            .post("Subject: hi\r\n\r\nbody\r\n.\r\n")
            .await
            .unwrap();
        server_task.await.unwrap();
    }

    async fn read_until(stream: &mut tokio::io::DuplexStream, buf: &mut String, needle: &str) {
        use tokio::io::AsyncReadExt;
        let mut tmp = [0u8; 256];
        while !buf.contains(needle) {
            let n = stream.read(&mut tmp).await.unwrap();
            assert!(n > 0, "eof waiting for {needle} in {buf:?}");
            buf.push_str(&String::from_utf8_lossy(&tmp[..n]));
        }
    }
}
