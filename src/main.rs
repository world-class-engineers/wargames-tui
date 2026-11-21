mod app;
mod credentials;
mod pty;
mod scraper;
mod signals;
mod ssh;
mod ui;
mod verify;
mod widgets;

use anyhow::Result;
use app::App;
use credentials::Paths;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use portable_pty::PtySize;
use std::time::Duration;

fn main() {
    // Raw mode and the alternate screen must be restored even on panic.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        set_terminal_modes(false);
        ratatui::restore();
        previous_hook(info);
    }));

    // SIGINT/SIGTERM only flip a flag; the event loop then exits normally so
    // raw mode, the alternate screen, and the SSH process group are cleaned up.
    if let Err(error) = signals::install() {
        eprintln!("bandit-tui: could not install signal handlers: {error}");
    }

    let mut terminal = ratatui::init();
    set_terminal_modes(true);
    let result = run(&mut terminal);
    set_terminal_modes(false);
    ratatui::restore();

    if let Err(error) = result {
        eprintln!("bandit-tui: {error:#}");
        std::process::exit(1);
    }
}

/// Bracketed paste and mouse capture are both enabled for the UI lifetime and
/// disabled on every exit path, including the panic hook above.
fn set_terminal_modes(enabled: bool) {
    let paste = if enabled {
        crossterm::execute!(std::io::stdout(), EnableBracketedPaste)
    } else {
        crossterm::execute!(std::io::stdout(), DisableBracketedPaste)
    };
    if let Err(error) = paste {
        eprintln!("bandit-tui: could not set bracketed paste mode: {error}");
    }

    let mouse = if enabled {
        crossterm::execute!(std::io::stdout(), EnableMouseCapture)
    } else {
        crossterm::execute!(std::io::stdout(), DisableMouseCapture)
    };
    if let Err(error) = mouse {
        eprintln!("bandit-tui: could not set mouse capture: {error}");
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

    while !app.should_quit {
        if signals::quit_requested() {
            app.should_quit = true;
            break;
        }
        if event::poll(Duration::from_millis(50))? {
            app.handle_event(event::read()?)?;
        }
        while let Ok(message) = rx.try_recv() {
            app.handle_message(message)?;
        }
        terminal.draw(|frame| ui::render(frame, &app))?;
    }

    app.shutdown();
    Ok(())
}
