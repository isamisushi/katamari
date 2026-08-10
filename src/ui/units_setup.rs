//! First-use setup for semantic-units grouping: a three-step popup
//! (agent CLI → model → effort) shown the first time `u`/`U` would
//! actually spawn an agent CLI in a session with no `[units]` config
//! anywhere. Spawning someone's authenticated, metered CLI without ever
//! having asked is the wrong default — the choice is made once, written to
//! the home config (see [`Selections::toml_section`]), and never asked
//! again. A cache *hit* never prompts: showing an already-computed
//! grouping costs nothing.
//!
//! The option lists are curated conveniences, not an authority — every
//! choice ends up as plain config the user can edit to any value the CLI
//! accepts (see `config::UnitsConfig`'s pass-through-verbatim rule). The
//! claude effort vocabulary matches `claude --help` (low…max); codex's
//! matches its `model_reasoning_effort` config values.

use crate::config::UnitsConfig;
use crate::groups::agent::{AgentCli, AgentKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

/// One choice row: what the list shows, and the value it stands for
/// (`None` = "no flag — the CLI's own default decides").
type Option_ = (&'static str, Option<&'static str>);

/// Model choices per CLI. Aliases only for claude (evergreen — they track
/// Anthropic's releases so this list can't go stale); codex has no alias
/// scheme, so its named entries are best-effort current and "CLI default"
/// leads as the recommendation.
fn model_options(kind: AgentKind) -> &'static [Option_] {
    match kind {
        AgentKind::Claude => &[
            ("sonnet (recommended)", Some("sonnet")),
            ("haiku — fastest", Some("haiku")),
            ("opus — most capable", Some("opus")),
            ("CLI default", None),
        ],
        AgentKind::Codex => &[
            ("CLI default (recommended)", None),
            ("gpt-5-codex", Some("gpt-5-codex")),
            ("gpt-5", Some("gpt-5")),
        ],
    }
}

fn effort_options(kind: AgentKind) -> &'static [Option_] {
    match kind {
        AgentKind::Claude => &[
            ("CLI default (recommended)", None),
            ("low", Some("low")),
            ("medium", Some("medium")),
            ("high", Some("high")),
            ("xhigh", Some("xhigh")),
            ("max", Some("max")),
        ],
        AgentKind::Codex => &[
            ("CLI default (recommended)", None),
            ("low", Some("low")),
            ("medium", Some("medium")),
            ("high", Some("high")),
        ],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Agent,
    Model,
    Effort,
}

/// What the wizard produced — applied to the live session via
/// [`Selections::apply`] and persisted via [`Selections::toml_section`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selections {
    pub agent: AgentKind,
    /// `None` = the "CLI default" choice (no `--model` flag).
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl Selections {
    /// Overlays the choices onto the session's live config. Only the
    /// chosen CLI's fields are touched — picking codex must not disturb
    /// the claude tuning that the fallback path would use if codex later
    /// disappeared from PATH.
    pub fn apply(&self, config: &mut UnitsConfig) {
        config.agent = Some(self.agent.binary().to_owned());
        match self.agent {
            AgentKind::Claude => {
                config.claude_model = self.model.clone();
                config.claude_effort = self.effort.clone();
            }
            AgentKind::Codex => {
                config.codex_model = self.model.clone();
                config.codex_effort = self.effort.clone();
            }
        }
    }

    /// The `[units]` block to append to the home config — minimal (only
    /// the chosen CLI's keys), with one deliberate exception: a claude
    /// "CLI default" model choice is written as `claude_model = ""`,
    /// because an *absent* key would be re-defaulted to
    /// [`crate::config::DEFAULT_CLAUDE_MODEL`] on the next load, silently
    /// overriding the very choice being saved (see `config::finalize`'s
    /// opt-out arm).
    pub fn toml_section(&self) -> String {
        use std::fmt::Write;
        // `toml::Value`'s Display renders a quoted, escaped TOML string —
        // vastly preferable to hand-rolled quoting even for values that
        // today only come from the curated lists above.
        let quoted = |s: &str| toml::Value::String(s.to_owned()).to_string();
        let mut out = String::from("\n[units]\n");
        let _ = writeln!(out, "agent = {}", quoted(self.agent.binary()));
        match self.agent {
            AgentKind::Claude => {
                let _ = writeln!(
                    out,
                    "claude_model = {}",
                    quoted(self.model.as_deref().unwrap_or(""))
                );
                if let Some(effort) = &self.effort {
                    let _ = writeln!(out, "claude_effort = {}", quoted(effort));
                }
            }
            AgentKind::Codex => {
                if let Some(model) = &self.model {
                    let _ = writeln!(out, "codex_model = {}", quoted(model));
                }
                if let Some(effort) = &self.effort {
                    let _ = writeln!(out, "codex_effort = {}", quoted(effort));
                }
            }
        }
        out
    }
}

