//! notetui — a terminal Misskey client.
//!
//! This is a thin TUI frontend over the `notecli` library (the same crate
//! NoteDeck consumes). It reads the shared account DB, fetches a timeline via
//! `MisskeyClient`, and renders notes with ratatui. All Misskey logic lives in
//! notecli; this binary only owns terminal state and rendering.

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use notecli::api::MisskeyClient;
use notecli::db::Database;
use notecli::models::{Account, NormalizedNote, TimelineKey, TimelineOptions};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

/// Selectable timelines: (notecli timeline key, display label).
const TIMELINES: &[(&str, &str)] = &[
    ("home", "Home"),
    ("social", "Social"),
    ("local", "Local"),
    ("global", "Global"),
];

/// MiAuth permission scopes requested at login (mirrors notecli's `login`).
const LOGIN_PERMISSIONS: &[&str] = &[
    "read:account",
    "write:account",
    "read:drive",
    "write:drive",
    "read:favorites",
    "write:favorites",
    "read:following",
    "write:following",
    "read:mutes",
    "write:mutes",
    "read:notes",
    "write:notes",
    "read:notifications",
    "write:notifications",
    "read:reactions",
    "write:reactions",
    "write:votes",
];

#[tokio::main]
async fn main() -> Result<()> {
    // notecli stores tokens in the OS keychain and clears the DB copy, so we
    // must initialize the same credential store before reading or writing
    // credentials — otherwise keychain lookups fail and we fall back to an
    // empty DB token.
    notecli::keychain::init_store().context("failed to initialize credential store")?;

    let db_path = data_dir().join("notecli").join("notecli.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).ok(); // first-run: DB dir may not exist yet
    }
    let db = Database::open(&db_path)
        .with_context(|| format!("failed to open account DB at {}", db_path.display()))?;
    let client = MisskeyClient::new()?;

    // Subcommands run before the TUI (they print to stdout / read stdin).
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("login") => {
            let host = args
                .next()
                .context("usage: notetui login <HOST>  (e.g. misskey.io)")?;
            login(&db, &client, &host).await?;
            return Ok(());
        }
        Some("-h" | "--help") => {
            print_usage();
            return Ok(());
        }
        Some(other) => bail!("unknown command: {other} (try `notetui login <HOST>`)"),
        None => {}
    }

    // No subcommand → launch the TUI. Prompt for login on first run.
    let account = match db.load_accounts()?.into_iter().next() {
        Some(account) => account,
        None => {
            println!(
                "アカウントがありません。ログインするホストを入力してください (例: misskey.io):"
            );
            print!("> ");
            io::stdout().flush().ok();
            let mut host = String::new();
            io::stdin().read_line(&mut host)?;
            let host = host.trim();
            if host.is_empty() {
                bail!("ホストが入力されませんでした");
            }
            login(&db, &client, host).await?
        }
    };

    let (host, token) = notecli::get_credentials(&db, &account.id)?;
    let mut app = App::new(account, client, host, token);
    let mut terminal = ratatui::init();
    app.refresh().await;
    let result = app.run(&mut terminal).await;
    ratatui::restore();
    result
}

fn print_usage() {
    println!("notetui — terminal Misskey client\n");
    println!("USAGE:");
    println!("  notetui              # launch the TUI (prompts for login if needed)");
    println!("  notetui login <HOST> # authenticate an account (e.g. misskey.io)");
}

/// Embedded MiAuth login flow: prints the auth URL, waits for the user to
/// approve it in a browser, then persists the account. Reuses notecli's
/// library primitives so the account is shared with notecli/notedeck.
async fn login(db: &Database, client: &MisskeyClient, host: &str) -> Result<Account> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let url = format!(
        "https://{host}/miauth/{session_id}?name=notetui&permission={}",
        LOGIN_PERMISSIONS.join(",")
    );
    println!("以下のURLをブラウザで開いて認証してください:\n");
    println!("  {url}\n");
    print!("認証が完了したらEnterを押してください... ");
    io::stdout().flush().ok();
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;

    let auth = client.complete_auth(host, &session_id).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let account = Account {
        id: id.clone(),
        host: host.to_string(),
        token: auth.token.clone(),
        user_id: auth.user.id.clone(),
        username: auth.user.username.clone(),
        display_name: auth.user.name.clone(),
        avatar_url: auth.user.avatar_url.clone(),
        software: "misskey".to_string(),
    };
    db.upsert_account(&account)?;

    // Mirror notecli: store in keychain, verify, then clear the DB copy.
    if notecli::keychain::store_token(&id, &auth.token).is_ok()
        && notecli::keychain::get_token(&id).ok().flatten().is_some()
    {
        let _ = db.clear_token(&id);
    }

    println!("ログインしました: @{}@{}", account.username, host);
    Ok(account)
}

