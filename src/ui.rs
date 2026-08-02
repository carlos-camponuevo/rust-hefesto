//! Minimal arrow-key menu: ↑↓ move, → / Enter select, ← / Esc go back,
//! typing filters, Backspace edits the filter, Ctrl-C aborts.
//! (inquire's Select can't remap keys, so this is a small custom picker
//! on the same crossterm backend.)

use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute, queue,
    style::Print,
    terminal,
};
use std::io::{Write, stdout};

pub enum Pick {
    Item(usize),
    Back,
}

struct RawGuard;
impl RawGuard {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(RawGuard)
    }
}
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

pub fn select(title: &str, items: &[String]) -> Result<Pick> {
    let _guard = RawGuard::new()?;
    let mut out = stdout();
    let mut filter = String::new();
    let mut pos: usize = 0;
    let mut rendered: u16 = 0;

    loop {
        let needle = filter.to_lowercase();
        let visible: Vec<(usize, &String)> = items
            .iter()
            .enumerate()
            .filter(|(_, s)| needle.is_empty() || s.to_lowercase().contains(&needle))
            .collect();
        if pos >= visible.len() {
            pos = visible.len().saturating_sub(1);
        }

        if rendered > 0 {
            execute!(
                out,
                cursor::MoveUp(rendered),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )?;
        }
        let mut lines: u16 = 0;
        let ftxt = if filter.is_empty() {
            String::new()
        } else {
            format!("  [filter: {filter}]")
        };
        queue!(
            out,
            Print(format!(
                "? {title}{ftxt}  (↑↓ move · →/enter select · ← back)\r\n"
            ))
        )?;
        lines += 1;
        for (i, (_, s)) in visible.iter().enumerate() {
            let marker = if i == pos { "❯" } else { " " };
            queue!(out, Print(format!("{marker} {s}\r\n")))?;
            lines += 1;
        }
        if visible.is_empty() {
            queue!(out, Print("  (no match — Backspace to edit filter)\r\n"))?;
            lines += 1;
        }
        out.flush()?;
        rendered = lines;

        if let Event::Key(k) = event::read()? {
            match k.code {
                KeyCode::Up => {
                    if !visible.is_empty() {
                        pos = if pos == 0 { visible.len() - 1 } else { pos - 1 };
                    }
                }
                KeyCode::Down => {
                    if !visible.is_empty() {
                        pos = (pos + 1) % visible.len();
                    }
                }
                KeyCode::Enter | KeyCode::Right => {
                    if let Some((orig, _)) = visible.get(pos) {
                        return Ok(Pick::Item(*orig));
                    }
                }
                KeyCode::Left | KeyCode::Esc => return Ok(Pick::Back),
                KeyCode::Backspace => {
                    filter.pop();
                }
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    anyhow::bail!("interrupted (Ctrl-C)")
                }
                KeyCode::Char(c) => {
                    filter.push(c);
                    pos = 0;
                }
                _ => {}
            }
        }
    }
}
