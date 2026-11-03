//! Human-readable terminal output; JSON is rendered separately.
use std::fmt::Write;

use chrono::Local;

use crate::{processes::RunningProcess, status::AccountStatus, usage::AccountUsage};

pub struct Theme {
    pub color: bool,
}

// Bright ANSI accents and light-gray secondary text stay legible on dark terminals.
impl Theme {
    fn paint(&self, value: &str, code: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{value}\x1b[0m")
        } else {
            value.into()
        }
    }

    /// Cyan marks the account of the current workspace; the tag names every
    /// workspace logged in to the account.
    fn heading(&self, name: &str, active: bool, workspaces: &[String]) -> String {
        let color = if active { "96" } else { "37" };
        format!(
            "  {} {}{}",
            self.paint("●", color),
            self.paint(&clean(name), "1;97"),
            if workspaces.is_empty() {
                String::new()
            } else {
                let tag = format!("[{}]", clean(&workspaces.join(", ")));
                format!("  {}", self.paint(&tag, color))
            }
        )
    }

    pub fn warmup(&self, rows: &[crate::warmup::WarmupResult]) -> String {
        let mut out = format!("\n  {}\n", self.paint("Account warmup", "1;96"));
        if rows.is_empty() {
            out.push_str("\n    No saved accounts.\n");
        }
        for row in rows {
            let color = match row.status {
                "warmed" => "92",
                "error" => "91",
                _ => "93",
            };
            let _ = writeln!(
                out,
                "\n{}\n    {}  {}",
                self.heading(&row.name, false, &[]),
                self.paint(row.status, color),
                clean(&row.message)
            );
        }
        out.push('\n');
        out
    }

    pub fn usage(&self, rows: &[AccountUsage]) -> String {
        let mut out = format!(
            "\n  {}  {}\n",
            self.paint("Account usage", "1;96"),
            self.paint(&format!("{} account(s)", rows.len()), "37")
        );
        if rows.is_empty() {
            out.push_str("\n    No saved accounts. Run `codex-switcher add NAME` first.\n");
        }
        for row in rows {
            let _ = writeln!(
                out,
                "\n{}",
                self.heading(
                    &format!(
                        "{} ({})",
                        row.name,
                        row.plan_type.as_deref().unwrap_or("unknown")
                    ),
                    row.is_active,
                    &row.workspaces,
                )
            );
            if let Some(error) = &row.error {
                let _ = writeln!(
                    out,
                    "    {}  {}",
                    self.paint(
                        if row.status == "error" {
                            "Error"
                        } else {
                            "Unavailable"
                        },
                        if row.status == "error" { "91" } else { "93" }
                    ),
                    clean(error)
                );
                continue;
            }
            if row.windows.is_empty() {
                let _ = writeln!(
                    out,
                    "\n    {}",
                    self.paint("Usage / reset not returned by server", "93")
                );
            }
            // Windows are sorted shortest first: 5h on the left, weekly on the right.
            for pair in row.windows.chunks(2) {
                let columns: Vec<[String; 4]> =
                    pair.iter().map(|window| self.window_cell(window)).collect();
                out.push('\n');
                for line in 0..4 {
                    let mut text = String::from("    ");
                    for (index, column) in columns.iter().enumerate() {
                        text.push_str(&column[line]);
                        if index + 1 < columns.len() {
                            let pad = COLUMN_WIDTH.saturating_sub(visible_len(&column[line]));
                            text.push_str(&" ".repeat(pad));
                        }
                    }
                    let _ = writeln!(out, "{}", text.trim_end());
                }
            }
        }
        out.push('\n');
        out
    }

