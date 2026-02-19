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
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const DB_FILE: &str = "typing_data.db";

// The in-memory representation of the statistics
type Stats = HashMap<(Option<char>, char), HashMap<char, u32>>;

enum TestMode {
    Sentence,
    WordsTimed,
    WordsCount,
}

enum AppMode {
    Typing,
    Stats,
}

struct AppState {
    text: Vec<char>,
    typed: Vec<char>,
    raw_keystrokes: String,
    start_time: Option<Instant>,
    duration: Option<Duration>,
    finished: bool,
    stats: Stats,
    mode: AppMode,
    lifetime_stats: LifetimeStats,
    key_error_stats: KeyErrorStats,
    selected_char: Option<char>,
    available_words: Vec<String>,
    test_mode: TestMode,
    word_count_target: Option<usize>,
}
struct LifetimeStats {
    total_sessions: u32,
    average_wpm: f64,
    average_accuracy: f64,
}

struct KeyErrorStats {
    char_error_counts: HashMap<char, u32>,
    most_common_prev_char: HashMap<char, Option<char>>,
    most_common_typed_char: HashMap<char, char>,
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

fn load_lifetime_stats(conn: &Connection) -> Result<LifetimeStats> {
    let mut stmt = conn.prepare("SELECT COUNT(*), AVG(wpm), AVG(accuracy) FROM sessions")?;
    let stats = stmt.query_row([], |row| {
        Ok(LifetimeStats {
            total_sessions: row.get(0)?,
            average_wpm: row.get(1)?,
            average_accuracy: row.get(2)?,
        })
    })?;
    Ok(stats)
}

fn load_key_error_stats(conn: &Connection) -> Result<KeyErrorStats> {
    let mut char_error_counts = HashMap::new();
    let mut most_common_prev_char = HashMap::new();
    let mut most_common_typed_char = HashMap::new();

    // Load total error counts per char
    let mut stmt = conn.prepare("SELECT expected_char, SUM(error_count) FROM error_stats GROUP BY expected_char")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?.chars().next().unwrap(), row.get::<_, u32>(1)?))
    })?;
    for row in rows {
        let (expected_char, count) = row?;
        char_error_counts.insert(expected_char, count);
    }

    // Load most common previous char for each expected char
    let mut stmt_prev = conn.prepare(
        "SELECT
            expected_char,
            previous_char,
            SUM(error_count) AS total_errors
        FROM
            error_stats
        GROUP BY
            expected_char, previous_char
        ORDER BY
            expected_char, total_errors DESC;"
    )?;
    let rows_prev = stmt_prev.query_map([], |row| {
        let expected: String = row.get(0)?;
        let prev: Option<String> = row.get(1)?;
        Ok((expected.chars().next().unwrap(), prev.and_then(|s| s.chars().next())))
    })?;

    let mut current_expected_char: Option<char> = None;
    for row in rows_prev {
        let (expected_char, prev_char) = row?;
        if current_expected_char != Some(expected_char) {
            most_common_prev_char.insert(expected_char, prev_char);
            current_expected_char = Some(expected_char);
        }
    }

    // Load most common typed char for each expected char
    let mut stmt_typed = conn.prepare(
        "SELECT
            expected_char,
            typed_char,
            SUM(error_count) AS total_errors
        FROM
            error_stats
        GROUP BY
            expected_char, typed_char
        ORDER BY
            expected_char, total_errors DESC;"
    )?;
    let rows_typed = stmt_typed.query_map([], |row| {
        let expected: String = row.get(0)?;
        let typed: String = row.get(1)?;
        Ok((expected.chars().next().unwrap(), typed.chars().next().unwrap()))
    })?;

    current_expected_char = None;
    for row in rows_typed {
        let (expected_char, typed_char) = row?;
        if current_expected_char != Some(expected_char) {
            most_common_typed_char.insert(expected_char, typed_char);
            current_expected_char = Some(expected_char);
        }
    }

    Ok(KeyErrorStats { char_error_counts, most_common_prev_char, most_common_typed_char })
}

// --- Application Logic ---

