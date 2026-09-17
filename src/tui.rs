use crate::filesystem::StateLock;
use crate::{config_dir, inventory, App};
use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Terminal,
};
use std::io::{self, IsTerminal};

struct TerminalSession<'a, B: ratatui::backend::Backend + io::Write> {
    terminal: &'a mut Terminal<B>,
    raw_mode: bool,
    alternate_screen: bool,
}

impl<'a, B: ratatui::backend::Backend + io::Write> TerminalSession<'a, B> {
    fn enter(terminal: &'a mut Terminal<B>) -> Result<Self> {
        enable_raw_mode().context("enable terminal raw mode")?;
        let mut session = Self {
            terminal,
            raw_mode: true,
            alternate_screen: false,
        };
        execute!(session.terminal.backend_mut(), EnterAlternateScreen)
            .context("enter terminal alternate screen")?;
        session.alternate_screen = true;
        Ok(session)
    }
}

impl<B: ratatui::backend::Backend + io::Write> Drop for TerminalSession<'_, B> {
    fn drop(&mut self) {
        if self.alternate_screen {
            let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        }
        if self.raw_mode {
            let _ = disable_raw_mode();
        }
        let _ = self.terminal.show_cursor();
    }
}

pub(crate) fn run() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(());
    }
    let config = config_dir();
    let lock = StateLock::acquire_read_only_if_present(&config)?;
    let app = App::load(lock.as_ref())?;
    if !app.state_path.exists() {
        println!("Skillsync is not initialized; run init");
        return Ok(());
    }
    let snapshot = inventory::query(&app).context("build library inventory")?;
    drop(lock);
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let session = TerminalSession::enter(&mut terminal)?;
    #[cfg(feature = "test-hooks")]
    if std::env::var("SKILLSYNC_TEST_TUI_PANIC").as_deref() == Ok("1") {
        panic!("injected TUI panic (test-only)");
    }
    browser(session.terminal, &snapshot)
}

fn browser<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    inventory: &inventory::Inventory,
) -> Result<()> {
    let mut selected = 0usize;
    let mut detail = false;
    loop {
        terminal.draw(|frame| {
            let areas = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(3), Constraint::Min(5), Constraint::Length(2)]).split(frame.area());
            let title = Paragraph::new(format!("Skillsync Library  •  {} packages  •  worker: {}", inventory.packages.len(), inventory.worker)).block(Block::default().borders(Borders::ALL).title("Library"));
            frame.render_widget(title, areas[0]);
            if detail && !inventory.packages.is_empty() {
                let p = &inventory.packages[selected];
                let source = p.sources.iter().map(|s| format!("{} {}", s.kind, s.repository.as_deref().unwrap_or("local"))).collect::<Vec<_>>().join("; ");
                let text = format!("{}\n\nPath: {}\nProvenance: {}\nRelationships: subscriptions {}, publications {}, adoptions {}\nHarness health: {}\nUnsupported capabilities: Hermes curation, harness discovery/filtering/reload, registry integration\n\nEnter/Esc: back", p.name, p.path, source, p.relationship.subscriptions.len(), p.relationship.publications.len(), p.relationship.local_adoptions.len(), inventory.harness_links.iter().map(|h| h.status.as_str()).collect::<Vec<_>>().join(", "));
                frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).block(Block::default().borders(Borders::ALL).title("Package detail (read-only)")), areas[1]);
            } else {
                let items = inventory.packages.iter().map(|p| ListItem::new(format!("{}  [{}]", p.path, p.sources.first().map(|s| s.kind.as_str()).unwrap_or("local")))).collect::<Vec<_>>();
                frame.render_widget(List::new(items).block(Block::default().borders(Borders::ALL).title("Packages (library-first)")), areas[1]);
            }
            frame.render_widget(Paragraph::new("Attention/status: read-only snapshot  •  ↑↓/j/k navigate  •  Enter detail/back  •  q/Esc quit"), areas[2]);
        })?;
        if event::poll(std::time::Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Up | KeyCode::Char('k') if !detail => {
                        selected = selected.saturating_sub(1)
                    }
                    KeyCode::Down | KeyCode::Char('j') if !detail => {
                        selected = (selected + 1).min(inventory.packages.len().saturating_sub(1))
                    }
                    KeyCode::Enter => detail = !detail,
                    _ => {}
                }
            }
        }
    }
    Ok(())
}