    fn window_cell(&self, window: &crate::usage::UsageWindow) -> [String; 4] {
        let filled = (window.used_percent.clamp(0., 100.) / 100. * 20.).round() as usize;
        let bar = format!(
            "{}{}",
            self.paint(&"━".repeat(filled), "91"),
            self.paint(&"━".repeat(20 - filled), "92")
        );
        let countdown = window
            .resets_in_seconds
            .map(countdown)
            .unwrap_or_else(|| "unknown".into());
        let time = window
            .resets_at
            .map(|time| time.with_timezone(&Local).format("%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "unknown".into());
        [
            self.paint(&clean(&window.label), "1;97"),
            format!(
                "{bar}  {}",
                self.paint(&format!("{:.1}% used", window.used_percent), "91")
            ),
            format!(
                "{} {} {}",
                self.paint(&format!("{:.1}% left", window.remaining_percent), "92"),
                self.paint("· resets in", "37"),
                self.paint(&countdown, "96"),
            ),
            format!("{} {time}", self.paint("at", "37")),
        ]
    }

    pub fn accounts(&self, rows: &[AccountStatus]) -> String {
        let mut out = format!(
            "\n  {}  {}\n",
            self.paint("Saved accounts", "1;96"),
            self.paint(&format!("{} account(s)", rows.len()), "37")
        );
        if rows.is_empty() {
            out.push_str("\n    No saved accounts. Run `codex-switcher add NAME` first.\n");
        }
        for row in rows {
            let _ = writeln!(
                out,
                "\n{}",
                self.heading(
                    row.name.as_deref().unwrap_or("-"),
                    row.is_active,
                    &row.workspaces
                )
            );
            let _ = writeln!(
                out,
                "    {}",
                self.paint(
                    &format!("ID    {}", clean(row.id.as_deref().unwrap_or("-"))),
                    "37"
                )
            );
            let _ = writeln!(out, "    Auth  {}", row.auth_label());
            if let Some(email) = &row.email {
                let _ = writeln!(out, "    Email {}", clean(email));
            }
            if let Some(plan) = &row.plan_type {
                let _ = writeln!(out, "    Plan  {} (local)", clean(plan));
            }
            let code = match row.credential_status {
                "expired" | "missing" => "91",
                "expiring_soon" | "unknown" => "93",
                _ => "92",
            };
            let _ = writeln!(
                out,
                "    State {}",
                self.paint(&row.credential_status.replace('_', " "), code)
            );
        }
        let _ = writeln!(
            out,
            "\n  {}\n",
            self.paint(
                "Local credentials only · use `status` for usage and resets",
                "37"
            )
        );
        out
    }

    pub fn processes(&self, rows: &[RunningProcess]) -> String {
        let mut out = format!(
            "\n  {}  {}\n",
            self.paint("Running processes", "1;96"),
            self.paint(&format!("{} process(es)", rows.len()), "37")
        );
        if rows.is_empty() {
            out.push_str("\n    No running Codex, app-server, or ChatGPT processes found.\n\n");
            return out;
        }

        let label_width = rows
            .iter()
            .map(|process| process.kind.label().len())
            .max()
            .unwrap_or(0);
        for process in rows {
            let label = format!("{:<label_width$}", process.kind.label());
            let _ = writeln!(
                out,
                "    {}  {}",
                self.paint(&label, "1;97"),
                self.paint(&process.pid.to_string(), "93")
            );
        }
        out.push('\n');
        out
    }
}

const COLUMN_WIDTH: usize = 40;

// Display width ignoring ANSI color sequences; all rendered glyphs are single-width.
fn visible_len(value: &str) -> usize {
    let mut len = 0;
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            for next in chars.by_ref() {
                if next == 'm' {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

fn countdown(seconds: i64) -> String {
    if seconds <= 0 {
        return "reset time reached".into();
    }
    let units = [
        (seconds / 86400, "d"),
        (seconds % 86400 / 3600, "h"),
        (seconds % 3600 / 60, "m"),
        (seconds % 60, "s"),
    ];
    units
        .into_iter()
        .filter(|(value, _)| *value > 0)
        .take(2)
        .map(|(value, unit)| format!("{value}{unit}"))
        .collect::<Vec<_>>()
        .join(" ")
}

// Account names and server-provided text must not inject terminal escape codes.
fn clean(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_control() {
                ch.escape_default().to_string()
            } else {
                ch.to_string()
            }
        })
        .collect()
}
