# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

`atuin-fullhistory` is a standalone TUI for browsing shell history from `~/.fullhistory`. It was refactored from the atuin-tui crate to build independently without any `atuin-*` workspace dependencies. Key types from atuin-client/atuin-common are inlined in `src/types.rs`.

## Commands

```bash
cargo build              # debug build
cargo build --release    # release build
cargo run -- --file ~/.fullhistory  # run with history file
cargo check              # fast type check
cargo clippy             # lint
cargo fmt                # format
cargo test               # run all tests
cargo test <test_name>   # run a single test
```

Requires Rust edition 2024 (1.84+).

## Architecture

### Data Flow

```
~/.fullhistory file
    → input.rs (FullHistoryReader)
        - read_tail(): reads last ~128KB first (NFS-block-aligned) for fast startup
        - read_head(): loads older entries in background via spawn_blocking
    → memory_db.rs (MemoryDatabase)
        - RwLock<Vec<History>> shared between main TUI and background loader
        - watch channel signals TUI when new entries arrive
    → tui/interactive.rs (main event loop)
        - crossterm events → keybindings/actions → state transitions
        - ratatui rendering
```

### Key Modules

| Module | Role |
|--------|------|
| `main.rs` | CLI parsing (clap), context init, orchestrates startup |
| `src/types.rs` | All domain types inlined from atuin — `History`, `Settings`, `Theme`, `SearchMode`, `FilterMode`, etc. (~1800 lines) |
| `src/input.rs` | File I/O with NFS-aware tail-first loading strategy |
| `src/memory_db.rs` | In-memory DB; implements the `Db` trait; uses `tokio::sync::watch` to signal background load progress |
| `src/local_db.rs` | `Db` async trait definition |
| `src/sort.rs` | Scoring function for search results (match type × recency × case sensitivity) |
| `tui/interactive.rs` | Main event loop — ~3000 lines; owns all UI state, handles keyboard/mouse, manages tab switching (Search ↔ Inspect) |
| `tui/history_list.rs` | Renders the filterable list with configurable `UiColumn` layout |
| `tui/inspector.rs` | Detail view: statistics, execution timeline, similar commands |
| `tui/cursor.rs` | Text cursor for search input box |
| `tui/engines/` | Search strategies: `db.rs` (prefix/fulltext/fuzzy against MemoryDatabase), `skim.rs` (FZF-style via `skim` crate) |
| `tui/keybindings/` | Configurable keybinding system with condition evaluation (mode-aware) |

### Search Architecture

The `SearchEngine` enum in `tui/engines/mod.rs` dispatches to either:
- `DbSearchEngine` — queries `MemoryDatabase` directly; supports Prefix, FullText, Fuzzy modes
- `SkimEngine` — spawns a skim subprocess for interactive fuzzy search

Search results are scored by `sort.rs` before rendering.

### Performance Design

Startup is optimized for large NFS-backed files:
1. Tail (last ~128 KB, NFS block-aligned) is read synchronously before the TUI opens
2. Head (everything older) is loaded in `spawn_blocking` to avoid blocking the async runtime
3. The TUI event loop receives `WatchSender` updates to refresh the count display incrementally

### History File Format

```
hostname:"cwd" pid YYYY-MM-DDTHH:MM:SS+ZZZZ command
##EXIT## hostname pid=N $?=N t_ms=N
```

Exit lines are matched back to their command by hostname + pid to populate `History.exit` and `History.duration`.

### Output / Shell Integration

When a command is selected, it is printed to stdout so the calling shell can `eval` or insert it into the readline buffer.
