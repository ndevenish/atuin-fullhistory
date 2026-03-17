# atuin-fullhistory

A standalone terminal UI for browsing shell history stored in `~/.fullhistory`.

This is a refactored version of the atuin TUI that builds independently without any `atuin-*` workspace dependencies. It is designed for machines where the full atuin binary isn't available or desirable, and is optimized for large history files on NFS mounts.

## Usage

```sh
# Browse history (reads ~/.fullhistory by default)
atuin-fullhistory

# Specify a different file
atuin-fullhistory --file /path/to/history

# Shell integration: insert selected command into the current line
output=$(ATUIN_SHELL_ZSH=t ATUIN_LOG=error atuin-fullhistory 3>&1 1>&2 2>&3)
```

The selected command is written to stdout when stdout is not a terminal (direct
`$()` capture), or to stderr otherwise. This matches the fd-swap pattern used by
atuin's shell integration (`3>&1 1>&2 2>&3`), where the TUI renders on the
reassigned stdout and the result is captured from stderr.

### CLI flags

| Flag | Description |
|------|-------------|
| `--file <PATH>` | History file to read (default: `~/.fullhistory`) |
| `--session <ID>` | Session ID (also read from `$ATUIN_SESSION`) |
| `--hostname <NAME>` | Hostname for host/session filter modes |
| `--cwd <DIR>` | Working directory for directory/workspace filter modes |
| `--git-root <DIR>` | Git root for workspace filter mode (auto-detected from `--cwd` if omitted) |

## History file format

```
hostname:"cwd" pid YYYY-MM-DDTHH:MM:SS+ZZZZ command
##EXIT## hostname pid=N $?=N t_ms=N
```

Each command line is optionally followed by an `##EXIT##` line that records the exit code and duration. This is the format written by atuin's `fullhistory` feature.

## Performance

Startup is fast even for very large files. The last ~128 KB of the file (NFS-block-aligned) is read synchronously before the UI opens. Older entries load in the background while you're already browsing.

## Building

```sh
cargo build --release
```

Requires Rust edition 2024 (rustup target: stable ≥ 1.84).

## Configuration

Settings are loaded from the atuin config file (typically `~/.config/atuin/config.toml`). Configurable options include search mode, filter mode, theme, keybindings, and UI column layout.
