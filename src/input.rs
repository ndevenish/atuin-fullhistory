use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::PathBuf;

use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use uuid::Uuid;

use crate::types::History;

/// Initial read window. Aligned down to NFS_ALIGN before seeking.
const TAIL_BYTES: u64 = 128 * 1024;
/// NFS block alignment (typical rsize/wsize is a multiple of this).
const NFS_ALIGN: u64 = 4096;
/// Block size for backwards head reading.
const HEAD_BLOCK: u64 = 1024 * 1024;

pub struct FullHistoryReader {
    path: PathBuf,
}

impl FullHistoryReader {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Read the tail of the file (~TAIL_BYTES, NFS-block-aligned) and return
    /// entries newest-first for immediate display.
    ///
    /// Returns the byte offset at which the tail region starts (i.e. the end
    /// of the head region), or `None` if the whole file was read.
    pub async fn read_tail(&self) -> (Vec<History>, Option<u64>) {
        let mut file = match tokio::fs::File::open(&self.path).await {
            Ok(f) => f,
            Err(_) => return (vec![], None),
        };
        let size = match file.metadata().await {
            Ok(m) => m.len(),
            Err(_) => return (vec![], None),
        };

        if size <= TAIL_BYTES {
            // Small file: read everything.
            let mut content = String::new();
            let _ = file.read_to_string(&mut content).await;
            let mut entries = parse_fullhistory(&content);
            entries.reverse();
            return (entries, None);
        }

        // Align seek offset down to an NFS block boundary.
        let raw_start = size - TAIL_BYTES;
        let start = (raw_start / NFS_ALIGN) * NFS_ALIGN;

        if file.seek(SeekFrom::Start(start)).await.is_err() {
            return (vec![], None);
        }
        let mut buf = Vec::with_capacity((size - start) as usize);
        let _ = file.read_to_end(&mut buf).await;

        // Skip forward to the first complete line (we may have landed mid-line).
        let skip = buf.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
        let tail_offset = start + skip as u64;

        let content = String::from_utf8_lossy(&buf[skip..]).into_owned();
        let mut entries = parse_fullhistory(&content);
        entries.reverse();

        (entries, Some(tail_offset))
    }

    /// Read the head of the file (bytes 0..end_offset) in reverse 1 MB blocks,
    /// returning one parsed block per Vec entry (newest block first).
    ///
    /// Reading backwards means the UI sees entries just before the tail first,
    /// i.e. the most recently used commands appear earliest in the background load.
    ///
    /// Lines that span a block boundary are reassembled via a byte-level carry
    /// buffer. Commands whose ##EXIT## record lands in a different block than
    /// their header will have exit=-1 / duration=-1; this affects at most one
    /// entry per block boundary.
    ///
    /// Runs entirely inside `spawn_blocking` using synchronous `std::fs` I/O so
    /// that slow NFS reads and the subsequent `parse_fullhistory` CPU work are
    /// isolated on one OS thread and cannot starve the async executor.
    pub async fn read_head(&self, end_offset: u64) -> Vec<Vec<History>> {
        if end_offset == 0 {
            return vec![];
        }
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::{Read, Seek, SeekFrom};
            let Ok(mut file) = std::fs::File::open(&path) else {
                return vec![];
            };

            let mut blocks: Vec<Vec<History>> = Vec::new();
            let mut cursor = end_offset;
            // Byte-level carry: the tail of a line that begins in the current
            // block but whose newline terminator sits in the next newer block.
            // It is appended to the end of the current block's content so the
            // line is complete before parsing.
            let mut carry: Vec<u8> = Vec::new();

            while cursor > 0 {
                let block_start = cursor.saturating_sub(HEAD_BLOCK);
                let block_len = (cursor - block_start) as usize;

                if file.seek(SeekFrom::Start(block_start)).is_err() {
                    break;
                }
                let mut buf = vec![0u8; block_len];
                if file.read_exact(&mut buf).is_err() {
                    break;
                }

                // Append carry so the line straddling the boundary is complete.
                buf.extend_from_slice(&carry);

                // Everything before (and including) the first '\n' in buf is the
                // tail end of a line whose head is in the next older block.
                // Save it as the new carry and parse the rest.
                let (new_carry, to_parse) = match buf.iter().position(|&b| b == b'\n') {
                    Some(i) => (buf[..=i].to_vec(), &buf[i + 1..]),
                    None => (buf.clone(), &buf[buf.len()..]), // whole block is one fragment
                };

                let content = String::from_utf8_lossy(to_parse);
                let entries = parse_fullhistory(&content);
                if !entries.is_empty() {
                    blocks.push(entries);
                }

                carry = new_carry;
                cursor = block_start;
            }

            // Parse the carry left after reaching the start of the file.
            if !carry.is_empty() {
                let content = String::from_utf8_lossy(&carry);
                let entries = parse_fullhistory(&content);
                if !entries.is_empty() {
                    blocks.push(entries);
                }
            }

            blocks
        })
        .await
        .unwrap_or_default()
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
    let mut results: Vec<History> = Vec::new();
    // Most recently opened pid, so continuation lines know where to attach.
    let mut last_pid: Option<u32> = None;

    for line in content.lines() {
        if let Some((pid, exit_code, duration_ns)) = parse_exit_line(line) {
            if last_pid == Some(pid) {
                last_pid = None;
            }
            if let Some(mut h) = pending.remove(&pid) {
                h.exit = exit_code;
                h.duration = duration_ns;
                results.push(h);
            }
        } else if let Some((pid, h)) = parse_command_line(line) {
            // Flush any previous command from this pid that never got an EXIT
            if let Some(old) = pending.remove(&pid) {
                results.push(old);
            }
            pending.insert(pid, h);
            last_pid = Some(pid);
        } else if let Some(pid) = last_pid {
            // Continuation line — append to the most recently opened command.
            if let Some(h) = pending.get_mut(&pid) {
                h.command.push('\n');
                h.command.push_str(line);
            }
        }
    }

    // Flush commands that never received an EXIT record
    let mut leftovers: Vec<History> = pending.into_values().collect();
    leftovers.sort_by_key(|h| h.timestamp);
    results.extend(leftovers);

    results.sort_by_key(|h| h.timestamp);
    results
}

/// Parse a command line: `hostname[:"cwd"] pid timestamp command`
/// Returns `(pid, History)` on success.
fn parse_command_line(line: &str) -> Option<(u32, History)> {
    // hostname: [a-zA-Z0-9.-]+
    let hostname_end = line
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-')?;
    let hostname = &line[..hostname_end];
    if hostname.is_empty() {
        return None;
    }
    let rest = &line[hostname_end..];

    // optional :"cwd" (cwd is always double-quoted when present)
    let (cwd, rest) = if rest.starts_with(":\"") {
        let close = rest[2..].find('"')? + 2;
        (rest[2..close].to_string(), rest[close + 1..].trim_start())
    } else {
        (String::new(), rest.trim_start())
    };

    // pid (used directly as session, matching original importer behaviour)
    let sp = rest.find(' ')?;
    let pid: u32 = rest[..sp].parse().ok()?;
    let rest = &rest[sp + 1..];

    // timestamp
    let sp = rest.find(' ')?;
    let ts_str = &rest[..sp];
    let command = rest[sp + 1..].trim().to_string();
    if command.is_empty() {
        return None;
    }

    let timestamp = parse_timestamp(ts_str)?;
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
            session: pid.to_string(),
            hostname: hostname.to_string(),
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

fn parse_timestamp(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &time::format_description::well_known::Iso8601::DEFAULT).ok()
}