/// Same data dir notecli uses (see `dirs_data_dir` in notecli's commands/mod.rs).
fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".local/share")
    })
}

struct App {
    account: Account,
    client: MisskeyClient,
    host: String,
    token: String,
    tl_idx: usize,
    notes: Vec<NormalizedNote>,
    list_state: ListState,
    status: String,
    should_quit: bool,
}

impl App {
    fn new(account: Account, client: MisskeyClient, host: String, token: String) -> Self {
        Self {
            account,
            client,
            host,
            token,
            tl_idx: 0,
            notes: Vec::new(),
            list_state: ListState::default(),
            status: String::from("loading…"),
            should_quit: false,
        }
    }

    fn timeline(&self) -> TimelineKey {
        // TIMELINES は既知のベーシック TL 名のみなので parse は常に成功する
        TimelineKey::parse(TIMELINES[self.tl_idx].0).expect("basic timeline key")
    }

    /// Fetch the current timeline and reset the selection to the top.
    async fn refresh(&mut self) {
        self.status = format!("fetching {}…", TIMELINES[self.tl_idx].1);
        let opts = TimelineOptions::new(40, None, None);
        match self
            .client
            .get_timeline(
                &self.host,
                &self.token,
                &self.account.id,
                &self.timeline(),
                opts,
            )
            .await
        {
            Ok(notes) => {
                self.notes = notes;
                self.list_state
                    .select(if self.notes.is_empty() { None } else { Some(0) });
                self.status = format!("{} notes", self.notes.len());
            }
            Err(e) => {
                // notecli scrubs tokens from messages via safe_message().
                self.status = format!("error: {}", e.safe_message());
            }
        }
    }

