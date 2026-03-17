use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
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
#[command(name = "atuin-fullhistory", about = "Standalone TUI history inspector")]
struct Args {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Search shell history
    Search {
        /// Open interactive TUI
        #[arg(short, long)]
        interactive: bool,

        /// History file to read [default: ~/.fullhistory]
        #[arg(long)]
        file: Option<PathBuf>,

        /// Session ID [env: ATUIN_SESSION]
        #[arg(long)]
        session: Option<String>,

        /// Hostname to match against history entries
        #[arg(long)]
        hostname: Option<String>,

        /// Working directory (used by directory/workspace filter modes)
        #[arg(long)]
        cwd: Option<String>,

        /// Git root directory (auto-detected from --cwd if omitted)
        #[arg(long)]
        git_root: Option<PathBuf>,

        /// Search query
        #[arg(allow_hyphen_values = true)]
        query: Vec<String>,
    },

    /// Print shell integration script
    Init {
        shell: Shell,
    },
}

#[derive(Clone, Copy, ValueEnum, Debug)]
#[value(rename_all = "lower")]
enum Shell {
    Zsh,
    Bash,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    match args.command {
        Cmd::Init { shell } => {
            match shell {
                Shell::Zsh => print!("{}", ZSH_INIT),
                Shell::Bash => print!("{}", BASH_INIT),
            }
        }

        Cmd::Search {
            interactive,
            file,
            session,
            hostname,
            cwd,
            git_root,
            query,
        } => {
            if !interactive {
                eprintln!("Non-interactive search is not yet implemented. Use -i for the TUI.");
                return Ok(());
            }

            let session = session.unwrap_or_else(|| {
                std::env::var("ATUIN_SESSION")
                    .unwrap_or_else(|_| uuid::Uuid::new_v4().simple().to_string())
            });
            let hostname = hostname.unwrap_or_else(|| {
                whoami::hostname().unwrap_or_else(|_| "unknown-host".to_string())
            });
            let cwd = cwd.unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| String::from("/"))
            });
            let git_root = git_root.or_else(|| in_git_repo(&cwd));

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
            let app_theme =
                theme_manager.load_theme(theme_name.as_str(), settings.theme.max_depth);

            let path = file
                .or_else(default_history_file)
                .ok_or_else(|| eyre::eyre!("no history file found (tried ~/.fullhistory); use --file to specify one"))?;

            let reader = FullHistoryReader::new(path);
            let (first_page, head_end) = reader.read_tail().await;

            let (db, db_handle) = MemoryDatabase::new(first_page);
            let (tx, rx) = tokio::sync::watch::channel(());
            let _tx_open = tx.clone();

            tokio::spawn(async move {
                if let Some(end) = head_end {
                    for block in reader.read_head(end).await {
                        db_handle.append(block).await;
                        let _ = tx.send(());
                    }
                }
            });

            let result =
                tui::interactive::history(&query, &settings, db, &app_theme, rx, context).await?;
            if !result.is_empty() {
                println!("{result}");
            }
        }
    }

    Ok(())
}

const ZSH_INIT: &str = r#"_atuin_fh_search() {
    emulate -L zsh
    zle -I

    # swap stderr and stdout, so that the tui stuff works
    # TODO: not this
    local output
    # shellcheck disable=SC2048
    output=$(ATUIN_SHELL_ZSH=t ATUIN_LOG=error ATUIN_QUERY=$BUFFER atuin-fullhistory $* 3>&1 1>&2 2>&3)

    zle reset-prompt
    # re-enable bracketed paste
    # shellcheck disable=SC2154
    echo -n ${zle_bracketed_paste[1]} >/dev/tty

    if [[ -n $output ]]; then
        RBUFFER=""
        LBUFFER=$output

        if [[ $LBUFFER == __atuin_accept__:* ]]
        then
            LBUFFER=${LBUFFER#__atuin_accept__:}
            zle accept-line
        fi
    fi
}

zle -N atuin-fh-search _atuin_fh_search

bindkey -M emacs '^r' atuin-fh-search
bindkey -M vicmd '/' atuin-fh-search
"#;

const BASH_INIT: &str = r#"# atuin-fullhistory bash integration - not yet implemented
"#;
