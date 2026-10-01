mod app;
mod commits;
mod input;
mod render;
mod ui;

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use app::{App, AppAction};

type Term = Terminal<CrosstermBackend<io::Stdout>>;

const POLL: Duration = Duration::from_millis(250);
const TICK: Duration = Duration::from_millis(1000);

fn main() -> io::Result<()> {
    let worktree = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    install_panic_hook();
    let mut terminal = setup_terminal()?;
    let app = App::new(worktree);
    let result = run(&mut terminal, app);
    leave_screen()?;
    result
}

/// Enter raw mode + the alternate screen + mouse capture. Shared by startup
/// and the resume-after-editor path.
fn enter_screen() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)
}

/// Inverse of [`enter_screen`]: leave the alternate screen, drop raw mode and
/// mouse capture, and show the cursor. Shared by shutdown, the panic hook, and
/// the suspend-to-editor path.
fn leave_screen() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        Show
    )
}

fn setup_terminal() -> io::Result<Term> {
    enter_screen()?;
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

/// Restore the terminal even on panic, so a crash never leaves it wedged.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = leave_screen();
        original(info);
    }));
}

/// Build the `(program, args)` to launch for an `$EDITOR`/`$VISUAL` value,
/// opening `path` at `line` via the `+N file` convention. Empty/`None` falls
/// back to `vi`; the spec is split on whitespace so `EDITOR="code -w"` keeps
/// its leading args.
fn editor_command(env_val: Option<&str>, line: u32, path: &Path) -> (String, Vec<String>) {
    let spec = env_val
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("vi");
    let mut parts = spec.split_whitespace().map(String::from);
    let prog = parts.next().unwrap_or_else(|| "vi".into());
    let mut args: Vec<String> = parts.collect();
    args.push(format!("+{line}"));
    args.push(path.to_string_lossy().into_owned());
    (prog, args)
}

/// Suspend the TUI, open the selected change's file in the user's editor at the
/// changed line, then restore. Returns a user-facing message on failure (no
/// selection is a silent no-op).
fn edit_selected(terminal: &mut Term, app: &App) -> Result<(), String> {
    let Some((path, line)) = app.selected_path_and_line() else {
        return Ok(());
    };
    let env_val = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok();
    let (prog, args) = editor_command(env_val.as_deref(), line, &path);

    leave_screen().map_err(|e| format!("terminal: {e}"))?;
    let result = Command::new(&prog).args(&args).status();
    enter_screen().map_err(|e| format!("terminal: {e}"))?;
    let _ = terminal.clear();

    match result {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("could not launch '{prog}': {e}")),
    }
}

fn run(terminal: &mut Term, mut app: App) -> io::Result<()> {
    let mut last_tick = Instant::now();
    loop {
        terminal.draw(|f| ui::draw(f, &mut app))?;

        if event::poll(POLL)? {
            let ev: Event = event::read()?;
            // A real keypress dismisses a prior transient status; mouse motion,
            // scroll, and key-release events leave it on screen.
            if matches!(&ev, Event::Key(k) if k.kind == KeyEventKind::Press) {
                app.clear_status();
            }
            let action = input::map(ev, &app);
            match action {
                AppAction::OpenInEditor => {
                    if let Err(msg) = edit_selected(terminal, &app) {
                        app.set_status(msg);
                    }
                }
                other => app.apply(other),
            }
        }
        if last_tick.elapsed() >= TICK {
            app.apply(AppAction::Tick);
            last_tick = Instant::now();
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::editor_command;
    use std::path::Path;

    #[test]
    fn falls_back_to_vi_when_unset() {
        let (prog, args) = editor_command(None, 5, Path::new("/a.rs"));
        assert_eq!(prog, "vi");
        assert_eq!(args, vec!["+5".to_string(), "/a.rs".to_string()]);
    }

    #[test]
    fn falls_back_to_vi_when_blank() {
        let (prog, _) = editor_command(Some("   "), 1, Path::new("/a.rs"));
        assert_eq!(prog, "vi");
    }

    #[test]
    fn single_binary_gets_line_and_path() {
        let (prog, args) = editor_command(Some("nvim"), 42, Path::new("/src/x.rs"));
        assert_eq!(prog, "nvim");
        assert_eq!(args, vec!["+42".to_string(), "/src/x.rs".to_string()]);
    }

    #[test]
    fn embedded_args_are_preserved_before_line_and_path() {
        let (prog, args) = editor_command(Some("code -w"), 7, Path::new("/a.rs"));
        assert_eq!(prog, "code");
        assert_eq!(
            args,
            vec!["-w".to_string(), "+7".to_string(), "/a.rs".to_string()]
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let (prog, args) = editor_command(Some("  nvim  "), 3, Path::new("/a.rs"));
        assert_eq!(prog, "nvim");
        assert_eq!(args, vec!["+3".to_string(), "/a.rs".to_string()]);
    }
}
