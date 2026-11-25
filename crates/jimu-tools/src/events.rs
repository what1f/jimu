use jimu_ai::CancelToken;
use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorStopReason {
    Exited,
    Timeout,
    Flood,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellStopReason {
    Exited,
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BackgroundToolEvent {
    MonitorLines {
        id: String,
        name: String,
        lines: String,
        log_path: PathBuf,
    },
    MonitorExited {
        id: String,
        name: String,
        exit_code: Option<i32>,
        reason: MonitorStopReason,
        tail_lines: String,
        log_path: PathBuf,
    },
    ShellExited {
        session_id: u32,
        command: String,
        exit_code: Option<i32>,
        reason: ShellStopReason,
    },
}

/// Synchronous event delivery. Enqueue the event and return promptly.
pub type BackgroundEventCallback = Arc<dyn Fn(BackgroundToolEvent) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeOutcome {
    Completed,
    Woken { reason: String, elapsed_ms: u64 },
    Cancelled,
}

/// Broadcasts new wake reasons to all current waiters. New waiters start with the
/// current channel version already seen, so old events never wake a later sleep.
#[derive(Clone, Debug)]
pub struct WakeGate(watch::Sender<String>);

impl Default for WakeGate {
    fn default() -> Self {
        Self(watch::channel(String::new()).0)
    }
}

impl WakeGate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn notify(&self, reason: impl Into<String>) {
        self.0.send_replace(reason.into());
    }

    pub fn wait_timeout<'a>(
        &self,
        duration: Duration,
        cancel: &'a CancelToken,
    ) -> impl Future<Output = WakeOutcome> + use<'a> {
        let mut reason = self.0.subscribe();
        let started = Instant::now();
        async move {
            let timer = async {
                match started.checked_add(duration) {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => WakeOutcome::Cancelled,
                Ok(()) = reason.changed() => WakeOutcome::Woken {
                    reason: reason.borrow_and_update().clone(),
                    elapsed_ms: elapsed_ms(started),
                },
                _ = timer => WakeOutcome::Completed,
            }
        }
    }
}

pub(crate) fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(all(not(target_os = "ios"), any(feature = "shell", feature = "monitor")))]
#[derive(Clone)]
pub(crate) struct BackgroundEvents {
    pub wake_gate: WakeGate,
    pub callback: Option<BackgroundEventCallback>,
}

#[cfg(all(not(target_os = "ios"), any(feature = "shell", feature = "monitor")))]
impl BackgroundEvents {
    pub fn emit(&self, event: BackgroundToolEvent) {
        let reason = match &event {
            BackgroundToolEvent::MonitorLines { name, .. } => format!("New output from {name}."),
            BackgroundToolEvent::MonitorExited { name, reason, .. } => match reason.hint() {
                Some(hint) => format!("Monitor {name} ended. {hint}"),
                None => format!("Monitor {name} ended."),
            },
            BackgroundToolEvent::ShellExited { reason, .. } => match reason {
                ShellStopReason::Timeout => "Background command timed out.".into(),
                ShellStopReason::Exited => "Background command finished.".into(),
            },
        };
        if let Some(callback) = &self.callback {
            // An unwinding host callback must not interrupt process cleanup.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(event)));
        }
        self.wake_gate.notify(reason);
    }
}

impl MonitorStopReason {
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::Exited => None,
            Self::Timeout => Some("Stopped at its timeout."),
            Self::Flood => Some(
                "Stopped due to frequent output. Filter the output with grep --line-buffered and restart.",
            ),
        }
    }
}

pub fn format_background_events_xml(events: &[BackgroundToolEvent]) -> String {
    let mut xml = String::new();
    for event in events {
        if !xml.is_empty() {
            xml.push('\n');
        }
        match event {
            BackgroundToolEvent::MonitorLines {
                id, name, lines, ..
            } => {
                monitor_tag(&mut xml, id, name);
                xml.push_str(">\n");
                escape_xml(&mut xml, lines, false);
                xml.push_str("\n</monitor_event>");
            }
            BackgroundToolEvent::MonitorExited {
                id,
                name,
                exit_code,
                reason,
                tail_lines,
                ..
            } => {
                monitor_tag(&mut xml, id, name);
                exit_attribute(&mut xml, *exit_code);
                let name = match reason {
                    MonitorStopReason::Exited => "exited",
                    MonitorStopReason::Timeout => "timeout",
                    MonitorStopReason::Flood => "flood",
                };
                let _ = writeln!(xml, " reason=\"{name}\">");
                if let Some(hint) = reason.hint() {
                    xml.push_str(hint);
                    xml.push('\n');
                }
                escape_xml(&mut xml, tail_lines, false);
                xml.push_str("\n</monitor_event>");
            }
            BackgroundToolEvent::ShellExited {
                session_id,
                command,
                exit_code,
                reason,
            } => {
                let _ = write!(xml, "<shell_event session_id=\"{session_id}\"");
                exit_attribute(&mut xml, *exit_code);
                let name = match reason {
                    ShellStopReason::Exited => "exited",
                    ShellStopReason::Timeout => "timeout",
                };
                let _ = writeln!(xml, " reason=\"{name}\">");
                escape_xml(&mut xml, command, false);
                xml.push_str("\n</shell_event>");
            }
        }
    }
    xml
}

fn monitor_tag(xml: &mut String, id: &str, name: &str) {
    xml.push_str("<monitor_event id=\"");
    escape_xml(xml, id, true);
    xml.push_str("\" name=\"");
    escape_xml(xml, name, true);
    xml.push('"');
}

fn exit_attribute(xml: &mut String, exit_code: Option<i32>) {
    if let Some(code) = exit_code {
        let _ = write!(xml, " exit=\"{code}\"");
    }
}

fn escape_xml(xml: &mut String, text: &str, attribute: bool) {
    for ch in text.chars() {
        match ch {
            '\n' | '\r' | '\t' if !attribute => xml.push(ch),
            '&' => xml.push_str("&amp;"),
            '<' => xml.push_str("&lt;"),
            '>' => xml.push_str("&gt;"),
            '"' => xml.push_str("&quot;"),
            '\'' => xml.push_str("&apos;"),
            '\t' => xml.push_str("&#9;"),
            '\r' => xml.push_str("&#13;"),
            '\n' => xml.push_str("&#10;"),
            '\u{0}'..='\u{1f}' | '\u{fffe}' | '\u{ffff}' => xml.push('\u{fffd}'),
            _ => xml.push(ch),
        }
    }
}