fn load_words_from_files() -> Option<Vec<String>> {
    static WORD_CACHE: OnceLock<Option<Vec<String>>> = OnceLock::new();

    WORD_CACHE
        .get_or_init(|| {
            let paths: Vec<_> = glob("sentences/*.txt").ok()?.flatten().collect();
            if paths.is_empty() {
                return None;
            }
            let mut all_words = Vec::new();
            for path in paths {
                let content = fs::read_to_string(path).ok()?;
                let words = content
                    .split_whitespace()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                all_words.extend(words);
            }
            if all_words.is_empty() {
                None
            } else {
                Some(all_words)
            }
        })
        .clone()
}

fn generate_text(
    available_words: &Vec<String>,
    test_mode: &TestMode,
    word_count_target: Option<usize>,
) -> Vec<char> {
    let mut rng = rand::rngs::ThreadRng::default();
    let mut generated_text = String::new();

    match test_mode {
        TestMode::Sentence => {
            let sentence = available_words.choose(&mut rng).unwrap_or(&"No words found.".to_string()).clone();
            generated_text.push_str(&sentence);
        },
        TestMode::WordsCount => {
            let target_count = word_count_target.unwrap_or(10); // Default to 10 words
            for i in 0..target_count {
                let word = available_words.choose(&mut rng).unwrap();
                generated_text.push_str(word);
                if i < target_count - 1 {
                    generated_text.push(' ');
                }
            }
        },
        TestMode::WordsTimed => {
            // For timed mode, generate a sufficiently long text
            // For now, let's generate a fixed number of words
            let num_words = 50; // Generate 50 words for timed mode
            for i in 0..num_words {
                let word = available_words.choose(&mut rng).unwrap();
                generated_text.push_str(word);
                if i < num_words - 1 {
                    generated_text.push(' ');
                }
            }
        }
    }
    generated_text.chars().collect()
}

impl AppState {
    fn new(stats: Stats, conn: &Connection) -> Self {
        let available_words = load_words_from_files().unwrap_or_else(|| vec!["No words found.".to_string()]);
        let test_mode = TestMode::Sentence; // Default to sentence mode for now
        let word_count_target = None;

        let text = generate_text(&available_words, &test_mode, word_count_target);

        Self {
            text,
            typed: Vec::new(),
            raw_keystrokes: String::new(),
            start_time: None,
            duration: None,
            finished: false,
            stats,
            mode: AppMode::Typing,
            lifetime_stats: load_lifetime_stats(conn).unwrap_or(LifetimeStats { total_sessions: 0, average_wpm: 0.0, average_accuracy: 0.0 }),
            key_error_stats: load_key_error_stats(conn).unwrap_or(KeyErrorStats { char_error_counts: HashMap::new(), most_common_prev_char: HashMap::new(), most_common_typed_char: HashMap::new() }),
            selected_char: None,
            available_words,
            test_mode,
            word_count_target,
        }
    }

