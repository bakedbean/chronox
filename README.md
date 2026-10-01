# chronox

A standalone [ratatui](https://ratatui.rs) terminal UI for browsing the
newest-first timeline of file changes a coding agent (Claude Code, Codex, or pi)
made in a worktree,
with a syntax-highlighted diff of the selected change. The timeline updates live
while a session is running.

## Install

To run `chronox` from anywhere, build a release binary and symlink it onto
your `PATH`:

```bash
./install.sh
```

This builds `target/release/chronox` and links it to
`~/.local/bin/chronox` (override with `BIN_DIR=~/bin ./install.sh`). The
symlink points at the build output, so to update after pulling new changes just
rebuild:

```bash
git pull && cargo build --release   # or re-run ./install.sh
```

## Run

```bash
cargo run                      # current directory
cargo run -- /path/to/worktree # an explicit worktree
```

Once installed, `cargo run` becomes just `chronox`:

```bash
chronox                      # current directory
chronox /path/to/worktree    # an explicit worktree
```

The worktree must have agent session logs — Claude Code
(`~/.claude/projects/<encoded-worktree>/*.jsonl`), Codex
(`~/.codex/sessions/**/rollout-*.jsonl` whose recorded cwd is the worktree), or
pi (`~/.pi/agent/sessions/<encoded-worktree>/*.jsonl`). Sessions from every
harness are merged into one timeline. Run a session in it (and make a few edits)
first, or you'll see the empty state.

## Keys

| Key | Action |
|-----|--------|
| `↑`/`↓`, `k`/`j` | List: move selection · Diff: scroll one line |
| `PgUp`/`PgDn` | Diff: scroll a page |
| `g`/`G`, `Home`/`End` | List: jump to top / bottom |
| `Tab` | Toggle focus between the list and the diff |
| `Enter` | Focus the diff pane |
| `d` | Toggle the diff between side-by-side (default) and block (before/after) |
| `e` | Open the selected file in `$VISUAL`/`$EDITOR` at the changed line |
| `[` / `]` | Nudge the split divider left / right |
| mouse drag on the divider | Resize the split |
| mouse wheel over the diff | Scroll the diff |
| `q`, `Esc`, `Ctrl-C` | Quit |

## Note on the diff views

`d` toggles how the selected change is shown. **Side-by-side** (the default)
aligns the old and new text line-by-line in two columns, coloring only the lines
that actually changed — old on the left (red), new on the right (green), with
unchanged context lines on both sides. **Block** shows the original
before/after form: the whole old block in red, then the whole new block in
green. In side-by-side the two columns scroll together as one. The two views
have different line counts, so switching with `d` starts the new view from the
top (the scroll position resets); the divider/`[`/`]` resize applies to the
pane as a whole.

## Note on the editor key

`e` resolves your editor from `$VISUAL`, then `$EDITOR`, falling back to `vi`,
and opens the file at the changed line using the `+N file` convention
(vim/neovim, nano, emacs, …). Editors that don't accept `+N` for a line number
(e.g. VS Code) will open the file but ignore the line.

## Note on mouse capture

The app captures the mouse (to support drag-resize), so your terminal's native
text selection needs the usual modifier (often `Shift`) while chronox is
running.
