use std::io::{self, stderr};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Nucleo, Utf32String};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

use crate::nix::Package;

const UPDATED_NOTICE_DURATION: Duration = Duration::from_secs(4);

enum BgStatus {
    None,
    Fetching,
    Updated(Instant),
    Failed(String),
}

type BgResult = io::Result<Vec<Package>>;

struct App {
    query: String,
    cursor: usize,
    selected: usize,
    matcher: Nucleo<usize>,
    packages: Vec<Package>,
    bg_status: BgStatus,
}

fn build_search_text(pkg: &Package) -> String {
    // Name appears first so nucleo's positional scoring prioritises it.
    // Name is repeated to further boost packages that match by name.
    if pkg.package_set.is_empty() {
        format!(
            "{} {} {} {}",
            pkg.name, pkg.name, pkg.version, pkg.description
        )
    } else {
        format!(
            "{} {}.{} {} {} {}",
            pkg.name, pkg.package_set, pkg.name, pkg.name, pkg.version, pkg.description
        )
    }
}

fn build_matcher(packages: &[Package]) -> Nucleo<usize> {
    let matcher = Nucleo::new(Config::DEFAULT.match_paths(), Arc::new(|| {}), None, 1);
    let injector = matcher.injector();
    for (idx, pkg) in packages.iter().enumerate() {
        let search_text = build_search_text(pkg);
        injector.push(idx, |_, cols| {
            cols[0] = Utf32String::from(search_text.as_str());
        });
    }
    matcher
}

impl App {
    fn new(packages: Vec<Package>) -> Self {
        let matcher = build_matcher(&packages);
        App {
            query: String::new(),
            cursor: 0,
            selected: 0,
            matcher,
            packages,
            bg_status: BgStatus::None,
        }
    }

    fn replace_packages(&mut self, packages: Vec<Package>) {
        self.matcher = build_matcher(&packages);
        self.packages = packages;
        self.selected = 0;
        self.update_pattern();
    }

    fn update_pattern(&mut self) {
        self.matcher.pattern.reparse(
            0,
            &self.query,
            CaseMatching::Smart,
            Normalization::Smart,
            false,
        );
    }

    fn matched_count(&self) -> u32 {
        self.matcher.snapshot().matched_item_count()
    }

    fn total_count(&self) -> u32 {
        self.matcher.snapshot().item_count()
    }

    fn get_matched_package(&self, index: u32) -> Option<&Package> {
        let item = self.matcher.snapshot().get_matched_item(index)?;
        Some(&self.packages[*item.data])
    }

    fn status_message(&self) -> Option<(String, Color)> {
        match &self.bg_status {
            BgStatus::None => None,
            BgStatus::Fetching => Some((
                "⟳ Using cached results... fetching in the background".to_string(),
                Color::Yellow,
            )),
            BgStatus::Updated(at) => {
                if at.elapsed() < UPDATED_NOTICE_DURATION {
                    Some(("✓ Index updated".to_string(), Color::Green))
                } else {
                    None
                }
            }
            BgStatus::Failed(err) => Some((
                format!("✗ Background fetch failed: {err}"),
                Color::Red,
            )),
        }
    }
}

fn spawn_fetch(flake: &str) -> Receiver<BgResult> {
    let flake = flake.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::nix::fetch_fresh(&flake));
    });
    rx
}

