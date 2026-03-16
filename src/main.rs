use std::path::PathBuf;

use clap::Parser;
use eyre::Result;

use crate::types::{Context, Settings, ThemeManager, in_git_repo};

mod input;
mod local_db;
mod memory_db;
mod sort;
mod tui;
mod types;

use input::FullHistoryReader;
use memory_db::MemoryDatabase;

fn get_host_user() -> String {
    let host = whoami::hostname()
        .unwrap_or_else(|_| "unknown-host".to_string());
    let user = whoami::username()
        .unwrap_or_else(|_| "unknown-user".to_string());
    format!("{host}:{user}")
}

fn default_history_file() -> Option<PathBuf> {
    let path = directories::UserDirs::new()?.home_dir().join(".fullhistory");
    path.exists().then_some(path)
}

#[derive(Parser, Debug)]
#[command(
    name = "atuin-tui",
    about = "Standalone TUI history inspector",
    long_about = "Interactive TUI for browsing shell history.\n\
        \n\
        Reads ~/.fullhistory by default; use --file to specify another path.\n\
        Exits with an error if no file is found.\n\
        \n\
        ~/.fullhistory format (one command per line):\n\
        \n\
        \x20 hostname:\"cwd\" pid YYYY-MM-DDTHH:MM:SS+ZZZZ command\n\
        \x20 ##EXIT## hostname pid=N $?=N t_ms=N\n\
        \n\
        Stdin TSV format (7 tab-separated columns):\n\
        \n\
        \x20 timestamp_ns<TAB>duration_ns<TAB>exit<TAB>command<TAB>cwd<TAB>session<TAB>hostname\n\
        \n\
        Example — browse atuin history via stdin:\n\
        \n\
        \x20 atuin history list --format \"{time}\\t{duration}\\t{exit}\\t{command}\\t{cwd}\\t{session}\\t{host}\" \\\n\
        \x20   | atuin-tui\n\
        \n\
        The selected command is printed to stdout on exit:\n\
        \n\
        \x20 cmd=$(atuin-tui) && eval \"$cmd\""
)]
struct Args {
    /// History file to read [default: ~/.fullhistory]
    #[arg(long)]
    file: Option<PathBuf>,

    /// Number of recent entries to display before loading the rest
    #[arg(long, default_value = "200")]
    page_size: usize,

    /// Session ID [env: ATUIN_SESSION]
    #[arg(long)]
    session: Option<String>,

    /// Hostname in host:user format (used by host/session filter modes)
    #[arg(long)]
    hostname: Option<String>,

    /// Working directory (used by directory/workspace filter modes)
    #[arg(long)]
    cwd: Option<String>,

    /// Git root directory (used by workspace filter mode; auto-detected from --cwd if omitted)
    #[arg(long)]
    git_root: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let session = args.session.unwrap_or_else(|| {
        std::env::var("ATUIN_SESSION")
            .unwrap_or_else(|_| uuid::Uuid::new_v4().simple().to_string())
    });
    let hostname = args.hostname.unwrap_or_else(get_host_user);
    let cwd = args.cwd.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| String::from("/"))
    });
    let git_root = args.git_root.or_else(|| in_git_repo(&cwd));

    let context = Context {
        session,
        hostname,
        cwd,
        host_id: String::new(),
        git_root,
    };

    let settings = Settings::new().unwrap_or_default();
    let theme_name = settings.theme.name.clone();
    let mut theme_manager = ThemeManager::new(settings.theme.debug, None);
    let app_theme = theme_manager.load_theme(theme_name.as_str(), settings.theme.max_depth);

    let path = args
        .file
        .or_else(default_history_file)
        .ok_or_else(|| eyre::eyre!("no history file found (tried ~/.fullhistory); use --file to specify one"))?;

    let mut all = FullHistoryReader::new(path).read_all().await;
    all.reverse(); // newest first

    let split = args.page_size.min(all.len());
    let rest = all.split_off(split);
    let first_page = all;

    let (db, db_handle) = MemoryDatabase::new(first_page);
    let (tx, rx) = tokio::sync::watch::channel(());

    tokio::spawn(async move {
        for chunk in rest.chunks(200) {
            db_handle.append(chunk.to_vec()).await;
            let _ = tx.send(());
        }
    });

    let result =
        tui::interactive::history(&[], &settings, db, &app_theme, rx, context).await?;
    if !result.is_empty() {
        println!("{result}");
    }

    Ok(())
}
