//! Deterministic anomaly-alert foundations and calm mode.
//!
//! Rules still decide *what* is wrong; this module decides *what happens
//! next*. [`AlertDispatcher`] turns fresh Warning/Critical insights into
//! notifications with per-insight cooldown and deduplication, so a flapping
//! rule pages once, not every tick. Calm mode is the opposite direction:
//! under Critical pressure the UI sheds expensive work (trend rendering)
//! while critical alert dispatch keeps running — monitoring degrades
//! visibly, never silently.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::analyzer::{Insight, Severity};

/// Where alert notifications go.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AlertSink {
    /// `tracing::warn`, for journald/syslog pipelines.
    #[default]
    Log,
    /// One line per alert on stderr; stdout stays pure data.
    Stderr,
}

/// Tunable alert behavior. Cooldown is documented in seconds wherever it
/// surfaces so operators can reason about paging frequency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlertConfig {
    /// Minimum severity to notify on; Info insights never page.
    pub min_severity: Severity,
    /// Quiet period per insight ID before it may notify again.
    pub cooldown: Duration,
    /// When true, notifications are formatted and returned but never sent.
    pub dry_run: bool,
    pub sink: AlertSink,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            min_severity: Severity::Warning,
            cooldown: Duration::from_secs(60),
            dry_run: false,
            sink: AlertSink::Log,
        }
    }
}

/// Dispatches insight notifications with cooldown and dedup. `dispatch`
/// returns the messages it emitted (dry-run included) so behavior is
/// observable without capturing global sinks.
pub struct AlertDispatcher {
    config: AlertConfig,
    notified_at: HashMap<String, Instant>,
}

impl AlertDispatcher {
    pub fn new(config: AlertConfig) -> Self {
        Self {
            config,
            notified_at: HashMap::new(),
        }
    }

    /// Notify for fresh, severe-enough insights. Calm mode never reaches
    /// this function — dispatch runs regardless of UI load shedding, so
    /// critical updates are preserved by construction.
    pub fn dispatch(&mut self, insights: &[&Insight], now: Instant) -> Vec<String> {
        // Prune expired cooldowns so the map stays bounded.
        self.notified_at
            .retain(|_, notified| now.saturating_duration_since(*notified) < self.config.cooldown);
        let mut emitted = Vec::new();
        for insight in insights {
            if insight.severity < self.config.min_severity || insight.acknowledged {
                continue;
            }
            if let Some(notified) = self.notified_at.get(&insight.id)
                && now.saturating_duration_since(*notified) < self.config.cooldown
            {
                continue;
            }
            let message = format!(
                "[{}] {}: {}",
                severity_label(insight.severity),
                insight.title,
                insight.suggestion
            );
            let message = if self.config.dry_run {
                format!("[dry-run] {message}")
            } else {
                match self.config.sink {
                    AlertSink::Log => tracing::warn!("{message}"),
                    AlertSink::Stderr => eprintln!("{message}"),
                }
                message
            };
            self.notified_at.insert(insight.id.clone(), now);
            emitted.push(message);
        }
        emitted
    }
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Critical => "CRITICAL",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

/// Calm mode: shed expensive UI work under load. Rendering the trend chart
/// is the first thing paused; collection, analysis and alert dispatch are
/// untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CalmMode {
    pub active: bool,
}

impl CalmMode {
    /// Engage automatically under Critical pressure. Disengaging is always
    /// manual, so a flickering level cannot flap the UI.
    pub fn auto_engage(&mut self, critical: bool) {
        if critical {
            self.active = true;
        }
    }

    pub fn toggle(&mut self) {
        self.active = !self.active;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insight(id: &str, severity: Severity) -> Insight {
        Insight::new(id, severity, "title", "detail", "suggestion")
    }

    #[test]
    fn dispatch_notifies_once_per_cooldown() {
        let mut dispatcher = AlertDispatcher::new(AlertConfig {
            dry_run: true,
            ..AlertConfig::default()
        });
        let warning = insight("w1", Severity::Warning);
        let now = Instant::now();
        let first = dispatcher.dispatch(&[&warning], now);
        assert_eq!(first.len(), 1);
        assert!(first[0].contains("[dry-run]"));
        assert!(dispatcher.dispatch(&[&warning], now).is_empty());
        let later = now + Duration::from_secs(61);
        assert_eq!(dispatcher.dispatch(&[&warning], later).len(), 1);
    }

    #[test]
    fn info_insights_and_acknowledged_never_page() {
        let mut dispatcher = AlertDispatcher::new(AlertConfig {
            dry_run: true,
            ..AlertConfig::default()
        });
        let info = insight("i1", Severity::Info);
        assert!(dispatcher.dispatch(&[&info], Instant::now()).is_empty());
        let mut warning = insight("w1", Severity::Warning);
        warning.acknowledged = true;
        assert!(dispatcher.dispatch(&[&warning], Instant::now()).is_empty());
    }

    #[test]
    fn expired_cooldowns_are_pruned() {
        let mut dispatcher = AlertDispatcher::new(AlertConfig {
            dry_run: true,
            ..AlertConfig::default()
        });
        let warning = insight("w1", Severity::Warning);
        let now = Instant::now();
        dispatcher.dispatch(&[&warning], now);
        assert_eq!(dispatcher.notified_at.len(), 1);
        dispatcher.dispatch(&[&warning], now + Duration::from_secs(61));
        assert_eq!(dispatcher.notified_at.len(), 1);
    }

    #[test]
    fn calm_engages_on_critical_and_never_auto_releases() {
        let mut calm = CalmMode::default();
        calm.auto_engage(false);
        assert!(!calm.active);
        calm.auto_engage(true);
        assert!(calm.active);
        calm.auto_engage(false);
        assert!(calm.active);
        calm.toggle();
        assert!(!calm.active);
    }
}