pub fn run(flake: &str, viewport: Viewport) -> io::Result<Option<String>> {
    let fullscreen = matches!(viewport, Viewport::Fullscreen);

    if fullscreen {
        execute!(stderr(), EnterAlternateScreen)?;
    }

    let mut terminal = Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(stderr()),
        TerminalOptions { viewport },
    )?;

    crossterm::terminal::enable_raw_mode()?;

    // Try the on-disk cache first so the UI is interactive immediately.
    let cached = crate::nix::load_from_cache(flake);

    let (packages, bg_rx) = match cached {
        Some(cache) if cache.fresh => (cache.packages, None),
        Some(cache) => {
            // Stale cache: show it now, refresh in the background.
            let rx = spawn_fetch(flake);
            (cache.packages, Some(rx))
        }
        None => {
            // No cache: fetch synchronously with a loading message.
            let loading_msg = format!("Loading {flake} index...");
            terminal.draw(|f| {
                let area = f.area();
                let msg = Paragraph::new(loading_msg.as_str())
                    .style(Style::default().fg(Color::DarkGray));
                f.render_widget(msg, area);
            })?;

            match crate::nix::fetch_fresh(flake) {
                Ok(p) if !p.is_empty() => (p, None),
                Ok(_) => {
                    cleanup(&mut terminal, fullscreen)?;
                    eprintln!("No packages found.");
                    return Ok(None);
                }
                Err(e) => {
                    cleanup(&mut terminal, fullscreen)?;
                    return Err(e);
                }
            }
        }
    };

    let mut app = App::new(packages);
    if bg_rx.is_some() {
        app.bg_status = BgStatus::Fetching;
    }
    app.matcher.tick(10);

    let result = run_loop(&mut terminal, &mut app, bg_rx);

    cleanup(&mut terminal, fullscreen)?;

    result
}

fn cleanup(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<io::Stderr>>,
    fullscreen: bool,
) -> io::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    if fullscreen {
        execute!(stderr(), LeaveAlternateScreen)?;
    } else {
        terminal.clear()?;
    }
    Ok(())
}

fn run_loop(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<io::Stderr>>,
    app: &mut App,
    mut bg_rx: Option<Receiver<BgResult>>,
) -> io::Result<Option<String>> {
    loop {
        app.matcher.tick(10);

        // Poll the background fetch (if any) without blocking.
        if let Some(rx) = bg_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(packages)) if !packages.is_empty() => {
                    app.replace_packages(packages);
                    app.bg_status = BgStatus::Updated(Instant::now());
                    bg_rx = None;
                }
                Ok(Ok(_)) => {
                    // Empty result: keep showing cached data.
                    app.bg_status =
                        BgStatus::Failed("fetch returned no packages".to_string());
                    bg_rx = None;
                }
                Ok(Err(e)) => {
                    app.bg_status = BgStatus::Failed(e.to_string());
                    bg_rx = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    app.bg_status =
                        BgStatus::Failed("background fetch thread died".to_string());
                    bg_rx = None;
                }
            }
        }

        terminal.draw(|f| render(f, app))?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            match handle_key(app, key) {
                Action::Continue => {}
                Action::Quit => return Ok(None),
                Action::Select => {
                    if let Some(pkg) = app.get_matched_package(app.selected as u32) {
                        return Ok(Some(pkg.name.clone()));
                    }
                    return Ok(None);
                }
            }
        }
    }
}

enum Action {
    Continue,
    Quit,
    Select,
}

fn handle_key(app: &mut App, key: KeyEvent) -> Action {
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) => Action::Quit,
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => Action::Quit,

        // Result navigation
        (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
            let count = app.matched_count();
            if count > 0 && (app.selected as u32) < count - 1 {
                app.selected += 1;
            }
            Action::Continue
        }
        (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
            if app.selected > 0 {
                app.selected -= 1;
            }
            Action::Continue
        }

        // Select
        (_, KeyCode::Enter) => Action::Select,

        // Cursor movement
        (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
            app.cursor = 0;
            Action::Continue
        }
        (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
            app.cursor = app.query.len();
            Action::Continue
        }
        (KeyModifiers::CONTROL, KeyCode::Char('b')) => {
            if app.cursor > 0 {
                app.cursor = app.query[..app.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
            Action::Continue
        }
        (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
            if app.cursor < app.query.len() {
                app.cursor += app.query[app.cursor..]
                    .chars()
                    .next()
                    .map_or(0, |c| c.len_utf8());
            }
            Action::Continue
        }

        // Editing
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
            app.query.drain(..app.cursor);
            app.cursor = 0;
            app.selected = 0;
            app.update_pattern();
            Action::Continue
        }
        (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
            app.query.truncate(app.cursor);
            app.selected = 0;
            app.update_pattern();
            Action::Continue
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
            if app.cursor < app.query.len() {
                let next = app.cursor
                    + app.query[app.cursor..]
                        .chars()
                        .next()
                        .map_or(0, |c| c.len_utf8());
                app.query.drain(app.cursor..next);
                app.selected = 0;
                app.update_pattern();
            }
            Action::Continue
        }
        (_, KeyCode::Backspace) => {
            if app.cursor > 0 {
                let prev = app.query[..app.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                app.query.drain(prev..app.cursor);
                app.cursor = prev;
                app.selected = 0;
                app.update_pattern();
            }
            Action::Continue
        }
        (_, KeyCode::Char(c)) => {
            app.query.insert(app.cursor, c);
            app.cursor += c.len_utf8();
            app.selected = 0;
            app.update_pattern();
            Action::Continue
        }

        _ => Action::Continue,
    }
}

