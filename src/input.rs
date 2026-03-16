use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, BufReader, Stdin};
use uuid::Uuid;

use crate::types::History;

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
