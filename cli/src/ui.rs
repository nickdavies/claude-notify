use std::io::{self, Write};

use chrono::Utc;
use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};

use crate::client::Approval;

/// What the user wants to do after a UI event.
pub enum Action {
    /// Approve the approval at the given index.
    Approve(usize),
    /// Deny the approval at the given index — enters reason input mode.
    StartDeny(usize),
    /// Navigate to the next item.
    Next,
    /// Navigate to the previous item.
    Prev,
    /// Quit the application.
    Quit,
    /// No action (unrecognized key, etc).
    None,
}

/// Mode of the UI.
pub enum Mode {
    /// Normal browsing mode.
    Normal,
    /// Typing a deny reason for the approval at the given index.
    DenyInput { index: usize, buffer: String },
}

/// Result of processing a key event in DenyInput mode.
pub enum DenyInputResult {
    /// User confirmed the deny reason.
    Confirm { index: usize, reason: String },
    /// User cancelled.
    Cancel,
    /// Still typing.
    Continue,
}

pub struct Ui {
    pub selected: usize,
    pub mode: Mode,
    pub status_message: Option<String>,
    server_url: String,
}

impl Ui {
    pub fn new(server_url: &str) -> Self {
        Self {
            selected: 0,
            mode: Mode::Normal,
            status_message: None,
            server_url: server_url.to_string(),
        }
    }

    pub fn enter(&self) -> io::Result<()> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        )?;
        Ok(())
    }

    pub fn leave(&self) -> io::Result<()> {
        let mut stdout = io::stdout();
        execute!(
            stdout,
            crossterm::cursor::Show,
            terminal::LeaveAlternateScreen
        )?;
        terminal::disable_raw_mode()?;
        Ok(())
    }

    /// Clamp selection index to valid range.
    pub fn clamp_selection(&mut self, count: usize) {
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    /// Handle a key event in normal mode. Returns the action to take.
    pub fn handle_normal_key(&self, key: KeyEvent, approval_count: usize) -> Action {
        if approval_count == 0 {
            return match key.code {
                KeyCode::Char('q') => Action::Quit,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
                _ => Action::None,
            };
        }

        match key.code {
            KeyCode::Char('a') | KeyCode::Char('1') => Action::Approve(self.selected),
            KeyCode::Char('d') | KeyCode::Char('2') => Action::StartDeny(self.selected),
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => Action::Next,
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k') => Action::Prev,
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
            _ => Action::None,
        }
    }

    /// Handle a key event in deny input mode. Mutates the buffer and returns result.
    pub fn handle_deny_key(&mut self, key: KeyEvent) -> DenyInputResult {
        let Mode::DenyInput { index, buffer } = &mut self.mode else {
            return DenyInputResult::Cancel;
        };
        let index = *index;

        match key.code {
            KeyCode::Enter => {
                let reason = std::mem::take(buffer);
                DenyInputResult::Confirm { index, reason }
            }
            KeyCode::Esc => DenyInputResult::Cancel,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                DenyInputResult::Cancel
            }
            KeyCode::Backspace => {
                buffer.pop();
                DenyInputResult::Continue
            }
            KeyCode::Char(c) => {
                buffer.push(c);
                DenyInputResult::Continue
            }
            _ => DenyInputResult::Continue,
        }
    }

    /// Render the full UI to the terminal.
    pub fn render(&self, approvals: &[Approval]) -> io::Result<()> {
        let mut stdout = io::stdout();
        let (cols, rows) = terminal::size()?;
        let width = cols as usize;

        queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;

        // Header
        let pending_text = format!("{} pending", approvals.len());
        let header_left = format!("Claude Review \u{2014} {}", self.server_url);
        let pad = width.saturating_sub(header_left.len() + pending_text.len());
        queue!(
            stdout,
            SetForegroundColor(Color::Cyan),
            SetAttribute(Attribute::Bold),
            Print(&header_left),
            Print(" ".repeat(pad)),
            Print(&pending_text),
            SetAttribute(Attribute::Reset),
            ResetColor,
            Print("\r\n"),
        )?;

        // Separator
        let separator: String = "\u{2501}".repeat(width);
        queue!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print(&separator),
            ResetColor,
            Print("\r\n"),
        )?;

        if approvals.is_empty() {
            queue!(
                stdout,
                Print("\r\n"),
                SetForegroundColor(Color::DarkGrey),
                Print("  Waiting for approvals..."),
                ResetColor,
                Print("\r\n"),
            )?;
        } else {
            let now = Utc::now();
            for (i, approval) in approvals.iter().enumerate() {
                let is_selected = i == self.selected;
                let age = format_age(now, approval.created_at);

                if is_selected {
                    // Selected indicator
                    queue!(
                        stdout,
                        Print("\r\n"),
                        SetForegroundColor(Color::Yellow),
                        SetAttribute(Attribute::Bold),
                        Print("\u{25b8} "),
                    )?;
                } else {
                    queue!(stdout, Print("\r\n"), Print("  "),)?;
                }

                // Summary line: project > tool_name                    age
                let summary = format!("{} \u{203a} {}", approval.project, approval.tool_name);
                let summary_pad = width.saturating_sub(summary.len() + age.len() + 4);
                if is_selected {
                    queue!(
                        stdout,
                        Print(&summary),
                        SetAttribute(Attribute::Reset),
                        ResetColor,
                        SetForegroundColor(Color::DarkGrey),
                        Print(" ".repeat(summary_pad)),
                        Print(&age),
                        ResetColor,
                    )?;
                } else {
                    queue!(
                        stdout,
                        SetForegroundColor(Color::White),
                        Print(&summary),
                        ResetColor,
                        SetForegroundColor(Color::DarkGrey),
                        Print(" ".repeat(summary_pad)),
                        Print(&age),
                        ResetColor,
                    )?;
                }
                queue!(stdout, Print("\r\n"))?;

                // Expanded detail for selected item
                if is_selected {
                    render_expanded(&mut stdout, approval, width)?;
                }
            }
        }

        // Bottom area: status message + separator + key hints
        // Calculate how many lines we've used and position at bottom
        let bottom_row = rows.saturating_sub(3);
        queue!(stdout, MoveTo(0, bottom_row))?;

        // Status message (if any)
        if let Some(msg) = &self.status_message {
            queue!(
                stdout,
                SetForegroundColor(Color::Red),
                Print("  "),
                Print(msg),
                ResetColor,
                Print("\r\n"),
            )?;
        } else {
            queue!(stdout, Print("\r\n"))?;
        }

        // Separator
        queue!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print(&separator),
            ResetColor,
            Print("\r\n"),
        )?;

        // Key hints or deny input
        match &self.mode {
            Mode::Normal => {
                if approvals.is_empty() {
                    queue!(
                        stdout,
                        SetForegroundColor(Color::DarkGrey),
                        Print(" (q)uit"),
                        ResetColor,
                    )?;
                } else {
                    queue!(
                        stdout,
                        Print(" "),
                        SetForegroundColor(Color::Green),
                        Print("(a)"),
                        ResetColor,
                        Print("pprove  "),
                        SetForegroundColor(Color::Red),
                        Print("(d)"),
                        ResetColor,
                        Print("eny  "),
                        SetForegroundColor(Color::DarkGrey),
                        Print("(Tab)"),
                        ResetColor,
                        Print(" next  "),
                        SetForegroundColor(Color::DarkGrey),
                        Print("(q)"),
                        ResetColor,
                        Print("uit"),
                    )?;
                }
            }
            Mode::DenyInput { buffer, .. } => {
                queue!(
                    stdout,
                    Print(" Reason (Enter to confirm, Esc to cancel): "),
                    SetForegroundColor(Color::Yellow),
                    Print(buffer),
                    Print("\u{2588}"),
                    ResetColor,
                )?;
            }
        }

        stdout.flush()?;
        Ok(())
    }
}

