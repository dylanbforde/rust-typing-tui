use chrono::Utc;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use glob::glob;
use rand::prelude::*;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};
use rusqlite::{params, Connection, Result};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

const DB_FILE: &str = "typing_data.db";

// The in-memory representation of the statistics
type Stats = HashMap<(Option<char>, char), HashMap<char, u32>>;

struct AppState {
    text: Vec<char>,
    typed: Vec<char>,
    raw_keystrokes: String,
    start_time: Option<Instant>,
    duration: Option<Duration>,
    finished: bool,
    stats: Stats,
}

// --- Database Functions ---

fn setup_database(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS error_stats (\n            previous_char  TEXT,\n            expected_char  TEXT NOT NULL,\n            typed_char     TEXT NOT NULL,\n            error_count    INTEGER NOT NULL,\n            PRIMARY KEY (previous_char, expected_char, typed_char)\n        )",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS sessions (\n            timestamp      INTEGER PRIMARY KEY,\n            target_text    TEXT NOT NULL,\n            typed_text     TEXT NOT NULL,\n            duration_ms    INTEGER,\n            wpm            REAL,\n            accuracy       REAL\n        )",
        [],
    )?;
    Ok(())
}

fn load_stats_from_db(conn: &Connection) -> Result<Stats> {
    let mut stmt = conn.prepare(
        "SELECT previous_char, expected_char, typed_char, error_count FROM error_stats",
    )?;
    let mut stats = Stats::new();
    let rows = stmt.query_map([], |row| {
        let prev: Option<String> = row.get(0)?;
        let prev_char = prev.and_then(|s| s.chars().next());
        Ok((
            (prev_char, row.get::<_, String>(1)?.chars().next().unwrap()),
            row.get::<_, String>(2)?.chars().next().unwrap(),
            row.get::<_, u32>(3)?,
        ))
    })?;

    for row in rows {
        let ((prev, expected), typed, count) = row?;
        *stats.entry((prev, expected)).or_default().entry(typed).or_default() += count;
    }
    Ok(stats)
}

fn save_stats_to_db(conn: &Connection, stats: &Stats) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for ((prev, expected), errors) in stats {
        for (typed, count) in errors {
            let prev_str = prev.map(|c| c.to_string());
            tx.execute(
                "INSERT OR REPLACE INTO error_stats (previous_char, expected_char, typed_char, error_count) \n                 VALUES (?1, ?2, ?3, ?4)",
                params![prev_str, expected.to_string(), typed.to_string(), count],
            )?;
        }
    }
    tx.commit()
}

fn save_session_to_db(conn: &Connection, app: &AppState) -> Result<()> {
    let target_text: String = app.text.iter().collect();
    let typed_text = &app.raw_keystrokes;
    conn.execute(
        "INSERT INTO sessions (timestamp, target_text, typed_text, duration_ms, wpm, accuracy) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            Utc::now().timestamp(),
            target_text,
            typed_text,
            app.duration.map(|d| d.as_millis() as u64),
            app.wpm(),
            app.accuracy(),
        ],
    )?;
    Ok(())
}

// --- Application Logic ---

fn get_text_from_files() -> Option<String> {
    let mut rng = rand::thread_rng();
    let paths: Vec<_> = glob("sentences/*.txt").ok()?.flatten().collect();
    if paths.is_empty() {
        return None;
    }
    let path = paths.choose(&mut rng)?;
    fs::read_to_string(path).ok()
}

impl AppState {
    fn new(stats: Stats) -> Self {
        let text = get_text_from_files().unwrap_or_else(|| "No sentences found in ./sentences folder.".to_string());
        Self {
            text: text.chars().collect(),
            typed: Vec::new(),
            raw_keystrokes: String::new(),
            start_time: None,
            duration: None,
            finished: false,
            stats,
        }
    }

    fn reset(&mut self) {
        let stats = std::mem::take(&mut self.stats);
        *self = Self::new(stats);
    }

