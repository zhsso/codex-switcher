//! Human-readable terminal output; JSON is rendered separately.
use std::fmt::Write;

use chrono::Local;

use crate::{daemon::State, processes::RunningProcess, status::AccountStatus, usage::AccountUsage};

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

    fn heading(&self, name: &str, active: bool) -> String {
        format!(
            "  {} {}{}",
            self.paint("●", if active { "96" } else { "37" }),
            self.paint(&clean(name), "1;97"),
            if active {
                format!("  {}", self.paint("[current]", "96"))
            } else {
                String::new()
            }
        )
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
            for window in &row.windows {
                let filled = (window.used_percent.clamp(0., 100.) / 100. * 20.).round() as usize;
                let bar = format!(
                    "{}{}",
                    self.paint(&"━".repeat(filled), "91"),
                    self.paint(&"━".repeat(20 - filled), "92")
                );
                let _ = writeln!(out, "\n    {}", self.paint(&clean(&window.label), "1;97"));
                let _ = writeln!(
                    out,
                    "      {bar}  {} used · {} left",
                    self.paint(&format!("{:.1}%", window.used_percent), "91"),
                    self.paint(&format!("{:.1}%", window.remaining_percent), "92")
                );
                let time = window
                    .resets_at
                    .map(|time| {
                        time.with_timezone(&Local)
                            .format("%Y-%m-%d %H:%M:%S %:z")
                            .to_string()
                    })
                    .unwrap_or_else(|| "unknown".into());
                let countdown = window
                    .resets_in_seconds
                    .map(countdown)
                    .unwrap_or_else(|| "unknown".into());
                let _ = writeln!(
                    out,
                    "      {} {} {}  {}",
                    self.paint("resets in", "37"),
                    self.paint(&countdown, "96"),
                    self.paint("at", "37"),
                    time,
                );
            }
        }
        out.push('\n');
        out
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
                self.heading(row.name.as_deref().unwrap_or("-"), row.is_active)
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

    pub fn daemon(&self, state: Option<&State>) -> String {
        let mut out = format!("\n  {}\n", self.paint("Auto-switch daemon", "1;96"));
        let Some(state) = state else {
            out.push_str(
                "\n    No checks recorded yet. Start it with `daemon install` or `daemon run`.\n\n",
            );
            return out;
        };
        let time = |value: Option<chrono::DateTime<chrono::Utc>>| {
            value.map_or("-".into(), |value| {
                value
                    .with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
        };
        let mut row = |label: &str, value: String| {
            let _ = writeln!(
                out,
                "    {}  {value}",
                self.paint(&format!("{label:<12}"), "37")
            );
        };
        row("PID", state.pid.to_string());
        row("Active", state.active.as_deref().map_or("-".into(), clean));
        row(
            "5h left",
            state
                .remaining_5h
                .map_or("-".into(), |value| format!("{value:.1}%")),
        );
        row("Last check", time(state.checked_at));
        row("Next check", time(state.next_check_at));
        if state.paused_until.is_some() {
            row("Paused until", self.paint(&time(state.paused_until), "93"));
        }
        if let Some(switch) = &state.last_switch {
            row(
                "Last switch",
                format!(
                    "{} -> {} at {}",
                    clean(&switch.from),
                    clean(&switch.to),
                    time(Some(switch.at))
                ),
            );
        }
        if let Some(error) = &state.last_error {
            row("Last error", self.paint(&clean(error), "91"));
        }
        out.push('\n');
        out
    }
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
