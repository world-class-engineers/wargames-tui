mod app;
mod credentials;
mod pty;
mod scraper;
mod signals;
mod ssh;
mod ui;
mod verify;
mod wargame;
mod widgets;

use anyhow::Result;
use app::App;
use credentials::Paths;
use crossterm::clipboard::CopyToClipboard;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use portable_pty::PtySize;
use std::io::stdout;
use std::time::Duration;

fn main() {
    // Raw mode and the alternate screen must be restored even on panic.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        set_terminal_modes(false, false);
        ratatui::restore();
        previous_hook(info);
    }));

    // SIGINT/SIGTERM only flip a flag; the event loop then exits normally so
    // raw mode, the alternate screen, and the SSH process group are cleaned up.
    if let Err(error) = signals::install() {
        eprintln!("wargames-tui: could not install signal handlers: {error}");
    }

    let mut terminal = ratatui::init();
    set_terminal_modes(true, true);
    let result = run(&mut terminal);
    set_terminal_modes(false, false);
    ratatui::restore();

    if let Err(error) = result {
        eprintln!("wargames-tui: {error:#}");
        std::process::exit(1);
    }
}

/// Bracketed paste is always on for the UI lifetime. Mouse capture follows
/// `App::native_mouse_mode` and is changed only when it actually flips.
fn set_terminal_modes(paste: bool, mouse: bool) {
    let paste_result = if paste {
        crossterm::execute!(stdout(), EnableBracketedPaste)
    } else {
        crossterm::execute!(stdout(), DisableBracketedPaste)
    };
    if let Err(error) = paste_result {
        eprintln!("wargames-tui: could not set bracketed paste mode: {error}");
    }
    set_mouse_capture(mouse);
}

fn set_mouse_capture(enabled: bool) {
    let result = if enabled {
        crossterm::execute!(stdout(), EnableMouseCapture)
    } else {
        crossterm::execute!(stdout(), DisableMouseCapture)
    };
    if let Err(error) = result {
        eprintln!("wargames-tui: could not set mouse capture: {error}");
    }
}

/// Best-effort OSC 52 copy. Terminal support varies, so failures are ignored.
fn drain_clipboard(app: &mut App) {
    for text in app.pending_clipboard.drain(..) {
        let _ = crossterm::execute!(stdout(), CopyToClipboard::to_clipboard_from(text));
    }
}

fn run(terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    let area = terminal.size()?;
    let inner = ui::ssh_inner_area(area.into());
    let pane_size = PtySize {
        rows: inner.height.max(1),
        cols: inner.width.max(1),
        pixel_width: 0,
        pixel_height: 0,
    };

    let (tx, rx) = App::messages();
    let mut app = App::new(Paths::from_env(), area.into(), pane_size, tx)?;
    let mut mouse_enabled = true;

    while !app.should_quit {
        if signals::quit_requested() {
            break;
        }
        if event::poll(Duration::from_millis(50))? {
            app.handle_event(event::read()?)?;
        }
        while let Ok(message) = rx.try_recv() {
            app.handle_message(message)?;
        }

        let desired_mouse = !app.native_mouse_mode;
        if desired_mouse != mouse_enabled {
            set_mouse_capture(desired_mouse);
            mouse_enabled = desired_mouse;
        }
        drain_clipboard(&mut app);
        terminal.draw(|frame| ui::render(frame, &app))?;
    }

    app.shutdown();
    Ok(())
}