    async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.should_quit {
            terminal.draw(|f| self.draw(f))?;
            self.handle_events().await?;
        }
        Ok(())
    }

    async fn handle_events(&mut self) -> Result<()> {
        // Short poll keeps the loop responsive without busy-waiting.
        if !event::poll(Duration::from_millis(150))? {
            return Ok(());
        }
        let Event::Key(key) = event::read()? else {
            return Ok(());
        };
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') | KeyCode::Home => self.select(0),
            KeyCode::Char('G') | KeyCode::End => {
                if !self.notes.is_empty() {
                    self.select(self.notes.len() - 1);
                }
            }
            KeyCode::Char('r') => self.refresh().await,
            KeyCode::Tab | KeyCode::Char('l') => {
                self.tl_idx = (self.tl_idx + 1) % TIMELINES.len();
                self.refresh().await;
            }
            KeyCode::BackTab | KeyCode::Char('h') => {
                self.tl_idx = (self.tl_idx + TIMELINES.len() - 1) % TIMELINES.len();
                self.refresh().await;
            }
            _ => {}
        }
        Ok(())
    }

    fn move_selection(&mut self, delta: isize) {
        if self.notes.is_empty() {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, self.notes.len() as isize - 1);
        self.list_state.select(Some(next as usize));
    }

    fn select(&mut self, idx: usize) {
        if !self.notes.is_empty() {
            self.list_state.select(Some(idx.min(self.notes.len() - 1)));
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Length(1), // header
            Constraint::Min(1),    // timeline
            Constraint::Length(1), // footer
        ])
        .split(f.area());

        self.draw_header(f, chunks[0]);
        self.draw_timeline(f, chunks[1]);
        self.draw_footer(f, chunks[2]);
    }

    fn draw_header(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let mut tabs: Vec<Span> = Vec::new();
        for (i, (_, label)) in TIMELINES.iter().enumerate() {
            let style = if i == self.tl_idx {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            tabs.push(Span::styled(format!(" {label} "), style));
            tabs.push(Span::raw(" "));
        }
        let acct = format!("@{}@{} ", self.account.username, self.host);
        let mut spans = vec![Span::styled(acct, Style::default().fg(Color::Yellow))];
        spans.extend(tabs);
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_timeline(&mut self, f: &mut Frame, area: ratatui::layout::Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", TIMELINES[self.tl_idx].1));

        // Empty state: show a hint instead of a bare box (loading / no notes).
        if self.notes.is_empty() {
            let msg = Paragraph::new(Line::from(Span::styled(
                format!("  {}", self.status),
                Style::default().fg(Color::DarkGray),
            )))
            .block(block);
            f.render_widget(msg, area);
            return;
        }

        let width = area.width.saturating_sub(2) as usize; // borders
        let items: Vec<ListItem> = self
            .notes
            .iter()
            .map(|n| ListItem::new(note_lines(n, width.max(8))))
            .collect();

        let list = List::new(items)
            .block(block)
            .highlight_style(Style::default().bg(Color::Rgb(40, 40, 60)))
            .highlight_symbol("▌");

        f.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn draw_footer(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let help = "q quit  j/k move  r refresh  Tab/h/l switch  g/G top/bottom";
        let line = Line::from(vec![
            Span::styled(
                format!(" {} ", self.status),
                Style::default().fg(Color::Black).bg(Color::Green),
            ),
            Span::raw("  "),
            Span::styled(help, Style::default().fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }
}

/// Render a single note as a block of lines: header, body (wrapped), footer.
fn note_lines(note: &NormalizedNote, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    // A pure renote (boost) carries no text of its own.
    if let Some(renote) = note.renote.as_deref() {
        if note.text.as_deref().unwrap_or("").is_empty() {
            lines.push(Line::from(Span::styled(
                format!("🔁 {} renoted", display_handle(note)),
                Style::default().fg(Color::Green),
            )));
            lines.extend(note_lines(renote, width.saturating_sub(2)));
            lines.push(Line::raw(""));
            return lines;
        }
    }

    lines.push(Line::from(vec![
        Span::styled(
            display_handle(note),
            Style::default().fg(Color::Cyan).bold(),
        ),
        Span::raw("  "),
        Span::styled(
            short_time(&note.created_at),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    if let Some(cw) = note.cw.as_deref().filter(|s| !s.is_empty()) {
        lines.push(Line::from(Span::styled(
            format!("⚠ {cw}"),
            Style::default().fg(Color::Yellow),
        )));
    }

    let body = note.text.as_deref().unwrap_or("");
    for raw_line in body.split('\n') {
        for wrapped in wrap(raw_line, width) {
            lines.push(Line::raw(wrapped));
        }
    }

    let reactions: i64 = note.reactions.values().sum();
    lines.push(Line::from(Span::styled(
        format!(
            "↩ {}  🔁 {}  ❤ {}",
            note.replies_count, note.renote_count, reactions
        ),
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::raw(""));
    lines
}

/// "Display Name @user@host" for a note's author.
fn display_handle(note: &NormalizedNote) -> String {
    let u = &note.user;
    let name = u
        .name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&u.username);
    match u.host.as_deref() {
        Some(h) if !h.is_empty() => format!("{name} @{}@{}", u.username, h),
        _ => format!("{name} @{}", u.username),
    }
}

/// ISO-8601 ("2026-06-18T12:34:56.789Z") → "06-18 12:34". Best-effort slicing,
/// no date library needed for a display hint.
fn short_time(iso: &str) -> String {
    let date = iso.get(5..10).unwrap_or("");
    let time = iso.get(11..16).unwrap_or("");
    format!("{date} {time}").trim().to_string()
}

/// Greedy word-wrap on whitespace; falls back to hard splits for long tokens.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if word.chars().count() > width {
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            let mut chunk = String::new();
            for ch in word.chars() {
                if chunk.chars().count() == width {
                    out.push(std::mem::take(&mut chunk));
                }
                chunk.push(ch);
            }
            line = chunk;
            continue;
        }
        let extra = if line.is_empty() { 0 } else { 1 };
        if line.chars().count() + extra + word.chars().count() > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}
