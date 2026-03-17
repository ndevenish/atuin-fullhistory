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


fn default_history_file() -> Option<PathBuf> {
    let path = directories::UserDirs::new()?.home_dir().join(".fullhistory");
    path.exists().then_some(path)
}

#[derive(Parser, Debug)]
#[command(
    name = "atuin-fullhistory",
    about = "Standalone TUI history inspector",
    long_about = "Interactive TUI for browsing shell history.\n\
        \n\
        Reads ~/.fullhistory by default; use --file to specify another path.\n\
        Exits with an error if no file is found.\n\
        \n\
        The last ~128 KB of the file is read first (NFS-block-aligned) so the\n\
        UI appears immediately with recent history. Older entries load in the\n\
        background.\n\
        \n\
        ~/.fullhistory format (one command per line):\n\
        \n\
        \x20 hostname:\"cwd\" pid YYYY-MM-DDTHH:MM:SS+ZZZZ command\n\
        \x20 ##EXIT## hostname pid=N $?=N t_ms=N\n\
        \n\
        The selected command is printed to stdout on exit:\n\
        \n\
        \x20 cmd=$(atuin-tui) && eval \"$cmd\""
)]
struct Args {
    /// History file to read [default: ~/.fullhistory]
    #[arg(long)]
    file: Option<PathBuf>,

    /// Session ID [env: ATUIN_SESSION]
    #[arg(long)]
    session: Option<String>,

    /// Hostname to match against history entries (used by host/session filter modes)
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
    let hostname = args.hostname.unwrap_or_else(|| {
        whoami::hostname().unwrap_or_else(|_| "unknown-host".to_string())
    });
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

    let reader = FullHistoryReader::new(path);
    let (first_page, head_end) = reader.read_tail().await;

    let (db, db_handle) = MemoryDatabase::new(first_page);
    let (tx, rx) = tokio::sync::watch::channel(());
    // Keep a sender alive in this scope so the channel is never closed while
    // the TUI is running.  When the background task finishes and drops its
    // clone of `tx`, `entries_rx.changed()` would otherwise start returning
    // Err immediately on every call, causing the TUI event-loop to spin and
    // never reach the event::poll arm.
    let _tx_open = tx.clone();

    // Load the older head portion in the background; the TUI is already showing.
    tokio::spawn(async move {
        if let Some(end) = head_end {
            let head = reader.read_head(end).await;
            for chunk in head.chunks(200) {
                db_handle.append(chunk.to_vec()).await;
                let _ = tx.send(());
            }
        }
    });

    let result =
        tui::interactive::history(&[], &settings, db, &app_theme, rx, context).await?;
    if !result.is_empty() {
        println!("{result}");
    }

    Ok(())
}
