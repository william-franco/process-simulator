use crossterm::event::{self, Event as CEvent, KeyCode};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use rand::Rng;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph};
use ratatui::{Frame, Terminal};
use std::cmp;
use std::collections::HashMap;
use std::env;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// Process state (simulated)
#[derive(Debug, Clone)]
enum ProcStatus {
    Running,
    Finished,
    Paused,
    Stopped,
}

/// Represents one simulated process
#[derive(Debug, Clone)]
struct ProcState {
    id: usize,
    total_ms: u64,
    elapsed_ms: u64,
    status: ProcStatus,
    started_at: Option<Instant>,
    finished_at: Option<Instant>,
}

impl ProcState {
    /// Calculates progress (0.0–1.0)
    fn progress_ratio(&self) -> f64 {
        if self.total_ms == 0 {
            return 1.0;
        }
        (self.elapsed_ms as f64) / (self.total_ms as f64)
    }
}

/// Messages sent from worker threads to the UI
#[derive(Debug)]
enum UpdateMsg {
    Progress { id: usize, elapsed_ms: u64 },
    Finished { id: usize, elapsed_ms: u64 },
    Started { id: usize },
    Stopped { id: usize },
}

/// Input / Tick events
enum Event<I> {
    Input(I),
    Tick,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Read CLI arguments: number of processes and simulation duration
    let args: Vec<String> = env::args().collect();
    let num_procs: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5);
    let sim_duration_secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);

    // Channels for updates from threads to UI
    let (tx, rx) = mpsc::channel::<UpdateMsg>();
    let (event_tx, event_rx) = mpsc::channel();

    // Shared control flags
    let global_running = Arc::new(AtomicBool::new(false));
    let global_stop = Arc::new(AtomicBool::new(false));

    // Initialize process states with random durations
    let mut procs: HashMap<usize, ProcState> = HashMap::new();
    let mut rng = rand::thread_rng();
    for i in 0..num_procs {
        let total_ms = rng.gen_range(2000..8000);
        procs.insert(
            i,
            ProcState {
                id: i,
                total_ms,
                elapsed_ms: 0,
                status: ProcStatus::Paused,
                started_at: None,
                finished_at: None,
            },
        );
    }

    // Input thread: reads keyboard and generates tick events
    {
        let event_tx = event_tx.clone();
        thread::spawn(move || {
            let tick_rate = Duration::from_millis(250);
            let mut last_tick = Instant::now();
            loop {
                let timeout = tick_rate
                    .checked_sub(last_tick.elapsed())
                    .unwrap_or(Duration::ZERO);
                if event::poll(timeout).unwrap_or(false) {
                    if let CEvent::Key(key) = event::read().unwrap() {
                        let _ = event_tx.send(Event::Input(key));
                    }
                }
                if last_tick.elapsed() >= tick_rate {
                    let _ = event_tx.send(Event::Tick);
                    last_tick = Instant::now();
                }
            }
        });
    }

    // Setup terminal UI (alternate screen + raw mode)
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // UI state
    let mut selected: usize = 0;
    let mut started_at_global: Option<Instant> = None;
    let mut show_summary = false;
    let mut worker_handles: Vec<thread::JoinHandle<()>> = Vec::new();

    // Function to spawn process worker threads
    let spawn_workers = |tx: mpsc::Sender<UpdateMsg>,
                         global_running: Arc<AtomicBool>,
                         global_stop: Arc<AtomicBool>,
                         procs_snapshot: HashMap<usize, ProcState>|
     -> Vec<thread::JoinHandle<()>> {
        let mut handles = Vec::new();
        for (id, p) in procs_snapshot.into_iter() {
            let tx_cl = tx.clone();
            let global_running = Arc::clone(&global_running);
            let global_stop = Arc::clone(&global_stop);
            handles.push(thread::spawn(move || {
                let mut rng = rand::thread_rng();
                thread::sleep(Duration::from_millis(rng.gen_range(0..200)));
                let _ = tx_cl.send(UpdateMsg::Started { id });
                let mut elapsed = 0u64;
                let step = 100;
                while elapsed < p.total_ms {
                    if global_stop.load(Ordering::SeqCst) {
                        let _ = tx_cl.send(UpdateMsg::Stopped { id });
                        return;
                    }
                    if global_running.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(step));
                        elapsed = cmp::min(elapsed + step, p.total_ms);
                        let _ = tx_cl.send(UpdateMsg::Progress {
                            id,
                            elapsed_ms: elapsed,
                        });
                    } else {
                        thread::sleep(Duration::from_millis(150));
                    }
                }
                let _ = tx_cl.send(UpdateMsg::Finished {
                    id,
                    elapsed_ms: elapsed,
                });
            }));
        }
        handles
    };

    // Main UI + control loop
    let ui_tick_rate = Duration::from_millis(250);
    let mut last_ui_tick = Instant::now();
    let total_sim_duration = Duration::from_secs(sim_duration_secs);

    loop {
        // Process worker update messages
        while let Ok(msg) = rx.try_recv() {
            match msg {
                UpdateMsg::Started { id } => {
                    if let Some(ps) = procs.get_mut(&id) {
                        ps.status = ProcStatus::Running;
                        ps.started_at = Some(Instant::now());
                    }
                }
                UpdateMsg::Progress { id, elapsed_ms } => {
                    if let Some(ps) = procs.get_mut(&id) {
                        ps.elapsed_ms = cmp::min(elapsed_ms, ps.total_ms);
                        if ps.elapsed_ms >= ps.total_ms {
                            ps.status = ProcStatus::Finished;
                            ps.finished_at = Some(Instant::now());
                        } else if !matches!(ps.status, ProcStatus::Paused) {
                            ps.status = ProcStatus::Running;
                        }
                    }
                }
                UpdateMsg::Finished { id, elapsed_ms } => {
                    if let Some(ps) = procs.get_mut(&id) {
                        ps.elapsed_ms = elapsed_ms;
                        ps.status = ProcStatus::Finished;
                        ps.finished_at = Some(Instant::now());
                    }
                }
                UpdateMsg::Stopped { id } => {
                    if let Some(ps) = procs.get_mut(&id) {
                        ps.status = ProcStatus::Stopped;
                        ps.finished_at = Some(Instant::now());
                    }
                }
            }
        }

        // Handle keyboard and tick events
        match event_rx.recv() {
            Ok(Event::Input(key_event)) => match key_event.code {
                KeyCode::Char('q') => {
                    // Quit simulation
                    global_stop.store(true, Ordering::SeqCst);
                    global_running.store(false, Ordering::SeqCst);
                    for h in worker_handles.drain(..) {
                        let _ = h.join();
                    }
                    break;
                }
                KeyCode::Char('s') => {
                    // Start simulation
                    if !global_running.load(Ordering::SeqCst) {
                        global_stop.store(false, Ordering::SeqCst);
                        global_running.store(true, Ordering::SeqCst);
                        for (_, ps) in procs.iter_mut() {
                            if !matches!(ps.status, ProcStatus::Finished | ProcStatus::Stopped) {
                                ps.elapsed_ms = 0;
                                ps.started_at = None;
                                ps.finished_at = None;
                                ps.status = ProcStatus::Running;
                            }
                        }
                        let snapshot: HashMap<usize, ProcState> = procs
                            .iter()
                            .filter(|(_, p)| !matches!(p.status, ProcStatus::Finished))
                            .map(|(k, v)| (*k, v.clone()))
                            .collect();
                        started_at_global = Some(Instant::now());
                        worker_handles.extend(spawn_workers(
                            tx.clone(),
                            Arc::clone(&global_running),
                            Arc::clone(&global_stop),
                            snapshot,
                        ));
                    }
                }
                KeyCode::Char('p') => {
                    // Pause/resume toggle
                    let was_running = global_running.load(Ordering::SeqCst);
                    global_running.store(!was_running, Ordering::SeqCst);
                    for (_, ps) in procs.iter_mut() {
                        if !matches!(ps.status, ProcStatus::Finished | ProcStatus::Stopped) {
                            ps.status = if !was_running {
                                ProcStatus::Running
                            } else {
                                ProcStatus::Paused
                            };
                        }
                    }
                }
                KeyCode::Up => {
                    if selected > 0 {
                        selected -= 1;
                    }
                }
                KeyCode::Down => {
                    if selected + 1 < procs.len() {
                        selected += 1;
                    }
                }
                _ => {}
            },
            Ok(Event::Tick) => {
                // Stop simulation automatically after total time expires
                if let Some(start) = started_at_global {
                    if start.elapsed() >= total_sim_duration {
                        global_running.store(false, Ordering::SeqCst);
                        global_stop.store(true, Ordering::SeqCst);
                        for h in worker_handles.drain(..) {
                            let _ = h.join();
                        }
                        show_summary = true;
                    }
                }
            }
            Err(_) => break,
        }

        // Draw UI periodically
        if last_ui_tick.elapsed() >= ui_tick_rate {
            terminal.draw(|f| {
                draw_ui(
                    f,
                    &procs,
                    selected,
                    started_at_global,
                    sim_duration_secs,
                    show_summary,
                )
            })?;
            last_ui_tick = Instant::now();
        }
    }

    // Final render + cleanup
    terminal.draw(|f| {
        draw_ui(
            f,
            &procs,
            selected,
            started_at_global,
            sim_duration_secs,
            true,
        )
    })?;
    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    // Print textual summary after exit
    println!("\nFinal Summary:");
    let mut finished = 0usize;
    for id in 0..num_procs {
        if let Some(ps) = procs.get(&id) {
            let status_str = match ps.status {
                ProcStatus::Finished => "Finished",
                ProcStatus::Stopped => "Stopped",
                ProcStatus::Running => "Running",
                ProcStatus::Paused => "Paused",
            };
            println!(
                "Process {:>2} | expected: {:>4} ms | done: {:>4} ms | status: {}",
                id, ps.total_ms, ps.elapsed_ms, status_str
            );
            if matches!(ps.status, ProcStatus::Finished) {
                finished += 1;
            }
        }
    }
    println!("{} of {} processes finished.", finished, num_procs);
    println!("Simulation complete.");
    Ok(())
}

