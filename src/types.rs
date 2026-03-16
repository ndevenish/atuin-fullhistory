// Inlined types from atuin-client, atuin-common, and atuin-history.
// This allows atuin-tui to build as a standalone crate without workspace deps.

use std::{borrow::Cow, collections::HashMap, fmt, io::prelude::*, path::PathBuf, str::FromStr};

use clap::ValueEnum;
use config::{
    Config, ConfigBuilder, Environment, File as ConfigFile, FileFormat, builder::DefaultState,
};
use crossterm::style::{Attribute, Attributes, Color, ContentStyle};
use eyre::{Context as EyreContext, Error, Result, bail, eyre};
use regex::RegexSet;
use serde::{Deserialize, Serialize};
use serde_with::DeserializeFromStr;
use std::sync::LazyLock;
use strum_macros;
use time::{OffsetDateTime, UtcOffset, format_description::FormatItem, macros::format_description};

// ---------------------------------------------------------------------------
// Section 1: History types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct HistoryId(pub String);

impl fmt::Display for HistoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for HistoryId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    pub id: HistoryId,
    pub timestamp: OffsetDateTime,
    pub duration: i64,
    pub exit: i64,
    pub command: String,
    pub cwd: String,
    pub session: String,
    pub hostname: String,
    pub author: String,
    pub intent: Option<String>,
    pub deleted_at: Option<OffsetDateTime>,
}

impl History {
    pub fn success(&self) -> bool {
        self.exit == 0 || self.duration == -1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryStats {
    pub next: Option<History>,
    pub previous: Option<History>,
    pub total: u64,
    pub average_duration: u64,
    pub exits: Vec<(i64, i64)>,
    pub day_of_week: Vec<(String, i64)>,
    pub duration_over_time: Vec<(String, i64)>,
}

// ---------------------------------------------------------------------------
// Section 2: DB query types
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Context {
    pub session: String,
    pub cwd: String,
    pub hostname: String,
    pub host_id: String,
    pub git_root: Option<PathBuf>,
}

impl Context {
    pub fn from_history(entry: &History) -> Self {
        Context {
            session: entry.session.to_string(),
            cwd: entry.cwd.to_string(),
            hostname: entry.hostname.to_string(),
            host_id: String::new(),
            git_root: in_git_repo(entry.cwd.as_str()),
        }
    }
}

#[derive(Default, Clone)]
pub struct OptFilters {
    pub exit: Option<i64>,
    pub exclude_exit: Option<i64>,
    pub cwd: Option<String>,
    pub exclude_cwd: Option<String>,
    pub before: Option<String>,
    pub after: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub reverse: bool,
    pub include_duplicates: bool,
}

pub struct QueryTokenizer<'a> {
    query: &'a str,
    last_pos: usize,
}

pub enum QueryToken<'a> {
    Match(&'a str, bool),
    MatchStart(&'a str, bool),
    MatchEnd(&'a str, bool),
    MatchFull(&'a str, bool),
    Or,
    Regex(&'a str),
}

impl<'a> QueryToken<'a> {
    pub fn has_uppercase(&self) -> bool {
        match self {
            Self::Match(term, _)
            | Self::MatchStart(term, _)
            | Self::MatchEnd(term, _)
            | Self::MatchFull(term, _) => term.contains(char::is_uppercase),
            _ => false,
        }
    }

    pub fn is_inverse(&self) -> bool {
        match self {
            Self::Match(_, inv)
            | Self::MatchStart(_, inv)
            | Self::MatchEnd(_, inv)
            | Self::MatchFull(_, inv) => *inv,
            _ => false,
        }
    }
}

impl<'a> QueryTokenizer<'a> {
    pub fn new(query: &'a str) -> Self {
        Self { query, last_pos: 0 }
    }
}

impl<'a> Iterator for QueryTokenizer<'a> {
    type Item = QueryToken<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let remaining = &self.query[self.last_pos..];
        if remaining.is_empty() {
            return None;
        }

        if let Some(remaining) = remaining.strip_prefix("r/") {
            let (regex, next_pos) = if let Some(end) = remaining.find("/ ") {
                (&remaining[..end], self.last_pos + 2 + end + 2)
            } else if let Some(remaining) = remaining.strip_suffix('/') {
                (remaining, self.query.len())
            } else {
                (remaining, self.query.len())
            };
            self.last_pos = next_pos;
            Some(QueryToken::Regex(regex))
        } else {
            let (mut part, next_pos) = if let Some(sp) = remaining.find(' ') {
                (&remaining[..sp], self.last_pos + sp + 1)
            } else {
                (remaining, self.query.len())
            };
            self.last_pos = next_pos;

            if part == "|" {
                return Some(QueryToken::Or);
            }

            let mut is_inverse = false;
            if let Some(s) = part.strip_prefix('!') {
                part = s;
                is_inverse = true;
            }
            let token = if let Some(s) = part.strip_prefix('^') {
                QueryToken::MatchStart(s, is_inverse)
            } else if let Some(s) = part.strip_suffix('$') {
                QueryToken::MatchEnd(s, is_inverse)
            } else if let Some(s) = part.strip_prefix('\'') {
                QueryToken::MatchFull(s, is_inverse)
            } else {
                QueryToken::Match(part, is_inverse)
            };
            Some(token)
        }
    }
}

// ---------------------------------------------------------------------------
// Section 3: Settings types
// ---------------------------------------------------------------------------

pub const HISTORY_PAGE_SIZE: i64 = 100;
static EXAMPLE_CONFIG: &str = "";

#[derive(Clone, Debug, Deserialize, Copy, ValueEnum, PartialEq, Serialize)]
pub enum SearchMode {
    #[serde(rename = "prefix")]
    Prefix,

    #[serde(rename = "fulltext")]
    #[clap(aliases = &["fulltext"])]
    FullText,

    #[serde(rename = "fuzzy")]
    Fuzzy,

    #[serde(rename = "skim")]
    Skim,

    #[serde(rename = "daemon-fuzzy")]
    #[clap(aliases = &["daemon-fuzzy"])]
    DaemonFuzzy,
}

impl SearchMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SearchMode::Prefix => "PREFIX",
            SearchMode::FullText => "FULLTXT",
            SearchMode::Fuzzy => "FUZZY",
            SearchMode::Skim => "SKIM",
            SearchMode::DaemonFuzzy => "DAEMON",
        }
    }
    pub fn next(&self, settings: &Settings) -> Self {
        match self {
            SearchMode::Prefix => SearchMode::FullText,
            SearchMode::FullText if settings.search_mode == SearchMode::Skim => SearchMode::Skim,
            SearchMode::FullText if settings.search_mode == SearchMode::DaemonFuzzy => {
                SearchMode::DaemonFuzzy
            }
            SearchMode::FullText => SearchMode::Fuzzy,
            SearchMode::Fuzzy | SearchMode::Skim | SearchMode::DaemonFuzzy => SearchMode::Prefix,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Copy, PartialEq, Eq, ValueEnum, Serialize)]
pub enum FilterMode {
    #[serde(rename = "global")]
    Global = 0,

    #[serde(rename = "host")]
    Host = 1,

    #[serde(rename = "session")]
    Session = 2,

    #[serde(rename = "directory")]
    Directory = 3,

    #[serde(rename = "workspace")]
    Workspace = 4,

    #[serde(rename = "session-preload")]
    SessionPreload = 5,
}

impl FilterMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            FilterMode::Global => "GLOBAL",
            FilterMode::Host => "HOST",
            FilterMode::Session => "SESSION",
            FilterMode::Directory => "DIRECTORY",
            FilterMode::Workspace => "WORKSPACE",
            FilterMode::SessionPreload => "SESSION+",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Copy, Serialize)]
