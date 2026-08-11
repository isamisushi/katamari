//! Resolves the command that speaks ACP for an agent — the same job
//! [`crate::lsp::adapter`] does for language servers and
//! [`crate::groups::agent`] does for headless grouping CLIs. Claude has
//! no native ACP mode (checked against claude 2.1.x); the canonical
//! adapter is the `@agentclientprotocol/claude-agent-acp` npm package,
//! whose binary name is `claude-agent-acp`. That package has already been
//! renamed twice (from `@zed-industries/claude-code-acp` via
//! `@zed-industries/claude-agent-acp`), which is exactly why an explicit
//! override is resolved first: when the ecosystem moves again, a user can
//! point at the new name without waiting for a katamari release.

use std::process::Command;

/// How the resolved command came to be — displayed by `agent-check` so a
/// surprising resolution is diagnosable from its output alone.
pub struct Resolution {
    pub command: Command,
    pub description: String,
}

/// Resolution order: explicit override → `claude-agent-acp` on PATH →
/// `npx -y @agentclientprotocol/claude-agent-acp` when npx exists.
///
/// The override string is split on whitespace (program + args) — enough
/// for every real adapter invocation (`npx -y <pkg>`, `gemini --acp`,
/// `node /path/to/index.js`), and a headless check flag doesn't warrant a
/// shell-quoting grammar. A managed npm install into katamari's own
/// prefix (the way language servers install) is the obvious next step
/// once this leaves spike stage; it deliberately isn't here yet.
pub fn resolve(override_cmd: Option<&str>) -> Result<Resolution, String> {
    if let Some(spec) = override_cmd {
        let mut parts = spec.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| "empty --adapter override".to_string())?;
        let mut command = Command::new(program);
        command.args(parts);
        return Ok(Resolution {
            command,
            description: format!("{spec} (explicit override)"),
        });
    }

    if let Some(path) = crate::lsp::adapter::which_on_path("claude-agent-acp") {
        let description = format!("{} (on PATH)", path.display());
        return Ok(Resolution {
            command: Command::new(path),
            description,
        });
    }

    if crate::lsp::adapter::which_on_path("npx").is_some() {
        let mut command = Command::new("npx");
        command.args(["-y", "@agentclientprotocol/claude-agent-acp"]);
        return Ok(Resolution {
            command,
            description: "npx -y @agentclientprotocol/claude-agent-acp".to_string(),
        });
    }

    Err(
        "no ACP adapter found — install one (npm i -g @agentclientprotocol/claude-agent-acp) \
         or pass --adapter <command>"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_override_wins_and_splits_program_from_args() {
        let resolution = resolve(Some("python3 fake_agent.py mode1")).unwrap();
        assert_eq!(resolution.command.get_program(), "python3");
        let args: Vec<_> = resolution.command.get_args().collect();
        assert_eq!(args, ["fake_agent.py", "mode1"]);
        assert!(resolution.description.contains("explicit override"));
    }

    #[test]
    fn an_empty_override_is_an_error_not_a_silent_fallback() {
        // Falling through to PATH resolution on an explicitly-passed empty
        // string would make a scripting typo pick a different agent.
        assert!(resolve(Some("  ")).is_err());
    }
}
