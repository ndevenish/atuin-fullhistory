use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use time::OffsetDateTime;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::local_db::Db;
use crate::types::{
    Context, FilterMode, History, HistoryStats, OptFilters, QueryToken, QueryTokenizer, SearchMode,
};

#[derive(Clone)]
pub struct MemoryDbHandle {
    entries: Arc<RwLock<Vec<History>>>,
}

impl MemoryDbHandle {
    pub async fn append(&self, batch: Vec<History>) {
        let mut entries = self.entries.write().await;
        entries.extend(batch);
    }
}

#[derive(Clone)]
pub struct MemoryDatabase {
    entries: Arc<RwLock<Vec<History>>>,
}

impl MemoryDatabase {
    pub fn new(initial: Vec<History>) -> (Self, MemoryDbHandle) {
        let entries = Arc::new(RwLock::new(initial));
        let db = MemoryDatabase {
            entries: entries.clone(),
        };
        let handle = MemoryDbHandle { entries };
        (db, handle)
    }
}

fn filter_by_mode(h: &History, filter: FilterMode, context: &Context) -> bool {
    let git_root = context
        .git_root
        .as_ref()
        .and_then(|p| p.to_str())
        .unwrap_or(&context.cwd);

    match filter {
        FilterMode::Global => true,
        FilterMode::Host => h
            .hostname
            .split(',')
            .any(|host| host == context.hostname.as_str()),
        FilterMode::Session => h
            .session
            .as_bytes()
            .chunks(32)
            .any(|chunk| chunk == context.session.as_bytes()),
        FilterMode::SessionPreload => {
            let is_current_session = h
                .session
                .as_bytes()
                .chunks(32)
                .any(|chunk| chunk == context.session.as_bytes());
            if is_current_session {
                return true;
            }
            if let Ok(uuid) = Uuid::parse_str(&context.session) {
                if let Some(timestamp) = uuid.get_timestamp() {
                    let (seconds, nanos) = timestamp.to_unix();
                    if let Ok(session_start) = OffsetDateTime::from_unix_timestamp_nanos(
                        i128::from(seconds) * 1_000_000_000 + i128::from(nanos),
                    ) {
                        return h.timestamp < session_start;
                    }
                }
            }
            false
        }
        FilterMode::Directory => h
            .cwd
            .split(':')
            .any(|cwd| cwd == context.cwd.as_str()),
        FilterMode::Workspace => h.cwd.split(':').any(|cwd| cwd == git_root),
    }
}

fn apply_opt_filters(entries: Vec<History>, filter_options: &OptFilters) -> Vec<History> {
    let mut results = entries;

    if let Some(exit) = filter_options.exit {
        results.retain(|h| h.exit == exit);
    }
    if let Some(exclude_exit) = filter_options.exclude_exit {
        results.retain(|h| h.exit != exclude_exit);
    }
    if let Some(ref cwd) = filter_options.cwd {
        results.retain(|h| h.cwd.contains(cwd.as_str()));
    }
    if let Some(ref exclude_cwd) = filter_options.exclude_cwd {
        results.retain(|h| !h.cwd.contains(exclude_cwd.as_str()));
    }
    if let Some(ref before) = filter_options.before {
        if let Ok(dt) = interim::parse_date_string(
            before,
            OffsetDateTime::now_utc(),
            interim::Dialect::Uk,
        ) {
            results.retain(|h| h.timestamp <= dt);
        }
    }
    if let Some(ref after) = filter_options.after {
        if let Ok(dt) = interim::parse_date_string(
            after,
            OffsetDateTime::now_utc(),
            interim::Dialect::Uk,
        ) {
            results.retain(|h| h.timestamp >= dt);
        }
    }

    if filter_options.reverse {
        results.reverse();
    }

    let offset = filter_options.offset.unwrap_or(0) as usize;
    let results: Vec<History> = results.into_iter().skip(offset).collect();

    if let Some(limit) = filter_options.limit {
        results.into_iter().take(limit as usize).collect()
    } else {
        results
    }
}

#[async_trait]
impl Db for MemoryDatabase {
    async fn load(&self, id: &str) -> Result<Option<History>> {
        let entries = self.entries.read().await;
        Ok(entries.iter().find(|h| h.id.0 == id).cloned())
    }

    async fn list(
        &self,
        filters: &[FilterMode],
        context: &Context,
        max: Option<usize>,
        unique: bool,
        include_deleted: bool,
    ) -> Result<Vec<History>> {
        let entries = self.entries.read().await;

        let mut results: Vec<History> = entries
            .iter()
            .filter(|h| {
                if !include_deleted && h.deleted_at.is_some() {
                    return false;
                }
                if filters.is_empty() {
                    return true;
                }
                filters.iter().any(|&f| filter_by_mode(h, f, context))
            })
            .cloned()
            .collect();

        results.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        if unique {
            let mut seen = std::collections::HashSet::new();
            results.retain(|h| seen.insert(h.command.clone()));
        }

        if let Some(max) = max {
            results.truncate(max);
        }

        Ok(results)
    }

    async fn search(
        &self,
        search_mode: SearchMode,
        filter: FilterMode,
        context: &Context,
        query: &str,
        filter_options: OptFilters,
    ) -> Result<Vec<History>> {
        let entries = self.entries.read().await;

        let mut results: Vec<History> = entries
            .iter()
            .filter(|h| {
                if h.deleted_at.is_some() {
                    return false;
                }
                filter_by_mode(h, filter, context)
            })
            .cloned()
            .collect();

        results.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        if !query.is_empty() {
            let tokens: Vec<_> = QueryTokenizer::new(query).collect();
            results.retain(|h| matches_query(&h.command, &tokens, search_mode));
        }

        drop(entries);

        Ok(apply_opt_filters(results, &filter_options))
    }