pub enum ExitMode {
    #[serde(rename = "return-original")]
    ReturnOriginal,

    #[serde(rename = "return-query")]
    ReturnQuery,
}

#[derive(Clone, Debug, Deserialize, Copy, Serialize)]
pub enum Dialect {
    #[serde(rename = "us")]
    Us,

    #[serde(rename = "uk")]
    Uk,
}

impl From<Dialect> for interim::Dialect {
    fn from(d: Dialect) -> interim::Dialect {
        match d {
            Dialect::Uk => interim::Dialect::Uk,
            Dialect::Us => interim::Dialect::Us,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, DeserializeFromStr, Serialize)]
pub struct Timezone(pub UtcOffset);
impl fmt::Display for Timezone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
static OFFSET_FMT: &[FormatItem<'_>] = format_description!(
    "[offset_hour sign:mandatory padding:none][optional [:[offset_minute padding:none][optional [:[offset_second padding:none]]]]]"
);
impl FromStr for Timezone {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        if matches!(s.to_lowercase().as_str(), "l" | "local") {
            let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
            return Ok(Self(offset));
        }

        if matches!(s.to_lowercase().as_str(), "0" | "utc") {
            let offset = UtcOffset::UTC;
            return Ok(Self(offset));
        }

        if let Ok(offset) = UtcOffset::parse(s, OFFSET_FMT) {
            return Ok(Self(offset));
        }

        bail!(r#""{s}" is not a valid timezone spec"#)
    }
}

#[derive(Clone, Debug, Deserialize, Copy, Serialize)]
pub enum Style {
    #[serde(rename = "auto")]
    Auto,

    #[serde(rename = "full")]
    Full,

    #[serde(rename = "compact")]
    Compact,
}

#[derive(Clone, Debug, Deserialize, Copy, Serialize)]
pub enum WordJumpMode {
    #[serde(rename = "emacs")]
    Emacs,

    #[serde(rename = "subl")]
    Subl,
}

#[derive(Clone, Debug, Deserialize, Copy, PartialEq, Eq, ValueEnum, Serialize)]
pub enum KeymapMode {
    #[serde(rename = "emacs")]
    Emacs,

    #[serde(rename = "vim-normal")]
    VimNormal,

    #[serde(rename = "vim-insert")]
    VimInsert,

    #[serde(rename = "auto")]
    Auto,
}

impl KeymapMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            KeymapMode::Emacs => "EMACS",
            KeymapMode::VimNormal => "VIMNORMAL",
            KeymapMode::VimInsert => "VIMINSERT",
            KeymapMode::Auto => "AUTO",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Copy, PartialEq, Eq, ValueEnum, Serialize)]
pub enum CursorStyle {
    #[serde(rename = "default")]
    DefaultUserShape,

    #[serde(rename = "blink-block")]
    BlinkingBlock,

    #[serde(rename = "steady-block")]
    SteadyBlock,

    #[serde(rename = "blink-underline")]
    BlinkingUnderScore,

    #[serde(rename = "steady-underline")]
    SteadyUnderScore,

    #[serde(rename = "blink-bar")]
    BlinkingBar,

    #[serde(rename = "steady-bar")]
    SteadyBar,
}