/// Render the expanded detail of a selected approval.
fn render_expanded(stdout: &mut io::Stdout, approval: &Approval, width: usize) -> io::Result<()> {
    let input_str = format_tool_input(&approval.tool_input);
    let max_line_width = width.saturating_sub(6); // "  │ " prefix

    for line in input_str.lines() {
        // Truncate long lines
        let display = if line.len() > max_line_width {
            format!("{}...", &line[..max_line_width.saturating_sub(3)])
        } else {
            line.to_string()
        };
        queue!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print("  \u{2502} "),
            ResetColor,
            Print(&display),
            Print("\r\n"),
        )?;
    }

    if let Some(context) = &approval.context {
        queue!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print("  \u{2502}"),
            ResetColor,
            Print("\r\n"),
            SetForegroundColor(Color::DarkGrey),
            Print("  \u{2502} "),
            SetForegroundColor(Color::Blue),
            Print("Context: "),
            ResetColor,
        )?;
        let ctx_display = if context.len() > max_line_width.saturating_sub(9) {
            format!("{}...", &context[..max_line_width.saturating_sub(12)])
        } else {
            context.clone()
        };
        queue!(stdout, Print(&ctx_display), Print("\r\n"))?;
    }

    Ok(())
}

/// Format tool_input as readable text. Shows key: value pairs for objects.
fn format_tool_input(input: &serde_json::Value) -> String {
    match input {
        serde_json::Value::Object(map) => {
            let mut lines = Vec::new();
            for (key, value) in map {
                let val_str = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                lines.push(format!("{key}: {val_str}"));
            }
            lines.join("\n")
        }
        other => other.to_string(),
    }
}

/// Format a duration between now and a timestamp as a human-readable age.
fn format_age(now: chrono::DateTime<Utc>, created: chrono::DateTime<Utc>) -> String {
    let secs = (now - created).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// Poll for a terminal event with the given timeout.
pub fn poll_event(timeout: std::time::Duration) -> io::Result<Option<Event>> {
    if event::poll(timeout)? {
        Ok(Some(event::read()?))
    } else {
        Ok(None)
    }
}
