use std::{io, time::Duration};

use anyhow::{Context, Result};
use chrono::Utc;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use metra_proto::HealthResponse;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use reqwest::Client;

use crate::rest::fetch_health;

#[derive(Default)]
struct TuiState {
    health: Option<HealthResponse>,
    error: Option<String>,
    refreshed_at: Option<String>,
}

pub async fn run_tui(http: &Client, server: &str) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;
    let result = run_tui_loop(&mut terminal, http, server).await;

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    result
}

async fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    http: &Client,
    server: &str,
) -> Result<()> {
    let mut state = TuiState::default();
    refresh_health_in_state(&mut state, http, server).await;

    loop {
        terminal.draw(|frame| render_tui(frame, &state, server))?;
        if event::poll(Duration::from_millis(200))? {
            let event = event::read()?;
            if let Event::Key(key) = event {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('r') => refresh_health_in_state(&mut state, http, server).await,
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

async fn refresh_health_in_state(state: &mut TuiState, http: &Client, server: &str) {
    match fetch_health(http, server).await {
        Ok(health) => {
            state.health = Some(health);
            state.error = None;
            state.refreshed_at = Some(Utc::now().to_rfc3339());
        }
        Err(err) => {
            state.error = Some(err.to_string());
            state.refreshed_at = Some(Utc::now().to_rfc3339());
        }
    }
}

fn render_tui(frame: &mut ratatui::Frame<'_>, state: &TuiState, server: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Min(8),
        ])
        .split(frame.area());

    let header = Paragraph::new(Text::from(vec![
        Line::from("Metra Client TUI"),
        Line::from(format!("Server: {server}")),
        Line::from("Keys: r=refresh health, q=quit"),
    ]))
    .block(Block::default().title("Session").borders(Borders::ALL));
    frame.render_widget(header, chunks[0]);

    let refreshed_text = state
        .refreshed_at
        .as_deref()
        .map_or_else(|| "never".to_owned(), ToOwned::to_owned);
    let refresh_box = Paragraph::new(format!("Last refresh: {refreshed_text}"))
        .block(Block::default().title("Refresh").borders(Borders::ALL));
    frame.render_widget(refresh_box, chunks[1]);

    let body_lines = if let Some(health) = &state.health {
        vec![
            Line::from(format!("status: {}", health.status)),
            Line::from(format!("version: {}", health.version)),
            Line::from(format!("quic_listener: {}", health.quic_listener)),
            Line::from(format!("timestamp: {}", health.timestamp)),
        ]
    } else if let Some(error) = &state.error {
        vec![Line::styled(
            format!("error: {error}"),
            Style::default().fg(Color::Red),
        )]
    } else {
        vec![Line::from("no data yet")]
    };

    let body = Paragraph::new(Text::from(body_lines))
        .wrap(Wrap { trim: true })
        .block(Block::default().title("Health").borders(Borders::ALL));
    frame.render_widget(body, chunks[2]);
}
