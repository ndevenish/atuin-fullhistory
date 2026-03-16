use std::collections::HashMap;
use std::path::PathBuf;

use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, BufReader, Stdin};
use uuid::Uuid;

use crate::types::History;

// ── stdin TSV reader ──────────────────────────────────────────────────────────

pub struct TsvReader {
    reader: BufReader<Stdin>,
}

impl TsvReader {
    pub fn new(stdin: tokio::io::Stdin) -> Self {
        Self {
            reader: BufReader::new(stdin),
        }
    }

    pub async fn read_batch(&mut self, n: usize) -> Vec<History> {
        let mut results = Vec::with_capacity(n);
        let mut line = String::new();

        for _ in 0..n {
            line.clear();
            match self.reader.read_line(&mut line).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Some(entry) = parse_tsv_line(trimmed) {
                        results.push(entry);
                    }
                }
                Err(_) => break,
            }
        }

        results
    }
}

fn parse_tsv_line(line: &str) -> Option<History> {
    let parts: Vec<&str> = line.splitn(7, '\t').collect();
    if parts.len() < 7 {
        return None;
    }

    let timestamp_ns: i128 = parts[0].trim().parse().ok()?;
    let duration_ns: i64 = parts[1].trim().parse().ok()?;
    let exit_code: i64 = parts[2].trim().parse().ok()?;
    let command = parts[3].to_string();
    let cwd = parts[4].to_string();
    let session = parts[5].to_string();
    let hostname = parts[6].trim_end().to_string();

    let timestamp = OffsetDateTime::from_unix_timestamp_nanos(timestamp_ns).ok()?;
    let id = Uuid::new_v4().simple().to_string();

    Some(History {
        id: id.into(),
        timestamp,
        duration: duration_ns,
        exit: exit_code,
        command,
        cwd,
        session,
        hostname,
        author: String::new(),
        intent: None,
        deleted_at: None,
    })
}

// ── fullhistory file reader ───────────────────────────────────────────────────

pub struct FullHistoryReader {
    path: PathBuf,
}

impl FullHistoryReader {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Read and parse the entire file, returning entries in chronological order.
    pub async fn read_all(&self) -> Vec<History> {
        let content = match tokio::fs::read_to_string(&self.path).await {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        parse_fullhistory(&content)
    }
}

/// Parse a fullhistory file into a chronologically ordered list of entries.
///
/// Format:
///   hostname:"cwd" pid YYYY-MM-DDTHH:MM:SS+ZZZZ command
///   ##EXIT## hostname pid=N $?=N t_ms=N
///
/// EXIT lines are correlated with their preceding command by pid.
fn parse_fullhistory(content: &str) -> Vec<History> {
    // pid → pending History (exit/duration not yet filled in)
    let mut pending: HashMap<u32, History> = HashMap::new();
    // pid → stable session UUID (so all commands from same shell share one)
    let mut sessions: HashMap<u32, String> = HashMap::new();
    let mut results: Vec<History> = Vec::new();

    for line in content.lines() {
        if let Some((pid, exit_code, duration_ns)) = parse_exit_line(line) {
            if let Some(mut h) = pending.remove(&pid) {
                h.exit = exit_code;
                h.duration = duration_ns;
                results.push(h);
            }
        } else if let Some((pid, h)) = parse_command_line(line, &mut sessions) {
            // Flush any previous command from this pid that never got an EXIT
            if let Some(old) = pending.remove(&pid) {
                results.push(old);
            }
            pending.insert(pid, h);
        }
        // Other lines (continuations, blank lines, noise) are skipped
    }

    // Flush commands that never received an EXIT record
    let mut leftovers: Vec<History> = pending.into_values().collect();
    leftovers.sort_by_key(|h| h.timestamp);
    results.extend(leftovers);

    results.sort_by_key(|h| h.timestamp);
    results
}

/// Parse a command line: `hostname:"cwd" pid timestamp command`
/// Returns `(pid, History)` on success.
fn parse_command_line(
    line: &str,
    sessions: &mut HashMap<u32, String>,
) -> Option<(u32, History)> {
    // hostname ends at first ':'
    let colon = line.find(':')?;
    let hostname = line[..colon].to_string();
    // hostname must look like a hostname (no spaces)
    if hostname.contains(' ') {
        return None;
    }
    let rest = &line[colon + 1..];

    // cwd is quoted with "…" or unquoted (to first space)
    let (cwd, rest) = if rest.starts_with('"') {
        let close = rest[1..].find('"')? + 1;
        (rest[1..close].to_string(), rest[close + 1..].trim_start())
    } else {
        let sp = rest.find(' ')?;
        (rest[..sp].to_string(), rest[sp + 1..].trim_start())
    };

    // pid
    let sp = rest.find(' ')?;
    let pid: u32 = rest[..sp].parse().ok()?;
    let rest = &rest[sp + 1..];

    // timestamp
    let sp = rest.find(' ')?;
    let ts_str = &rest[..sp];
    let command = rest[sp + 1..].to_string();
    if command.is_empty() {
        return None;
    }

    let timestamp = parse_timestamp(ts_str)?;

    let session = sessions
        .entry(pid)
        .or_insert_with(|| Uuid::new_v4().simple().to_string())
        .clone();

    let id = Uuid::new_v4().simple().to_string();

    Some((
        pid,
        History {
            id: id.into(),
            timestamp,
            duration: -1,
            exit: -1,
            command,
            cwd,
            session,
            hostname,
            author: String::new(),
            intent: None,
            deleted_at: None,
        },
    ))
}

/// Parse an exit line: `##EXIT## hostname pid=N $?=N t_ms=N`
/// Returns `(pid, exit_code, duration_ns)` on success.
fn parse_exit_line(line: &str) -> Option<(u32, i64, i64)> {
    let rest = line.strip_prefix("##EXIT## ")?;

    // skip hostname token
    let sp = rest.find(' ')?;
    let rest = &rest[sp + 1..];

    let rest = rest.strip_prefix("pid=")?;
    let sp = rest.find(' ')?;
    let pid: u32 = rest[..sp].parse().ok()?;
    let rest = &rest[sp + 1..];

    let rest = rest.strip_prefix("$?=")?;
    let sp = rest.find(' ')?;
    let exit_code: i64 = rest[..sp].parse().ok()?;
    let rest = &rest[sp + 1..];

    let dur_str = rest.strip_prefix("t_ms=")?;
    let duration_ms: i64 = dur_str.trim().parse().ok()?;

    Some((pid, exit_code, duration_ms * 1_000_000))
}

/// Parse timestamps in `YYYY-MM-DDTHH:MM:SS+HHMM` or `+HH:MM` offset format.
fn parse_timestamp(s: &str) -> Option<OffsetDateTime> {
    use time::format_description;

    // Try without colon in offset (+0000)
    if let Ok(fmt) = format_description::parse(
        "[year]-[month]-[day]T[hour]:[minute]:[second][offset_hour sign:mandatory][offset_minute]",
    ) {
        if let Ok(dt) = OffsetDateTime::parse(s, &fmt) {
            return Some(dt);
        }
    }

    // Try with colon in offset (+00:00)
    if let Ok(fmt) = format_description::parse(
        "[year]-[month]-[day]T[hour]:[minute]:[second][offset_hour sign:mandatory]:[offset_minute]",
    ) {
        if let Ok(dt) = OffsetDateTime::parse(s, &fmt) {
            return Some(dt);
        }
    }

    None
}