    fn wpm(&self) -> f64 {
        self.duration.map_or(0.0, |d| {
            let elapsed_min = d.as_secs_f64() / 60.0;
            if elapsed_min == 0.0 { return 0.0; }
            let word_count = self.text.len() as f64 / 5.0;
            word_count / elapsed_min
        })
    }

    fn accuracy(&self) -> f64 {
        if self.typed.is_empty() { return 100.0; }
        let correct_chars = self.typed.iter().zip(&self.text).filter(|&(a, b)| a == b).count();
        (correct_chars as f64 / self.typed.len() as f64) * 100.0
    }
}

fn main() -> anyhow::Result<()> {
    let conn = Connection::open(DB_FILE)?;
    setup_database(&conn)?;
    let stats = load_stats_from_db(&conn)?;

    let mut terminal = setup_terminal()?;
    let mut app_state = AppState::new(stats);

    let result = run(&mut terminal, &mut app_state, &conn);

    save_stats_to_db(&conn, &app_state.stats)?;
    restore_terminal(&mut terminal)?;
    result?;
    Ok(())
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppState,
    conn: &Connection,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(());
                }

                match key.code {
                    KeyCode::Char(c) => {
                        if app.finished {
                            continue;
                        }
                        if app.start_time.is_none() {
                            app.start_time = Some(Instant::now());
                        }

                        app.raw_keystrokes.push(c);
                        let current_pos = app.typed.len();
                        if current_pos < app.text.len() {
                            let expected = app.text[current_pos];
                            if c != expected {
                                let prev = if current_pos > 0 { Some(app.text[current_pos - 1]) } else { None };
                                let context_stats = app.stats.entry((prev, expected)).or_default();
                                *context_stats.entry(c).or_default() += 1;
                            }
                        }

                        app.typed.push(c);

                        if app.typed.len() == app.text.len() {
                            app.finished = true;
                            app.duration = app.start_time.map(|st| st.elapsed());
                            save_session_to_db(conn, app).unwrap_or_default(); // Save session and ignore potential errors
                        }
                    }
                    KeyCode::Backspace => {
                        if !app.finished {
                            app.raw_keystrokes.push('\u{8}');
                            app.typed.pop();
                        }
                    }
                    KeyCode::Esc => app.reset(),
                    _ => {}
                }
            }
        }
    }
}

fn ui(frame: &mut ratatui::Frame, app: &AppState) {
    let main_layout = Layout::new(
        Direction::Vertical,
        if app.finished {
            [Constraint::Percentage(50), Constraint::Percentage(50)]
        } else {
            [Constraint::Min(0), Constraint::Length(3)]
        },
    )
    .split(frame.area());

    let mut spans: Vec<Span> = Vec::new();
    for (i, &char_to_type) in app.text.iter().enumerate() {
        let typed_char = app.typed.get(i);
        let span = if let Some(typed_char) = typed_char {
            if *typed_char == char_to_type {
                Span::styled(char_to_type.to_string(), Style::default().fg(Color::Green))
            } else {
                Span::styled(char_to_type.to_string(), Style::default().fg(Color::Red).bg(Color::DarkGray))
            }
        } else {
            Span::styled(char_to_type.to_string(), Style::default().fg(Color::Gray))
        };
        spans.push(span);
    }

    let text_to_type = Paragraph::new(Line::from(spans))
        .block(Block::default().borders(Borders::ALL).title("Text to Type"));
    frame.render_widget(text_to_type, main_layout[0]);

    if app.finished {
        let results_text = format!(
            "Finished!\nWPM: {:.2}\nAccuracy: {:.2}%\n\nPress 'Esc' to try another line or 'Ctrl-C' to quit.",
            app.wpm(),
            app.accuracy()
        );
        let results = Paragraph::new(results_text)
            .block(Block::default().borders(Borders::ALL).title("Results"));
        frame.render_widget(results, main_layout[1]);
    } else {
        let help_text = "Start typing the text above. Press 'Esc' to restart or 'Ctrl-C' to quit.";
        let help_paragraph = Paragraph::new(help_text)
            .block(Block::default().borders(Borders::ALL).title("Instructions"));
        frame.render_widget(help_paragraph, main_layout[1]);
    }
}