fn render(f: &mut Frame, app: &App) {
    let area = f.area();

    let status = app.status_message();

    if let Some((msg, color)) = status {
        let [input_area, status_area, results_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .areas(area);

        render_input(f, app, input_area);
        let status_widget = Paragraph::new(msg).style(Style::default().fg(color));
        f.render_widget(status_widget, status_area);
        render_results(f, app, results_area);
    } else {
        let [input_area, results_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(area);

        render_input(f, app, input_area);
        render_results(f, app, results_area);
    }
}

fn render_input(f: &mut Frame, app: &App, area: Rect) {
    let matched = app.matched_count();
    let total = app.total_count();

    let input = Paragraph::new(app.query.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {matched}/{total} ")),
    );
    f.render_widget(input, area);

    let cursor_x = area.x + 1 + app.cursor as u16;
    let cursor_y = area.y + 1;
    f.set_cursor_position((cursor_x, cursor_y));
}

fn render_results(f: &mut Frame, app: &App, area: Rect) {
    let snapshot = app.matcher.snapshot();
    let visible_count = area.height as u32;
    let matched_count = snapshot.matched_item_count();

    if matched_count == 0 {
        let empty = Paragraph::new("  No matches").style(Style::default().fg(Color::DarkGray));
        f.render_widget(empty, area);
        return;
    }

    const SCROLL_PADDING: u32 = 18;
    let selected = app.selected as u32;

    let ideal_start = selected.saturating_sub(SCROLL_PADDING);
    let ideal_end = selected + SCROLL_PADDING + 1;
    let (start, end) = if ideal_end - ideal_start >= visible_count {
        let start = selected.saturating_sub(visible_count / 2);
        let end = (start + visible_count).min(matched_count);
        (end.saturating_sub(visible_count), end)
    } else if ideal_end > matched_count {
        let end = matched_count;
        (end.saturating_sub(visible_count), end)
    } else if ideal_start == 0 {
        (0, visible_count.min(matched_count))
    } else {
        let end = (ideal_end + (visible_count - (ideal_end - ideal_start))).min(matched_count);
        let start = end.saturating_sub(visible_count);
        (start, end)
    };

    let visible: Vec<_> = (start..end)
        .filter_map(|i| {
            let item = snapshot.get_matched_item(i)?;
            let pkg = &app.packages[*item.data];
            let display_name = if pkg.package_set.is_empty() {
                pkg.name.clone()
            } else {
                format!("{}.{}", pkg.package_set, pkg.name)
            };
            Some((i, pkg, display_name))
        })
        .collect();

    let name_w = visible.iter().map(|(_, _, n)| n.len()).max().unwrap_or(0) + 2;
    let ver_w = visible
        .iter()
        .map(|(_, p, _)| p.version.len())
        .max()
        .unwrap_or(0)
        + 2;

    let items: Vec<ListItem> = visible
        .iter()
        .map(|(i, pkg, display_name)| {
            let is_selected = *i == selected;

            let name_style = if is_selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Green)
            };

            let ver_style = if is_selected {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let desc_style = if is_selected {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let marker = if is_selected { "▸ " } else { "  " };

            let line = Line::from(vec![
                Span::styled(marker, name_style),
                Span::styled(format!("{:<name_w$}", display_name), name_style),
                Span::styled(format!("{:<ver_w$}", pkg.version), ver_style),
                Span::styled(&pkg.description, desc_style),
            ]);

            ListItem::new(line)
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, area);
}