/// Draws the entire UI screen
fn draw_ui<B: ratatui::backend::Backend>(
    f: &mut Frame<B>,
    procs: &HashMap<usize, ProcState>,
    selected: usize,
    started_at_global: Option<Instant>,
    sim_duration_secs: u64,
    show_summary: bool,
) {
    // Split main layout vertically: header, list, bottom details
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(7),
        ])
        .split(f.size());

    // HEADER: global timer and instructions
    let elapsed_global = started_at_global
        .map(|s| s.elapsed())
        .unwrap_or(Duration::ZERO);
    let header_text = if show_summary {
        format!(
            "Simulation summary (press 'q' to exit). Total duration: {}s",
            sim_duration_secs
        )
    } else {
        format!(
            "Press 's' to start, 'p' to pause/resume, 'q' to quit | Elapsed: {:.1}s / {}s",
            elapsed_global.as_secs_f64(),
            sim_duration_secs
        )
    };
    let header =
        Paragraph::new(header_text).block(Block::default().borders(Borders::ALL).title("Header"));
    f.render_widget(header, chunks[0]);

    // PROCESS LIST
    let mut items = Vec::new();
    let mut ids: Vec<usize> = procs.keys().copied().collect();
    ids.sort_unstable();
    for id in ids.iter() {
        if let Some(ps) = procs.get(id) {
            let status_str = match ps.status {
                ProcStatus::Running => "Running",
                ProcStatus::Finished => "Finished",
                ProcStatus::Paused => "Paused",
                ProcStatus::Stopped => "Stopped",
            };
            let progress_pct = (ps.progress_ratio() * 100.0).clamp(0.0, 100.0);
            let line1 = format!(
                "PID {:>2} | {:>4} ms / {:>4} ms | {}",
                ps.id, ps.elapsed_ms, ps.total_ms, status_str
            );
            let line2 = format!("Progress: {:.0}%", progress_pct);
            items.push(ListItem::new(vec![
                ratatui::text::Line::from(line1),
                ratatui::text::Line::from(line2),
            ]));
        }
    }

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Processes"))
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Yellow),
        );
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, chunks[1], &mut state);

    // DETAIL AREA (bottom)
    let detail_chunk = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(chunks[2]);

    // LEFT: details of selected process
    let sel_proc = procs.get(&selected);
    let detail_paragraph = if let Some(ps) = sel_proc {
        let text = format!(
            "PID: {}\nStatus: {:?}\nElapsed: {} ms\nTotal: {} ms\nProgress: {:.1}%",
            ps.id,
            ps.status,
            ps.elapsed_ms,
            ps.total_ms,
            ps.progress_ratio() * 100.0
        );
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Process Details"),
        )
    } else {
        Paragraph::new("No process selected").block(
            Block::default()
                .borders(Borders::ALL)
                .title("Process Details"),
        )
    };
    f.render_widget(detail_paragraph, detail_chunk[0]);

    // RIGHT: progress gauge + help box
    let gauge_ratio = sel_proc
        .map(|ps| ps.progress_ratio().clamp(0.0, 1.0))
        .unwrap_or(0.0);
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title("Progress"))
        .gauge_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::ITALIC),
        )
        .ratio(gauge_ratio);
    f.render_widget(gauge, detail_chunk[1]);

    let help_text = Paragraph::new(
        "Controls:\n s = start | p = pause/resume | ↑/↓ = navigate | q = quit\n\nProgress bar shows selected process status.",
    )
    .block(Block::default().borders(Borders::ALL).title("Help"));
    f.render_widget(help_text, detail_chunk[1]);
}