pub enum SetupOutcome {
    Continue,
    Done(Selections),
}

pub struct UnitsSetupState {
    detected: Vec<AgentCli>,
    step: Step,
    selected: usize,
    agent: Option<AgentKind>,
    /// `Some` once the model step confirmed; the inner `None` is the
    /// "CLI default" choice.
    model: Option<Option<String>>,
}

impl UnitsSetupState {
    /// `detected` must be non-empty — the caller (`ui::mod`'s
    /// `ToggleUnits`/`RegenerateUnits` arms) reports "no agent CLI found"
    /// itself rather than opening a wizard with nothing to choose.
    pub fn new(detected: Vec<AgentCli>) -> Self {
        debug_assert!(!detected.is_empty());
        Self {
            detected,
            step: Step::Agent,
            selected: 0,
            agent: None,
            model: None,
        }
    }

    fn current_agent(&self) -> AgentKind {
        // Only meaningful past the Agent step, where it was recorded.
        self.agent.expect("agent chosen before model/effort steps")
    }

    fn entry_count(&self) -> usize {
        match self.step {
            Step::Agent => self.detected.len(),
            Step::Model => model_options(self.current_agent()).len(),
            Step::Effort => effort_options(self.current_agent()).len(),
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.entry_count() {
            self.selected += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn confirm(&mut self) -> SetupOutcome {
        match self.step {
            Step::Agent => {
                self.agent = Some(self.detected[self.selected].kind);
                self.step = Step::Model;
                self.selected = 0;
                SetupOutcome::Continue
            }
            Step::Model => {
                let (_, value) = model_options(self.current_agent())[self.selected];
                self.model = Some(value.map(str::to_owned));
                self.step = Step::Effort;
                self.selected = 0;
                SetupOutcome::Continue
            }
            Step::Effort => {
                let (_, value) = effort_options(self.current_agent())[self.selected];
                SetupOutcome::Done(Selections {
                    agent: self.current_agent(),
                    model: self.model.clone().expect("model step already confirmed"),
                    effort: value.map(str::to_owned),
                })
            }
        }
    }

    fn title(&self) -> &'static str {
        match self.step {
            Step::Agent => " units setup: agent CLI ",
            Step::Model => " units setup: model ",
            Step::Effort => " units setup: effort ",
        }
    }

    fn entry_labels(&self) -> Vec<String> {
        match self.step {
            Step::Agent => self
                .detected
                .iter()
                .map(|cli| format!("{} — {}", cli.kind.binary(), cli.path.display()))
                .collect(),
            Step::Model => model_options(self.current_agent())
                .iter()
                .map(|(label, _)| (*label).to_owned())
                .collect(),
            Step::Effort => effort_options(self.current_agent())
                .iter()
                .map(|(label, _)| (*label).to_owned())
                .collect(),
        }
    }
}

const HINT: &str = "Enter next · Esc cancel";

fn popup_rect(area: Rect, content_height: u16) -> Rect {
    let width = 52u16.min(area.width.saturating_sub(2)).max(20);
    let height = content_height.min(area.height.saturating_sub(2)).max(3);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

pub fn render(frame: &mut Frame, area: Rect, state: &UnitsSetupState) {
    let labels = state.entry_labels();
    // entries + intro + hint + borders
    let rect = popup_rect(area, labels.len() as u16 + 4);
    frame.render_widget(Clear, rect);

    let block = Block::default().borders(Borders::ALL).title(state.title());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines = vec![Line::from(Span::styled(
        "one-time choice — saved to ~/.config/katamari/config.toml",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    ))];
    lines.extend(labels.into_iter().enumerate().map(|(idx, label)| {
        let mut style = Style::default();
        if idx == state.selected {
            style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
        }
        Line::from(Span::styled(label, style))
    }));
    lines.push(Line::from(Span::styled(
        HINT,
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn both_clis() -> Vec<AgentCli> {
        vec![
            AgentCli {
                kind: AgentKind::Claude,
                path: PathBuf::from("/bin/claude"),
            },
            AgentCli {
                kind: AgentKind::Codex,
                path: PathBuf::from("/bin/codex"),
            },
        ]
    }

    fn confirm(state: &mut UnitsSetupState) -> SetupOutcome {
        state.confirm()
    }

    #[test]
    fn walks_agent_model_effort_and_reports_the_selections() {
        let mut state = UnitsSetupState::new(both_clis());
        state.move_down(); // codex
        assert!(matches!(confirm(&mut state), SetupOutcome::Continue));
        state.move_down(); // "gpt-5-codex" (index 1 for codex models)
        assert!(matches!(confirm(&mut state), SetupOutcome::Continue));
        state.move_down();
        state.move_down();
        state.move_down(); // "high" (last codex effort entry)
        match confirm(&mut state) {
            SetupOutcome::Done(selections) => {
                assert_eq!(selections.agent, AgentKind::Codex);
                assert_eq!(selections.model.as_deref(), Some("gpt-5-codex"));
                assert_eq!(selections.effort.as_deref(), Some("high"));
            }
            SetupOutcome::Continue => panic!("effort confirm must finish the wizard"),
        }
    }

    #[test]
    fn selection_clamps_within_each_steps_own_list() {
        let mut state = UnitsSetupState::new(both_clis());
        for _ in 0..10 {
            state.move_down();
        }
        assert_eq!(state.selected, 1, "agent list has two entries");
        state.confirm(); // codex
        for _ in 0..10 {
            state.move_down();
        }
        assert_eq!(state.selected, 2, "codex model list has three entries");
    }

    #[test]
    fn apply_touches_only_the_chosen_clis_fields() {
        let selections = Selections {
            agent: AgentKind::Codex,
            model: Some("gpt-5-codex".to_owned()),
            effort: Some("high".to_owned()),
        };
        let mut config = UnitsConfig::default();
        let claude_model_before = config.claude_model.clone();
        selections.apply(&mut config);
        assert_eq!(config.agent.as_deref(), Some("codex"));
        assert_eq!(config.codex_model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(config.codex_effort.as_deref(), Some("high"));
        assert_eq!(config.claude_model, claude_model_before);
    }

    #[test]
    fn toml_section_round_trips_through_the_config_parser() {
        let selections = Selections {
            agent: AgentKind::Claude,
            model: None, // "CLI default" — must persist as the "" opt-out
            effort: Some("high".to_owned()),
        };
        let section = selections.toml_section();
        let table: toml::Table = section.parse().expect("generated TOML must parse");
        let units = table["units"].as_table().unwrap();
        assert_eq!(units["agent"].as_str(), Some("claude"));
        assert_eq!(
            units["claude_model"].as_str(),
            Some(""),
            "CLI-default must be the explicit opt-out, or the next load re-defaults it"
        );
        assert_eq!(units["claude_effort"].as_str(), Some("high"));
    }

    #[test]
    fn codex_toml_section_omits_cli_default_fields_entirely() {
        let selections = Selections {
            agent: AgentKind::Codex,
            model: None,
            effort: None,
        };
        let section = selections.toml_section();
        let table: toml::Table = section.parse().unwrap();
        let units = table["units"].as_table().unwrap();
        assert_eq!(units["agent"].as_str(), Some("codex"));
        assert!(
            !units.contains_key("codex_model") && !units.contains_key("codex_effort"),
            "codex has no re-defaulting to defeat, so absent keys are the cleanest form"
        );
    }
}