    fn reset(&mut self, conn: &Connection) {
        
        self.text = generate_text(&self.available_words, &self.test_mode, self.word_count_target);
        self.typed.clear();
        self.raw_keystrokes.clear();
        self.start_time = None;
        self.duration = None;
        self.finished = false;
        self.selected_char = None;
        self.lifetime_stats = load_lifetime_stats(conn).unwrap_or(LifetimeStats { total_sessions: 0, average_wpm: 0.0, average_accuracy: 0.0 });
        self.key_error_stats = load_key_error_stats(conn).unwrap_or(KeyErrorStats { char_error_counts: HashMap::new(), most_common_prev_char: HashMap::new(), most_common_typed_char: HashMap::new() });
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
    let mut app_state = AppState::new(stats, &conn);

    let result = run(&mut terminal, &mut app_state, &conn);

    let save_res = save_stats_to_db(&conn, &app_state.stats);
    let restore_res = restore_terminal(&mut terminal);

    save_res?;
    restore_res?;
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
) -> anyhow::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(());
                }

                match key.code {
                    KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.mode = match app.mode {
                            AppMode::Typing => AppMode::Stats,
                            AppMode::Stats => AppMode::Typing,
                        };
                        // Reset selected char when switching modes
                        app.selected_char = None;
                    }
                    KeyCode::Esc => app.reset(&conn),
                    _ => {
                        match app.mode {
                            AppMode::Typing => {
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
                                            save_session_to_db(conn, app)?;
                                        }
                                    }
                                    KeyCode::Backspace => {
                                        if !app.finished {
                                            app.raw_keystrokes.push('\u{8}');
                                            app.typed.pop();
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            AppMode::Stats => {
                                let alphabet: Vec<char> = ('a'..='z').collect();
                                match key.code {
                                    KeyCode::Left => {
                                        if let Some(c) = app.selected_char {
                                            if let Some(index) = alphabet.iter().position(|&x| x == c) {
                                                app.selected_char = Some(alphabet[(index + alphabet.len() - 1) % alphabet.len()]);
                                            }
                                        } else {
                                            app.selected_char = Some(alphabet[0]); // Select 'a' if nothing selected
                                        }
                                    }
                                    KeyCode::Right => {
                                        if let Some(c) = app.selected_char {
                                            if let Some(index) = alphabet.iter().position(|&x| x == c) {
                                                app.selected_char = Some(alphabet[(index + 1) % alphabet.len()]);
                                            }
                                        } else {
                                            app.selected_char = Some(alphabet[0]); // Select 'a' if nothing selected
                                        }
                                    }
                                    // For Up/Down, we can implement a grid navigation later if needed.
                                    // For now, just left/right.
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn ui(frame: &mut ratatui::Frame, app: &AppState) {
    match app.mode {
        AppMode::Typing => {
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
        AppMode::Stats => {
            let main_layout = Layout::new(
                Direction::Vertical,
                [Constraint::Percentage(50), Constraint::Percentage(50)],
            )
            .split(frame.area());

            let lifetime_stats_text = format!(
                "Lifetime Statistics:\n\nTotal Sessions: {}\nAverage WPM: {:.2}\nAverage Accuracy: {:.2}%\n\nPress 'Ctrl-S' to return to typing.",
                app.lifetime_stats.total_sessions,
                app.lifetime_stats.average_wpm,
                app.lifetime_stats.average_accuracy
            );
            let lifetime_stats_paragraph = Paragraph::new(lifetime_stats_text)
                .block(Block::default().borders(Borders::ALL).title("Lifetime Statistics"));
            frame.render_widget(lifetime_stats_paragraph, main_layout[0]);

            let alphabet_layout = Layout::new(
                Direction::Vertical,
                [Constraint::Length(3), Constraint::Min(0)],
            )
            .split(main_layout[1]);

            let mut alphabet_spans: Vec<Span> = Vec::new();
            for c in 'a'..='z' {
                let error_count = *app.key_error_stats.char_error_counts.get(&c).unwrap_or(&0);
                let color = if error_count > 10 {
                    Color::Red
                } else if error_count > 5 {
                    Color::Yellow
                } else if error_count > 0 {
                    Color::Green
                } else {
                    Color::Gray
                };
                let mut style = Style::default().fg(color);
                if app.selected_char == Some(c) {
                    style = style.bg(Color::DarkGray);
                }
                alphabet_spans.push(Span::styled(c.to_string(), style));
                alphabet_spans.push(Span::raw(" ")); // Space between letters
            }

            let alphabet_paragraph = Paragraph::new(Line::from(alphabet_spans))
                .block(Block::default().borders(Borders::ALL).title("Error Frequency (a-z)"));
            frame.render_widget(alphabet_paragraph, alphabet_layout[0]);

            if let Some(selected_char) = app.selected_char {
                let total_errors = app.key_error_stats.char_error_counts.get(&selected_char).unwrap_or(&0);
                let common_prev = app.key_error_stats.most_common_prev_char.get(&selected_char).and_then(|&c| c).map(|c| c.to_string()).unwrap_or_else(|| "N/A".to_string());
                let common_typed = app.key_error_stats.most_common_typed_char.get(&selected_char).map(|&c| c.to_string()).unwrap_or_else(|| "N/A".to_string());

                let detailed_stats_text = format!(
                    "Details for '{}':\n\nTotal Errors: {}\nMost common char before: {}\nMost common char typed instead: {}",
                    selected_char,
                    total_errors,
                    common_prev,
                    common_typed
                );
                let detailed_stats_paragraph = Paragraph::new(detailed_stats_text)
                    .block(Block::default().borders(Borders::ALL).title("Character Details"));
                frame.render_widget(detailed_stats_paragraph, alphabet_layout[1]);
            } else {
                let help_text = "Use Left/Right arrow keys to select a character for details.";
                let help_paragraph = Paragraph::new(help_text)
                    .block(Block::default().borders(Borders::ALL).title("Instructions"));
                frame.render_widget(help_paragraph, alphabet_layout[1]);
            }
        }
    }
}