    async fn all_with_count(&self) -> Result<Vec<(History, i32)>> {
        let entries = self.entries.read().await;

        let mut groups: std::collections::HashMap<String, Vec<&History>> =
            std::collections::HashMap::new();
        for h in entries.iter().filter(|h| h.deleted_at.is_none()) {
            groups.entry(h.command.clone()).or_default().push(h);
        }

        let mut results: Vec<(History, i32)> = groups
            .into_values()
            .map(|group| {
                let count = group.len() as i32;
                let newest = group
                    .iter()
                    .max_by_key(|h| h.timestamp)
                    .copied()
                    .unwrap();

                let cwds: Vec<&str> = group.iter().map(|h| h.cwd.as_str()).collect();
                let sessions: String = group.iter().map(|h| h.session.as_str()).collect();
                let hostnames: Vec<&str> = group.iter().map(|h| h.hostname.as_str()).collect();

                let mut entry = newest.clone();
                entry.cwd = cwds.join(":");
                entry.session = sessions;
                entry.hostname = hostnames.join(",");

                (entry, count)
            })
            .collect();

        results.sort_by(|a, b| b.0.timestamp.cmp(&a.0.timestamp));

        Ok(results)
    }

    async fn history_count(&self, include_deleted: bool) -> Result<i64> {
        let entries = self.entries.read().await;
        let count = if include_deleted {
            entries.len()
        } else {
            entries.iter().filter(|h| h.deleted_at.is_none()).count()
        };
        Ok(count as i64)
    }

    async fn stats(&self, h: &History) -> Result<HistoryStats> {
        let entries = self.entries.read().await;

        let mut session_entries: Vec<&History> = entries
            .iter()
            .filter(|e| e.session == h.session && e.deleted_at.is_none())
            .collect();
        session_entries.sort_by_key(|e| e.timestamp);

        let pos = session_entries.iter().position(|e| e.id == h.id);
        let previous = pos
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| session_entries.get(i))
            .map(|&e| e.clone());
        let next = pos
            .and_then(|i| session_entries.get(i + 1))
            .map(|&e| e.clone());

        let matching: Vec<&History> = entries
            .iter()
            .filter(|e| e.command == h.command && e.deleted_at.is_none())
            .collect();
        let total = matching.len() as u64;

        let durations: Vec<i64> = matching
            .iter()
            .filter(|e| e.duration > 0)
            .map(|e| e.duration)
            .collect();
        let average_duration = if durations.is_empty() {
            0u64
        } else {
            let sum: i64 = durations.iter().sum();
            (sum / durations.len() as i64) as u64
        };

        let mut exit_counts: std::collections::HashMap<i64, i64> =
            std::collections::HashMap::new();
        for e in matching.iter() {
            *exit_counts.entry(e.exit).or_insert(0) += 1;
        }
        let mut exits: Vec<(i64, i64)> = exit_counts.into_iter().collect();
        exits.sort_by_key(|&(k, _)| k);

        let mut dow_counts: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        for e in matching.iter() {
            let dow = (e.timestamp.weekday().number_from_sunday() - 1).to_string();
            *dow_counts.entry(dow).or_insert(0) += 1;
        }
        let mut day_of_week: Vec<(String, i64)> = dow_counts.into_iter().collect();
        day_of_week.sort_by_key(|(k, _)| k.parse::<u8>().unwrap_or(0));

        let mut dot_counts: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        for e in matching.iter() {
            let key = format!(
                "01-{:02}-{:04}",
                e.timestamp.month() as u8,
                e.timestamp.year()
            );
            *dot_counts.entry(key).or_insert(0) += e.duration;
        }
        let mut duration_over_time: Vec<(String, i64)> = dot_counts.into_iter().collect();
        duration_over_time.sort_by_key(|(k, _)| k.clone());

        Ok(HistoryStats {
            next,
            previous,
            total,
            average_duration,
            exits,
            day_of_week,
            duration_over_time,
        })
    }

    async fn delete(&self, h: History) -> Result<()> {
        let mut entries = self.entries.write().await;
        entries.retain(|e| e.id != h.id);
        Ok(())
    }

    fn clone_boxed(&self) -> Box<dyn Db + 'static> {
        Box::new(self.clone())
    }
}

fn matches_query(command: &str, tokens: &[QueryToken<'_>], mode: SearchMode) -> bool {
    match mode {
        SearchMode::Prefix => {
            if let Some(first) = tokens.first() {
                match first {
                    QueryToken::Match(term, false) => command.starts_with(term),
                    _ => true,
                }
            } else {
                true
            }
        }
        SearchMode::FullText
        | SearchMode::Fuzzy
        | SearchMode::Skim
        | SearchMode::DaemonFuzzy => {
            let lower_command = command.to_ascii_lowercase();
            for token in tokens {
                if token.is_inverse() {
                    continue;
                }
                let matchee = if token.has_uppercase() {
                    command
                } else {
                    &lower_command
                };
                let matched = match token {
                    QueryToken::Match(term, _) => matchee.contains(*term),
                    QueryToken::MatchStart(term, _) => matchee.starts_with(*term),
                    QueryToken::MatchEnd(term, _) => matchee.ends_with(*term),
                    QueryToken::MatchFull(term, _) => matchee == *term,
                    QueryToken::Or => true,
                    QueryToken::Regex(r) => {
                        regex::Regex::new(r).map_or(false, |re| re.is_match(command))
                    }
                };
                if !matched {
                    return false;
                }
            }
            true
        }
    }
}
