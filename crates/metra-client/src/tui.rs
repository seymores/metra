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

use crate::{
    cli::{BenchArgs, TuiArgs},
    rest::fetch_health,
    transfer::run_benchmark_quiet,
};

#[derive(Default)]
struct TuiState {
    health: Option<HealthResponse>,
    error: Option<String>,
    refreshed_at: Option<String>,
    benchmark_running: bool,
    benchmark_report_json: Option<String>,
    benchmark_error: Option<String>,
}

pub async fn run_tui(http: &Client, server: &str, args: TuiArgs) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;
    let result = run_tui_loop(&mut terminal, http, server, &args).await;

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    result
}

async fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    http: &Client,
    server: &str,
    args: &TuiArgs,
) -> Result<()> {
    let mut state = TuiState::default();
    refresh_health_in_state(&mut state, http, server).await;

    loop {
        terminal.draw(|frame| render_tui(frame, &state, server, args))?;
        if event::poll(Duration::from_millis(200))? {
            let event = event::read()?;
            if let Event::Key(key) = event {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('r') => refresh_health_in_state(&mut state, http, server).await,
                    KeyCode::Char('b') => {
                        run_benchmark_in_state(&mut state, http, server, args).await
                    }
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

async fn run_benchmark_in_state(state: &mut TuiState, http: &Client, server: &str, args: &TuiArgs) {
    state.benchmark_running = true;
    state.benchmark_error = None;
    state.benchmark_report_json = None;
    let bench_args = BenchArgs {
        size_gib: args.bench_size_gib,
        file_path: args.bench_file_path.clone(),
        tenant_id: "tui-tenant".to_owned(),
        user_id: "tui-user".to_owned(),
        destination_uri: "local://benchmark/metra-tui-bench.bin".to_owned(),
        quic_addr: None,
        io_chunk_bytes: args.bench_io_chunk_bytes,
        lanes: args.bench_lanes,
        no_disk: args.bench_no_disk,
        auto_lanes_report: None,
        lane_policy: None,
        auto_runtime_report: args.auto_runtime_report.clone(),
        runtime_policy: args.runtime_policy.clone(),
        runtime_policy_out: None,
        runtime_profile: None,
        file_read_pipeline_depth: None,
    };

    match run_benchmark_quiet(http, server, bench_args).await {
        Ok(report) => {
            state.benchmark_report_json = serde_json::to_string_pretty(&report).ok();
            state.benchmark_error = None;
        }
        Err(err) => {
            state.benchmark_error = Some(err.to_string());
        }
    }
    state.benchmark_running = false;
}

fn render_tui(frame: &mut ratatui::Frame<'_>, state: &TuiState, server: &str, args: &TuiArgs) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Length(7),
            Constraint::Min(8),
        ])
        .split(frame.area());

    let header = Paragraph::new(Text::from(vec![
        Line::from("Metra Client TUI"),
        Line::from(format!("Server: {server}")),
        Line::from("Keys: r=refresh health, b=run benchmark, q=quit"),
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

    let benchmark_meta = format!(
        "size={}GiB lanes={} chunk={} no_disk={} auto_report={} runtime_policy={}",
        args.bench_size_gib,
        args.bench_lanes,
        args.bench_io_chunk_bytes,
        args.bench_no_disk,
        args.auto_runtime_report
            .as_ref()
            .map(|value| value.display().to_string())
            .unwrap_or_else(|| "none".to_owned()),
        args.runtime_policy
            .as_ref()
            .map(|value| value.display().to_string())
            .unwrap_or_else(|| "none".to_owned())
    );
    let benchmark_meta_box = Paragraph::new(benchmark_meta)
        .block(
            Block::default()
                .title("Benchmark Config")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(benchmark_meta_box, chunks[2]);

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
    frame.render_widget(body, chunks[3]);

    let benchmark_body = if state.benchmark_running {
        "benchmark running...".to_owned()
    } else if let Some(error) = &state.benchmark_error {
        format!("benchmark error: {error}")
    } else if let Some(report) = &state.benchmark_report_json {
        report.clone()
    } else {
        "no benchmark run yet".to_owned()
    };
    let benchmark_box = Paragraph::new(benchmark_body)
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .title("Last Benchmark")
                .borders(Borders::ALL),
        );
    frame.render_widget(benchmark_box, chunks[4]);
}
