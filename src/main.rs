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

use input::TsvReader;
use memory_db::MemoryDatabase;

fn get_host_user() -> String {
    let host = whoami::hostname()
        .unwrap_or_else(|_| "unknown-host".to_string());
    let user = whoami::username()
        .unwrap_or_else(|_| "unknown-user".to_string());
    format!("{host}:{user}")
}

#[derive(Parser, Debug)]
#[command(
    name = "atuin-tui",
    about = "Standalone TUI history inspector",
    long_about = "Interactive TUI for browsing shell history piped in over stdin.\n\
        \n\
        Reads TSV records from stdin, one per line, with columns:\n\
        \n\
        \x20 timestamp_ns<TAB>duration_ns<TAB>exit<TAB>command<TAB>cwd<TAB>session<TAB>hostname\n\
        \n\
        Example — browse atuin history:\n\
        \n\
        \x20 atuin history list --format \"{time}\\t{duration}\\t{exit}\\t{command}\\t{cwd}\\t{session}\\t{host}\" \\\n\
        \x20   | atuin-tui\n\
        \n\
        Example — one-shot from a plain text log (timestamp in nanoseconds):\n\
        \n\
        \x20 printf '%s\\t-1\\t0\\techo hello\\t/tmp\\tsession1\\tlocalhost:user\\n' \"$(date +%s%N)\" \\\n\
        \x20   | atuin-tui\n\
        \n\
        The selected command is printed to stdout on exit, making it easy to\n\
        capture and execute:\n\
        \n\
        \x20 cmd=$(atuin history list ... | atuin-tui) && eval \"$cmd\""
)]
struct Args {
    /// Number of entries to read before displaying the TUI
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
    let mut theme_manager =
        ThemeManager::new(settings.theme.debug, None);
    let app_theme = theme_manager.load_theme(theme_name.as_str(), settings.theme.max_depth);

    let mut reader = TsvReader::new(tokio::io::stdin());
    let first_page = reader.read_batch(args.page_size).await;

    let (db, db_handle) = MemoryDatabase::new(first_page);

    let (tx, rx) = tokio::sync::watch::channel(());

    // Background loader: reads remaining entries and notifies TUI
    tokio::spawn(async move {
        loop {
            let batch = reader.read_batch(100).await;
            if batch.is_empty() {
                break;
            }
            db_handle.append(batch).await;
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