impl CursorStyle {
    pub fn as_str(&self) -> &'static str {
        match self {
            CursorStyle::DefaultUserShape => "DEFAULT",
            CursorStyle::BlinkingBlock => "BLINKBLOCK",
            CursorStyle::SteadyBlock => "STEADYBLOCK",
            CursorStyle::BlinkingUnderScore => "BLINKUNDERLINE",
            CursorStyle::SteadyUnderScore => "STEADYUNDERLINE",
            CursorStyle::BlinkingBar => "BLINKBAR",
            CursorStyle::SteadyBar => "STEADYBAR",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Stats {
    #[serde(default = "Stats::common_prefix_default")]
    pub common_prefix: Vec<String>,
    #[serde(default = "Stats::common_subcommands_default")]
    pub common_subcommands: Vec<String>,
    #[serde(default = "Stats::ignored_commands_default")]
    pub ignored_commands: Vec<String>,
}

impl Stats {
    fn common_prefix_default() -> Vec<String> {
        vec!["sudo", "doas"].into_iter().map(String::from).collect()
    }

    fn common_subcommands_default() -> Vec<String> {
        vec![
            "apt", "cargo", "composer", "dnf", "docker", "dotnet", "git", "go", "ip", "jj",
            "kubectl", "nix", "nmcli", "npm", "pecl", "pnpm", "podman", "port", "systemctl",
            "tmux", "yarn",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    fn ignored_commands_default() -> Vec<String> {
        vec![]
    }
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            common_prefix: Self::common_prefix_default(),
            common_subcommands: Self::common_subcommands_default(),
            ignored_commands: Self::ignored_commands_default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Default, Serialize)]
pub struct Sync {
    pub records: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SyncProtocol {
    Hub,
    Legacy,
    #[default]
    Auto,
}

#[derive(Clone, Debug, Deserialize, Default, Serialize)]
pub struct Keys {
    pub scroll_exits: bool,
    pub exit_past_line_start: bool,
    pub accept_past_line_end: bool,
    pub accept_past_line_start: bool,
    pub accept_with_backspace: bool,
    pub prefix: String,
}

impl Keys {
    pub fn standard_defaults() -> Self {
        Keys {
            scroll_exits: true,
            exit_past_line_start: true,
            accept_past_line_end: true,
            accept_past_line_start: false,
            accept_with_backspace: false,
            prefix: "a".to_string(),
        }
    }

    pub fn has_non_default_values(&self) -> bool {
        let d = Self::standard_defaults();
        self.scroll_exits != d.scroll_exits
            || self.exit_past_line_start != d.exit_past_line_start
            || self.accept_past_line_end != d.accept_past_line_end
            || self.accept_past_line_start != d.accept_past_line_start
            || self.accept_with_backspace != d.accept_with_backspace
            || self.prefix != d.prefix
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct KeyRuleConfig {
    #[serde(default)]
    pub when: Option<String>,
    pub action: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum KeyBindingConfig {
    Simple(String),
    Rules(Vec<KeyRuleConfig>),
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct KeymapConfig {
    #[serde(default)]
    pub emacs: HashMap<String, KeyBindingConfig>,
    #[serde(default, rename = "vim-normal")]
    pub vim_normal: HashMap<String, KeyBindingConfig>,
    #[serde(default, rename = "vim-insert")]
    pub vim_insert: HashMap<String, KeyBindingConfig>,
    #[serde(default)]
    pub inspector: HashMap<String, KeyBindingConfig>,
    #[serde(default)]
    pub prefix: HashMap<String, KeyBindingConfig>,
}

impl KeymapConfig {
    pub fn is_empty(&self) -> bool {
        self.emacs.is_empty()
            && self.vim_normal.is_empty()
            && self.vim_insert.is_empty()
            && self.inspector.is_empty()
            && self.prefix.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Preview {
    pub strategy: PreviewStrategy,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ThemeSettings {
    pub name: String,
    pub debug: Option<bool>,
    pub max_depth: Option<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Daemon {
    #[serde(alias = "enable")]
    pub enabled: bool,
    pub autostart: bool,
    pub sync_frequency: u64,
    pub socket_path: String,
    pub pidfile_path: String,
    pub systemd_socket: bool,
    pub tcp_port: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Search {
    pub filters: Vec<FilterMode>,
    pub recency_score_multiplier: f64,
    pub frequency_score_multiplier: f64,
    pub frecency_score_multiplier: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Tmux {
    pub enabled: bool,
    pub width: String,
    pub height: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_directive(&self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct LogConfig {
    pub file: String,
    pub enabled: Option<bool>,
    pub level: Option<LogLevel>,
    pub retention: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Logs {
    #[serde(default = "Logs::default_enabled")]
    pub enabled: bool,
    pub dir: String,
    #[serde(default)]
    pub level: LogLevel,
    #[serde(default = "Logs::default_retention")]
    pub retention: u64,
    #[serde(default)]
    pub search: LogConfig,
    #[serde(default)]
    pub daemon: LogConfig,
    #[serde(default)]
    pub ai: LogConfig,
}

#[derive(Default, Clone, Debug, Deserialize, Serialize)]
pub struct Ai {
    pub enabled: bool,
    pub endpoint: Option<String>,
    pub api_token: Option<String>,
    pub send_cwd: bool,
}

impl Default for Preview {
    fn default() -> Self {
        Self {
            strategy: PreviewStrategy::Auto,
        }
    }
}

impl Default for ThemeSettings {
    fn default() -> Self {
        Self {
            name: "".to_string(),
            debug: None::<bool>,
            max_depth: Some(10),
        }
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self {
            enabled: false,
            autostart: false,
            sync_frequency: 300,
            socket_path: "".to_string(),
            pidfile_path: "".to_string(),
            systemd_socket: false,
            tcp_port: 8889,
        }
    }
}

impl Default for Logs {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: "".to_string(),
            level: LogLevel::default(),
            retention: Self::default_retention(),
            search: LogConfig {
                file: "search.log".to_string(),
                ..Default::default()
            },
            daemon: LogConfig {
                file: "daemon.log".to_string(),
                ..Default::default()
            },
            ai: LogConfig {
                file: "ai.log".to_string(),
                ..Default::default()
            },
        }
    }
}

impl Logs {
    fn default_enabled() -> bool {
        true
    }

    fn default_retention() -> u64 {
        4
    }

    pub fn search_enabled(&self) -> bool {
        self.search.enabled.unwrap_or(self.enabled)
    }

    pub fn daemon_enabled(&self) -> bool {
        self.daemon.enabled.unwrap_or(self.enabled)
    }

    pub fn ai_enabled(&self) -> bool {
        self.ai.enabled.unwrap_or(self.enabled)
    }

    pub fn search_level(&self) -> LogLevel {
        self.search.level.unwrap_or(self.level)
    }

    pub fn daemon_level(&self) -> LogLevel {
        self.daemon.level.unwrap_or(self.level)
    }

    pub fn ai_level(&self) -> LogLevel {
        self.ai.level.unwrap_or(self.level)
    }

    pub fn search_retention(&self) -> u64 {
        self.search.retention.unwrap_or(self.retention)
    }

    pub fn daemon_retention(&self) -> u64 {
        self.daemon.retention.unwrap_or(self.retention)
    }

    pub fn ai_retention(&self) -> u64 {
        self.ai.retention.unwrap_or(self.retention)
    }

    pub fn search_path(&self) -> PathBuf {
        let path = PathBuf::from(&self.search.file);
        PathBuf::from(&self.dir).join(path)
    }

    pub fn daemon_path(&self) -> PathBuf {
        let path = PathBuf::from(&self.daemon.file);
        PathBuf::from(&self.dir).join(path)
    }

    pub fn ai_path(&self) -> PathBuf {
        let path = PathBuf::from(&self.ai.file);
        PathBuf::from(&self.dir).join(path)
    }
}

impl Default for Search {
    fn default() -> Self {
        Self {
            filters: vec![
                FilterMode::Global,
                FilterMode::Host,
                FilterMode::Session,
                FilterMode::SessionPreload,
                FilterMode::Workspace,
                FilterMode::Directory,
            ],
            recency_score_multiplier: 1.0,
            frequency_score_multiplier: 1.0,
            frecency_score_multiplier: 1.0,
        }
    }
}

impl Default for Tmux {
    fn default() -> Self {
        Self {
            enabled: false,
            width: "80%".to_string(),
            height: "60%".to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Copy, PartialEq, Eq, ValueEnum, Serialize)]
pub enum PreviewStrategy {
    #[serde(rename = "auto")]
    Auto,

    #[serde(rename = "static")]
    Static,

    #[serde(rename = "fixed")]
    Fixed,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UiColumnType {
    Duration,
    Time,
    Datetime,
    Directory,
    Host,
    User,
    Exit,
    Command,
}

impl UiColumnType {
    pub fn default_width(&self) -> u16 {
        match self {
            UiColumnType::Duration => 5,
            UiColumnType::Time => 9,
            UiColumnType::Datetime => 16,
            UiColumnType::Directory => 20,
            UiColumnType::Host => 15,
            UiColumnType::User => 10,
            UiColumnType::Exit => {
                if cfg!(windows) {
                    11
                } else {
                    3
                }
            }
            UiColumnType::Command => 0,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct UiColumn {
    pub column_type: UiColumnType,
    pub width: u16,
    pub expand: bool,
}

impl UiColumn {
    pub fn new(column_type: UiColumnType) -> Self {
        Self {
            width: column_type.default_width(),
            expand: column_type == UiColumnType::Command,
            column_type,
        }
    }

    pub fn with_width(column_type: UiColumnType, width: u16) -> Self {
        Self {
            column_type,
            width,
            expand: column_type == UiColumnType::Command,
        }
    }
}

impl<'de> serde::Deserialize<'de> for UiColumn {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{self, MapAccess, Visitor};

        struct UiColumnVisitor;

        impl<'de> Visitor<'de> for UiColumnVisitor {
            type Value = UiColumn;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str(
                    "a column type string or an object with 'type' and optional 'width'/'expand'",
                )
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<UiColumn, E>
            where
                E: de::Error,
            {
                let column_type: UiColumnType =
                    serde::Deserialize::deserialize(serde::de::value::StrDeserializer::new(value))?;
                Ok(UiColumn::new(column_type))
            }

            fn visit_map<M>(self, mut map: M) -> std::result::Result<UiColumn, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut column_type: Option<UiColumnType> = None;
                let mut width: Option<u16> = None;
                let mut expand: Option<bool> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => {
                            column_type = Some(map.next_value()?);
                        }
                        "width" => {
                            width = Some(map.next_value()?);
                        }
                        "expand" => {
                            expand = Some(map.next_value()?);
                        }
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let column_type = column_type.ok_or_else(|| de::Error::missing_field("type"))?;
                let width = width.unwrap_or_else(|| column_type.default_width());
                let expand = expand.unwrap_or(column_type == UiColumnType::Command);
                Ok(UiColumn {
                    column_type,
                    width,
                    expand,
                })
            }
        }

        deserializer.deserialize_any(UiColumnVisitor)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Ui {
    #[serde(default = "Ui::default_columns")]
    pub columns: Vec<UiColumn>,
}

impl Ui {
    fn default_columns() -> Vec<UiColumn> {
        vec![
            UiColumn::new(UiColumnType::Duration),
            UiColumn::new(UiColumnType::Time),
            UiColumn::new(UiColumnType::Command),
        ]
    }

    pub fn validate(&self) -> Result<()> {
        let expand_count = self.columns.iter().filter(|c| c.expand).count();
        if expand_count > 1 {
            bail!(
                "Only one column can have expand = true, but {} columns are set to expand",
                expand_count
            );
        }
        Ok(())
    }
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            columns: Self::default_columns(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct DotfilesSettings {
    #[serde(alias = "enable")]
    pub enabled: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KvSettings {
    pub db_path: String,
}
impl Default for KvSettings {
    fn default() -> Self {
        Self {
            db_path: data_dir().join("kv.db").to_string_lossy().to_string(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ScriptsSettings {
    pub db_path: String,
}
impl Default for ScriptsSettings {
    fn default() -> Self {
        Self {
            db_path: data_dir().join("scripts.db").to_string_lossy().to_string(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MetaSettings {
    pub db_path: String,
}
impl Default for MetaSettings {
    fn default() -> Self {
        Self {
            db_path: data_dir().join("meta.db").to_string_lossy().to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Settings {
    pub data_dir: Option<String>,
    pub dialect: Dialect,
    pub timezone: Timezone,
    pub style: Style,
    pub auto_sync: bool,
    pub update_check: bool,
    pub sync_address: String,
    #[serde(default)]
    pub sync_protocol: SyncProtocol,
    pub sync_frequency: String,
    pub db_path: String,
    pub record_store_path: String,
    pub key_path: String,
    pub search_mode: SearchMode,
    pub filter_mode: Option<FilterMode>,
    pub filter_mode_shell_up_key_binding: Option<FilterMode>,
    pub search_mode_shell_up_key_binding: Option<SearchMode>,
    pub shell_up_key_binding: bool,
    pub inline_height: u16,
    pub inline_height_shell_up_key_binding: Option<u16>,
    pub invert: bool,
    pub show_preview: bool,
    pub max_preview_height: u16,
    pub show_help: bool,
    pub show_tabs: bool,
    pub show_numeric_shortcuts: bool,
    pub auto_hide_height: u16,
    pub exit_mode: ExitMode,
    pub keymap_mode: KeymapMode,
    pub keymap_mode_shell: KeymapMode,
    pub keymap_cursor: HashMap<String, CursorStyle>,
    pub word_jump_mode: WordJumpMode,
    pub word_chars: String,
    pub scroll_context_lines: usize,
    pub history_format: String,
    pub prefers_reduced_motion: bool,
    pub store_failed: bool,

    #[serde(with = "serde_regex", default = "RegexSet::empty", skip_serializing)]
    pub history_filter: RegexSet,

    #[serde(with = "serde_regex", default = "RegexSet::empty", skip_serializing)]
    pub cwd_filter: RegexSet,

    pub secrets_filter: bool,
    pub workspaces: bool,
    pub ctrl_n_shortcuts: bool,

    pub network_connect_timeout: u64,
    pub network_timeout: u64,
    pub local_timeout: f64,
    pub enter_accept: bool,
    pub smart_sort: bool,
    pub command_chaining: bool,

    #[serde(default)]
    pub stats: Stats,

    #[serde(default)]
    pub sync: Sync,

    #[serde(default)]
    pub keys: Keys,

    #[serde(default)]
    pub keymap: KeymapConfig,

    #[serde(default)]
    pub preview: Preview,

    #[serde(default)]
    pub dotfiles: DotfilesSettings,

    #[serde(default)]
    pub daemon: Daemon,

    #[serde(default)]
    pub search: Search,

    #[serde(default)]
    pub theme: ThemeSettings,

    #[serde(default)]
    pub ui: Ui,

    #[serde(default)]
    pub scripts: ScriptsSettings,

    #[serde(default)]
    pub kv: KvSettings,

    #[serde(default)]
    pub tmux: Tmux,

    #[serde(default)]
    pub logs: Logs,

    #[serde(default)]
    pub meta: MetaSettings,

    #[serde(default)]
    pub ai: Ai,
}

impl Settings {
    pub fn utc() -> Self {
        Self::builder_with_data_dir(&data_dir())
            .expect("Could not build default")
            .set_override("timezone", "0")
            .expect("failed to override timezone with UTC")
            .build()
            .expect("Could not build config")
            .try_deserialize()
            .expect("Could not deserialize config")
    }

    fn builder_with_data_dir(data_dir_path: &std::path::Path) -> Result<ConfigBuilder<DefaultState>> {
        let db_path = data_dir_path.join("history.db");
        let record_store_path = data_dir_path.join("records.db");
        let kv_path = data_dir_path.join("kv.db");
        let scripts_path = data_dir_path.join("scripts.db");
        let socket_path = runtime_dir().join("atuin.sock");
        let pidfile_path = data_dir_path.join("atuin-daemon.pid");
        let logs_dir_path = logs_dir();

        let key_path = data_dir_path.join("key");
        let meta_path = data_dir_path.join("meta.db");

        Ok(Config::builder()
            .set_default("history_format", "{time}\t{command}\t{duration}")?
            .set_default("db_path", db_path.to_str())?
            .set_default("record_store_path", record_store_path.to_str())?
            .set_default("key_path", key_path.to_str())?
            .set_default("dialect", "us")?
            .set_default("timezone", "local")?
            .set_default("auto_sync", true)?
            .set_default("update_check", false)?
            .set_default("sync_address", "https://api.atuin.sh")?
            .set_default("sync_frequency", "5m")?
            .set_default("search_mode", "fuzzy")?
            .set_default("filter_mode", None::<String>)?
            .set_default("style", "compact")?
            .set_default("inline_height", 40)?
            .set_default("show_preview", true)?
            .set_default("preview.strategy", "auto")?
            .set_default("max_preview_height", 4)?
            .set_default("show_help", true)?
            .set_default("show_tabs", true)?
            .set_default("show_numeric_shortcuts", true)?
            .set_default("auto_hide_height", 8)?
            .set_default("invert", false)?
            .set_default("exit_mode", "return-original")?
            .set_default("word_jump_mode", "emacs")?
            .set_default(
                "word_chars",
                "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
            )?
            .set_default("scroll_context_lines", 1)?
            .set_default("shell_up_key_binding", false)?
            .set_default("workspaces", false)?
            .set_default("ctrl_n_shortcuts", false)?
            .set_default("secrets_filter", true)?
            .set_default("network_connect_timeout", 5)?
            .set_default("network_timeout", 30)?
            .set_default("local_timeout", 2.0)?
            .set_default("enter_accept", false)?
            .set_default("sync.records", true)?
            .set_default("keys.scroll_exits", true)?
            .set_default("keys.accept_past_line_end", true)?
            .set_default("keys.exit_past_line_start", true)?
            .set_default("keys.accept_past_line_start", false)?
            .set_default("keys.accept_with_backspace", false)?
            .set_default("keys.prefix", "a")?
            .set_default("keymap_mode", "emacs")?
            .set_default("keymap_mode_shell", "auto")?
            .set_default("keymap_cursor", HashMap::<String, String>::new())?
            .set_default("smart_sort", false)?
            .set_default("command_chaining", false)?
            .set_default("store_failed", true)?
            .set_default("daemon.sync_frequency", 300)?
            .set_default("daemon.enabled", false)?
            .set_default("daemon.autostart", false)?
            .set_default("daemon.socket_path", socket_path.to_str())?
            .set_default("daemon.pidfile_path", pidfile_path.to_str())?
            .set_default("daemon.systemd_socket", false)?
            .set_default("daemon.tcp_port", 8889)?
            .set_default("logs.enabled", true)?
            .set_default("logs.dir", logs_dir_path.to_str())?
            .set_default("logs.level", "info")?
            .set_default("logs.search.file", "search.log")?
            .set_default("logs.daemon.file", "daemon.log")?
            .set_default("logs.ai.file", "ai.log")?
            .set_default("kv.db_path", kv_path.to_str())?
            .set_default("scripts.db_path", scripts_path.to_str())?
            .set_default("search.recency_score_multiplier", 1.0)?
            .set_default("search.frequency_score_multiplier", 1.0)?
            .set_default("search.frecency_score_multiplier", 1.0)?
            .set_default("meta.db_path", meta_path.to_str())?
            .set_default("ai.enabled", false)?
            .set_default("ai.send_cwd", false)?
            .set_default(
                "search.filters",
                vec![
                    "global",
                    "host",
                    "session",
                    "workspace",
                    "directory",
                    "session-preload",
                ],
            )?
            .set_default("theme.name", "default")?
            .set_default("theme.debug", None::<bool>)?
            .set_default("tmux.enabled", false)?
            .set_default("tmux.width", "80%")?
            .set_default("tmux.height", "60%")?
            .set_default(
                "prefers_reduced_motion",
                std::env::var("NO_MOTION")
                    .ok()
                    .map(|_| config::Value::new(None, config::ValueKind::Boolean(true)))
                    .unwrap_or_else(|| config::Value::new(None, config::ValueKind::Boolean(false))),
            )?
            .add_source(
                Environment::with_prefix("atuin")
                    .prefix_separator("_")
                    .separator("__"),
            ))
    }

    pub fn get_config_path() -> Result<PathBuf> {
        let cfg_dir = config_dir();

        std::fs::create_dir_all(&cfg_dir)
            .wrap_err_with(|| format!("could not create dir {cfg_dir:?}"))?;

        let mut config_file = if let Ok(p) = std::env::var("ATUIN_CONFIG_DIR") {
            PathBuf::from(p)
        } else {
            let mut config_file = PathBuf::new();
            config_file.push(cfg_dir);
            config_file
        };

        config_file.push("config.toml");

        Ok(config_file)
    }

    pub fn default_filter_mode(&self, git_root: bool) -> FilterMode {
        self.filter_mode
            .filter(|x| self.search.filters.contains(x))
            .or_else(|| {
                self.search
                    .filters
                    .iter()
                    .find(|x| match (x, git_root, self.workspaces) {
                        (FilterMode::Workspace, true, true) => true,
                        (FilterMode::Workspace, _, _) => false,
                        (_, _, _) => true,
                    })
                    .copied()
            })
            .unwrap_or(FilterMode::Global)
    }

    pub fn expand_path(path: String) -> Result<String> {
        shellexpand::full(&path)
            .map(|p| p.to_string())
            .map_err(|e| eyre!("failed to expand path: {}", e))
    }

    pub fn example_config() -> &'static str {
        EXAMPLE_CONFIG
    }

    pub fn paths_ok(&self) -> bool {
        let paths = [
            &self.db_path,
            &self.record_store_path,
            &self.key_path,
            &self.meta.db_path,
        ];
        paths.iter().all(|p| !broken_symlink(p))
    }

    pub fn new() -> Result<Self> {
        let config_file = Self::get_config_path()?;

        let effective_data_dir = if config_file.exists() {
            #[derive(Deserialize, Default)]
            struct DataDirOnly {
                data_dir: Option<String>,
            }

            let config_file_str = config_file
                .to_str()
                .ok_or_else(|| eyre!("config file path is not valid UTF-8"))?;

            let partial_config = Config::builder()
                .add_source(ConfigFile::new(config_file_str, FileFormat::Toml))
                .add_source(
                    Environment::with_prefix("atuin")
                        .prefix_separator("_")
                        .separator("__"),
                )
                .build()
                .ok();

            let custom_data_dir = partial_config
                .and_then(|c| c.try_deserialize::<DataDirOnly>().ok())
                .and_then(|d| d.data_dir);

            match custom_data_dir {
                Some(dir) => {
                    let expanded = shellexpand::full(&dir)
                        .map_err(|e| eyre!("failed to expand data_dir path: {}", e))?;
                    PathBuf::from(expanded.as_ref())
                }
                None => data_dir(),
            }
        } else {
            data_dir()
        };

        std::fs::create_dir_all(&effective_data_dir)
            .wrap_err_with(|| format!("could not create dir {effective_data_dir:?}"))?;

        let mut config_builder = Self::builder_with_data_dir(&effective_data_dir)?;

        config_builder = if config_file.exists() {
            let config_file_str = config_file
                .to_str()
                .ok_or_else(|| eyre!("config file path is not valid UTF-8"))?;
            config_builder.add_source(ConfigFile::new(config_file_str, FileFormat::Toml))
        } else {
            let mut file =
                std::fs::File::create(config_file).wrap_err("could not create config file")?;
            file.write_all(EXAMPLE_CONFIG.as_bytes())
                .wrap_err("could not write default config file")?;

            config_builder
        };

        let config = config_builder.build()?;
        let mut settings: Settings = config
            .try_deserialize()
            .map_err(|e| eyre!("failed to deserialize: {}", e))?;

        settings.db_path = Self::expand_path(settings.db_path)?;
        settings.record_store_path = Self::expand_path(settings.record_store_path)?;
        settings.key_path = Self::expand_path(settings.key_path)?;
        settings.daemon.socket_path = Self::expand_path(settings.daemon.socket_path)?;
        settings.daemon.pidfile_path = Self::expand_path(settings.daemon.pidfile_path)?;
        settings.logs.dir = Self::expand_path(settings.logs.dir)?;
        settings.logs.search.file = Self::expand_path(settings.logs.search.file)?;
        settings.logs.daemon.file = Self::expand_path(settings.logs.daemon.file)?;

        settings.ui.validate()?;

        Ok(settings)
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::builder_with_data_dir(&data_dir())
            .expect("Could not build default")
            .build()
            .expect("Could not build config")
            .try_deserialize()
            .expect("Could not deserialize config")
    }
}

// ---------------------------------------------------------------------------
// Section 4: Theme types
// ---------------------------------------------------------------------------

static DEFAULT_MAX_DEPTH: u8 = 10;

#[derive(
    Serialize, Deserialize, Copy, Clone, Hash, Debug, Eq, PartialEq, strum_macros::Display,
)]
#[strum(serialize_all = "camel_case")]
pub enum Meaning {
    AlertInfo,
    AlertWarn,
    AlertError,
    Annotation,
    Base,
    Guidance,
    Important,
    Title,
    Muted,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ThemeConfig {
    pub theme: ThemeDefinitionConfigBlock,
    pub colors: HashMap<Meaning, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ThemeDefinitionConfigBlock {
    pub name: String,
    pub parent: Option<String>,
}

pub struct Theme {
    pub name: String,
    pub parent: Option<String>,
    pub styles: HashMap<Meaning, ContentStyle>,
}

impl Theme {
    pub fn get_base(&self) -> ContentStyle {
        self.styles[&Meaning::Base]
    }

    pub fn get_info(&self) -> ContentStyle {
        self.get_alert(log::Level::Info)
    }

    pub fn get_warning(&self) -> ContentStyle {
        self.get_alert(log::Level::Warn)
    }

    pub fn get_error(&self) -> ContentStyle {
        self.get_alert(log::Level::Error)
    }

    pub fn get_alert(&self, severity: log::Level) -> ContentStyle {
        self.styles[ALERT_TYPES.get(&severity).unwrap()]
    }

    pub fn new(
        name: String,
        parent: Option<String>,
        styles: HashMap<Meaning, ContentStyle>,
    ) -> Theme {
        Theme {
            name,
            parent,
            styles,
        }
    }

    pub fn closest_meaning<'a>(&self, meaning: &'a Meaning) -> &'a Meaning {
        if self.styles.contains_key(meaning) {
            meaning
        } else if MEANING_FALLBACKS.contains_key(meaning) {
            self.closest_meaning(&MEANING_FALLBACKS[meaning])
        } else {
            &Meaning::Base
        }
    }

    pub fn as_style(&self, meaning: Meaning) -> ContentStyle {
        self.styles[self.closest_meaning(&meaning)]
    }

    pub fn from_foreground_colors(
        name: String,
        parent: Option<&Theme>,
        foreground_colors: HashMap<Meaning, String>,
        debug: bool,
    ) -> Theme {
        let styles: HashMap<Meaning, ContentStyle> = foreground_colors
            .iter()
            .map(|(name, color)| {
                (
                    *name,
                    StyleFactory::from_fg_string(color).unwrap_or_else(|err| {
                        if debug {
                            log::warn!("Tried to load string as a color unsuccessfully: ({name}={color}) {err}");
                        }
                        ContentStyle::default()
                    }),
                )
            })
            .collect();
        Theme::from_map(name, parent, &styles)
    }

    fn from_map(
        name: String,
        parent: Option<&Theme>,
        overrides: &HashMap<Meaning, ContentStyle>,
    ) -> Theme {
        let styles = match parent {
            Some(theme) => Box::new(theme.styles.clone()),
            None => Box::new(DEFAULT_THEME.styles.clone()),
        }
        .iter()
        .map(|(name, color)| match overrides.get(name) {
            Some(value) => (*name, *value),
            None => (*name, *color),
        })
        .collect();
        Theme::new(name, parent.map(|p| p.name.clone()), styles)
    }
}

fn from_string(name: &str) -> std::result::Result<Color, String> {
    if name.is_empty() {
        return Err("Empty string".into());
    }
    let first_char = name.chars().next().unwrap();
    match first_char {
        '#' => {
            let hexcode = &name[1..];
            let vec: Vec<u8> = hexcode
                .chars()
                .collect::<Vec<char>>()
                .chunks(2)
                .map(|pair| u8::from_str_radix(pair.iter().collect::<String>().as_str(), 16))
                .filter_map(|n| n.ok())
                .collect();
            if vec.len() != 3 {
                return Err("Could not parse 3 hex values from string".into());
            }
            Ok(Color::Rgb {
                r: vec[0],
                g: vec[1],
                b: vec[2],
            })
        }
        '@' => {
            serde_json::from_str::<Color>(format!("\"{}\"", &name[1..]).as_str())
                .map_err(|_| format!("Could not convert color name {name} to Crossterm color"))
        }
        _ => {
            let srgb = palette::named::from_str(name).ok_or("No such color in palette")?;
            Ok(Color::Rgb {
                r: srgb.red,
                g: srgb.green,
                b: srgb.blue,
            })
        }
    }
}

pub struct StyleFactory {}

impl StyleFactory {
    fn from_fg_string(name: &str) -> std::result::Result<ContentStyle, String> {
        match from_string(name) {
            Ok(color) => Ok(Self::from_fg_color(color)),
            Err(err) => Err(err),
        }
    }

    fn known_fg_string(name: &str) -> ContentStyle {
        Self::from_fg_string(name).unwrap()
    }

    fn from_fg_color(color: Color) -> ContentStyle {
        ContentStyle {
            foreground_color: Some(color),
            ..ContentStyle::default()
        }
    }

    fn from_fg_color_and_attributes(color: Color, attributes: Attributes) -> ContentStyle {
        ContentStyle {
            foreground_color: Some(color),
            attributes,
            ..ContentStyle::default()
        }
    }
}

static ALERT_TYPES: LazyLock<HashMap<log::Level, Meaning>> = LazyLock::new(|| {
    HashMap::from([
        (log::Level::Info, Meaning::AlertInfo),
        (log::Level::Warn, Meaning::AlertWarn),
        (log::Level::Error, Meaning::AlertError),
    ])
});

static MEANING_FALLBACKS: LazyLock<HashMap<Meaning, Meaning>> = LazyLock::new(|| {
    HashMap::from([
        (Meaning::Guidance, Meaning::AlertInfo),
        (Meaning::Annotation, Meaning::AlertInfo),
        (Meaning::Title, Meaning::Important),
    ])
});

static DEFAULT_THEME: LazyLock<Theme> = LazyLock::new(|| {
    Theme::new(
        "default".to_string(),
        None,
        HashMap::from([
            (
                Meaning::AlertError,
                StyleFactory::from_fg_color(Color::DarkRed),
            ),
            (
                Meaning::AlertWarn,
                StyleFactory::from_fg_color(Color::DarkYellow),
            ),
            (
                Meaning::AlertInfo,
                StyleFactory::from_fg_color(Color::DarkGreen),
            ),
            (
                Meaning::Annotation,
                StyleFactory::from_fg_color(Color::DarkGrey),
            ),
            (
                Meaning::Guidance,
                StyleFactory::from_fg_color(Color::DarkBlue),
            ),
            (
                Meaning::Important,
                StyleFactory::from_fg_color_and_attributes(
                    Color::White,
                    Attributes::from(Attribute::Bold),
                ),
            ),
            (Meaning::Muted, StyleFactory::from_fg_color(Color::Grey)),
            (Meaning::Base, ContentStyle::default()),
        ]),
    )
});

static BUILTIN_THEMES: LazyLock<HashMap<&'static str, Theme>> = LazyLock::new(|| {
    HashMap::from([
        ("default", HashMap::new()),
        (
            "(none)",
            HashMap::from([
                (Meaning::AlertError, ContentStyle::default()),
                (Meaning::AlertWarn, ContentStyle::default()),
                (Meaning::AlertInfo, ContentStyle::default()),
                (Meaning::Annotation, ContentStyle::default()),
                (Meaning::Guidance, ContentStyle::default()),
                (Meaning::Important, ContentStyle::default()),
                (Meaning::Muted, ContentStyle::default()),
                (Meaning::Base, ContentStyle::default()),
            ]),
        ),
        (
            "autumn",
            HashMap::from([
                (
                    Meaning::AlertError,
                    StyleFactory::known_fg_string("saddlebrown"),
                ),
                (
                    Meaning::AlertWarn,
                    StyleFactory::known_fg_string("darkorange"),
                ),
                (Meaning::AlertInfo, StyleFactory::known_fg_string("gold")),
                (
                    Meaning::Annotation,
                    StyleFactory::from_fg_color(Color::DarkGrey),
                ),
                (Meaning::Guidance, StyleFactory::known_fg_string("brown")),
            ]),
        ),
        (
            "marine",
            HashMap::from([
                (
                    Meaning::AlertError,
                    StyleFactory::known_fg_string("yellowgreen"),
                ),
                (Meaning::AlertWarn, StyleFactory::known_fg_string("cyan")),
                (
                    Meaning::AlertInfo,
                    StyleFactory::known_fg_string("turquoise"),
                ),
                (
                    Meaning::Annotation,
                    StyleFactory::known_fg_string("steelblue"),
                ),
                (
                    Meaning::Base,
                    StyleFactory::known_fg_string("lightsteelblue"),
                ),
                (Meaning::Guidance, StyleFactory::known_fg_string("teal")),
            ]),
        ),
    ])
    .iter()
    .map(|(name, theme)| (*name, Theme::from_map(name.to_string(), None, theme)))
    .collect()
});

pub struct ThemeManager {
    loaded_themes: HashMap<String, Theme>,
    debug: bool,
    override_theme_dir: Option<String>,
}

impl ThemeManager {
    pub fn new(debug: Option<bool>, theme_dir: Option<String>) -> Self {
        Self {
            loaded_themes: HashMap::new(),
            debug: debug.unwrap_or(false),
            override_theme_dir: match theme_dir {
                Some(theme_dir) => Some(theme_dir),
                None => std::env::var("ATUIN_THEME_DIR").ok(),
            },
        }
    }

    pub fn load_theme_from_file(
        &mut self,
        name: &str,
        max_depth: u8,
    ) -> std::result::Result<&Theme, Box<dyn std::error::Error>> {
        use std::io::{Error as IoError, ErrorKind};

        let mut theme_file = if let Some(p) = &self.override_theme_dir {
            if p.is_empty() {
                return Err(Box::new(IoError::new(
                    ErrorKind::NotFound,
                    "Empty theme directory override and could not find theme elsewhere",
                )));
            }
            PathBuf::from(p)
        } else {
            let cfg_dir = config_dir();
            let mut theme_file = if let Ok(p) = std::env::var("ATUIN_CONFIG_DIR") {
                PathBuf::from(p)
            } else {
                let mut theme_file = PathBuf::new();
                theme_file.push(cfg_dir);
                theme_file
            };
            theme_file.push("themes");
            theme_file
        };

        let theme_toml = format!("{name}.toml");
        theme_file.push(theme_toml);

        let mut config_builder = Config::builder();

        config_builder = config_builder.add_source(ConfigFile::new(
            theme_file.to_str().unwrap(),
            FileFormat::Toml,
        ));

        let config = config_builder.build()?;
        self.load_theme_from_config(name, config, max_depth)
    }

    pub fn load_theme_from_config(
        &mut self,
        name: &str,
        config: Config,
        max_depth: u8,
    ) -> std::result::Result<&Theme, Box<dyn std::error::Error>> {
        use std::io::{Error as IoError, ErrorKind};

        let debug = self.debug;
        let theme_config: ThemeConfig = match config.try_deserialize() {
            Ok(tc) => tc,
            Err(e) => {
                return Err(Box::new(IoError::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "Failed to deserialize theme: {}",
                        if debug {
                            e.to_string()
                        } else {
                            "set theme debug on for more info".to_string()
                        }
                    ),
                )));
            }
        };
        let colors: HashMap<Meaning, String> = theme_config.colors;
        let parent: Option<&Theme> = match theme_config.theme.parent {
            Some(parent_name) => {
                if max_depth == 0 {
                    return Err(Box::new(IoError::new(
                        ErrorKind::InvalidInput,
                        "Parent requested but we hit the recursion limit",
                    )));
                }
                Some(self.load_theme(parent_name.as_str(), Some(max_depth - 1)))
            }
            None => Some(self.load_theme("default", Some(max_depth - 1))),
        };

        if debug && name != theme_config.theme.name {
            log::warn!(
                "Your theme config name is not the name of your loaded theme {} != {}",
                name,
                theme_config.theme.name
            );
        }

        let theme = Theme::from_foreground_colors(theme_config.theme.name, parent, colors, debug);
        let name = name.to_string();
        self.loaded_themes.insert(name.clone(), theme);
        let theme = self.loaded_themes.get(&name).unwrap();
        Ok(theme)
    }

    pub fn load_theme(&mut self, name: &str, max_depth: Option<u8>) -> &Theme {
        if self.loaded_themes.contains_key(name) {
            return self.loaded_themes.get(name).unwrap();
        }
        let built_ins = &BUILTIN_THEMES;
        match built_ins.get(name) {
            Some(theme) => theme,
            None => match self.load_theme_from_file(name, max_depth.unwrap_or(DEFAULT_MAX_DEPTH)) {
                Ok(theme) => theme,
                Err(err) => {
                    log::warn!("Could not load theme {name}: {err}");
                    built_ins.get("(none)").unwrap()
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Section 5: Utils
// ---------------------------------------------------------------------------

pub fn home_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .expect("could not determine home directory")
}

pub fn config_dir() -> PathBuf {
    let cfg_dir =
        std::env::var("XDG_CONFIG_HOME").map_or_else(|_| home_dir().join(".config"), PathBuf::from);
    cfg_dir.join("atuin")
}

pub fn data_dir() -> PathBuf {
    let d = std::env::var("XDG_DATA_HOME")
        .map_or_else(|_| home_dir().join(".local").join("share"), PathBuf::from);
    d.join("atuin")
}

pub fn runtime_dir() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR").map_or_else(|_| data_dir(), PathBuf::from)
}

pub fn logs_dir() -> PathBuf {
    home_dir().join(".atuin").join("logs")
}

pub fn broken_symlink<P: Into<PathBuf>>(path: P) -> bool {
    let path = path.into();
    path.is_symlink() && !path.exists()
}

pub fn has_git_dir(path: &str) -> bool {
    let mut gitdir = PathBuf::from(path);
    gitdir.push(".git");
    gitdir.exists()
}

pub fn in_git_repo(path: &str) -> Option<PathBuf> {
    let mut gitdir = PathBuf::from(path);

    while gitdir.parent().is_some() && !has_git_dir(gitdir.to_str().unwrap()) {
        gitdir.pop();
    }

    if gitdir.parent().is_some() {
        return Some(gitdir);
    }

    None
}

pub trait Escapable: AsRef<str> {
    fn escape_control(&self) -> Cow<'_, str> {
        if !self.as_ref().contains(|c: char| c.is_ascii_control()) {
            self.as_ref().into()
        } else {
            let mut remaining = self.as_ref();
            let mut buf = String::with_capacity(remaining.len());
            while let Some(i) = remaining.find(|c: char| c.is_ascii_control()) {
                buf.push_str(&remaining[..i]);
                buf.push('^');
                buf.push(match remaining.as_bytes()[i] {
                    0x7F => '?',
                    code => char::from_u32(u32::from(code) + 64).unwrap(),
                });
                remaining = &remaining[i + 1..];
            }
            buf.push_str(remaining);
            buf.into()
        }
    }
}

impl<T: AsRef<str>> Escapable for T {}

// ---------------------------------------------------------------------------
// Section 6: Shell
// ---------------------------------------------------------------------------

#[derive(PartialEq)]
pub enum Shell {
    Sh,
    Bash,
    Fish,
    Zsh,
    Xonsh,
    Nu,
    Powershell,
    Unknown,
}

impl fmt::Display for Shell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Shell::Sh => "sh",
            Shell::Bash => "bash",
            Shell::Fish => "fish",
            Shell::Zsh => "zsh",
            Shell::Xonsh => "xonsh",
            Shell::Nu => "nu",
            Shell::Powershell => "powershell",
            Shell::Unknown => "unknown",
        };
        write!(f, "{name}")
    }
}

impl Shell {
    pub fn from_env() -> Shell {
        std::env::var("ATUIN_SHELL").map_or(Shell::Unknown, |shell| {
            Shell::from_string(shell.trim().to_lowercase())
        })
    }

    pub fn from_string(name: String) -> Shell {
        match name.as_str() {
            "bash" => Shell::Bash,
            "fish" => Shell::Fish,
            "zsh" => Shell::Zsh,
            "xonsh" => Shell::Xonsh,
            "nu" => Shell::Nu,
            "sh" => Shell::Sh,
            "powershell" => Shell::Powershell,
            _ => Shell::Unknown,
        }
    }

    pub fn is_posixish(&self) -> bool {
        matches!(self, Shell::Bash | Shell::Fish | Shell::Zsh)
    }
}
