//! Agent definitions (Regents). Each agent is one Markdown file in
//! `.regents/agents/`: TOML front matter between `+++` lines, then the role.
//!
//! ```text
//! +++
//! name = "chief"
//! cli = "claude"
//! +++
//! Você é o Chief of Staff deste workspace.
//! ```

use std::path::Path;

use serde::Deserialize;

pub(crate) const AGENTS_DIR: &str = "agents";
const FRONT_MATTER_FENCE: &str = "+++";

/// The agent CLI that a thread of the agent runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AgentCli {
    Claude,
    Codex,
    Agy,
}

impl AgentCli {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Agy => "agy",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        [Self::Claude, Self::Codex, Self::Agy]
            .into_iter()
            .find(|cli| cli.as_str() == value)
    }
}

/// Where a thread of the agent runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Isolation {
    /// The workspace folder.
    #[default]
    Shared,
    /// A git worktree for each thread.
    Worktree,
}

impl Isolation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Worktree => "worktree",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        [Self::Shared, Self::Worktree]
            .into_iter()
            .find(|isolation| isolation.as_str() == value)
    }
}

/// How the agent CLI asks for permission.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Approval {
    /// The CLI asks as it does by default.
    #[default]
    Default,
    /// The CLI runs without asking.
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentDefinition {
    /// The mention target: `@name`.
    pub(crate) name: String,
    pub(crate) cli: AgentCli,
    /// Absent means the default model of the CLI.
    pub(crate) model: Option<String>,
    pub(crate) isolation: Isolation,
    pub(crate) approval: Approval,
    /// The Chief of Staff: it cannot be removed.
    pub(crate) reserved: bool,
    /// The body of the file: the instructions of the agent.
    pub(crate) role: String,
}

/// Unknown keys are ignored, so a file written by a newer Regents still loads.
#[derive(Deserialize)]
struct FrontMatter {
    name: String,
    cli: AgentCli,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    isolation: Isolation,
    #[serde(default)]
    approval: Approval,
    #[serde(default)]
    reserved: bool,
}

/// Parses the text of an agent file.
pub(crate) fn parse(text: &str) -> Result<AgentDefinition, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text.strip_prefix(FRONT_MATTER_FENCE).and_then(|rest| {
        rest.strip_prefix('\n')
            .or_else(|| rest.strip_prefix("\r\n"))
    }) else {
        return Err(format!(
            "the file must start with a {FRONT_MATTER_FENCE} line"
        ));
    };
    let (front_matter, role) = split_at_fence(rest)
        .ok_or_else(|| format!("the front matter has no closing {FRONT_MATTER_FENCE} line"))?;
    let front: FrontMatter = toml::from_str(front_matter).map_err(|err| err.to_string())?;
    if !is_agent_name(&front.name) {
        return Err(format!(
            "invalid agent name {:?}: use lowercase letters, digits, - and _, starting with a letter",
            front.name
        ));
    }
    Ok(AgentDefinition {
        name: front.name,
        cli: front.cli,
        model: front.model.filter(|model| !model.trim().is_empty()),
        isolation: front.isolation,
        approval: front.approval,
        reserved: front.reserved,
        role: role.trim().to_owned(),
    })
}

/// Splits `text` at the first line that is only the fence.
fn split_at_fence(text: &str) -> Option<(&str, &str)> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_end() == FRONT_MATTER_FENCE {
            return Some((&text[..offset], &text[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// An agent name: a mention target, `[a-z][a-z0-9_-]*`.
fn is_agent_name(name: &str) -> bool {
    name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

/// The agents of the workspace whose state directory is `state_dir`, sorted
/// by name. A file that does not parse is skipped with a warning, and so is a
/// second file with the same name.
pub(crate) fn list(state_dir: &Path) -> Vec<AgentDefinition> {
    let dir = state_dir.join(AGENTS_DIR);
    let mut files: Vec<_> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md") && path.is_file())
            .collect(),
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(dir = %dir.display(), %err, "cannot read the agent directory");
            }
            return Vec::new();
        }
    };
    files.sort();
    let mut agents: Vec<AgentDefinition> = Vec::new();
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .map_err(|err| err.to_string())
            .and_then(|text| parse(&text));
        match parsed {
            Ok(agent) if agents.iter().any(|other| other.name == agent.name) => {
                tracing::warn!(path = %path.display(), name = %agent.name, "duplicate agent name; skipped");
            }
            Ok(agent) => agents.push(agent),
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "invalid agent file; skipped");
            }
        }
    }
    agents.sort_by(|a, b| a.name.cmp(&b.name));
    agents
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regents::store::TempFolder;

    #[test]
    fn a_definition_reads_its_fields_and_role() {
        let agent = parse(
            "+++\nname = \"reviewer\"\ncli = \"codex\"\nmodel = \"gpt-5\"\nisolation = \"worktree\"\napproval = \"auto\"\n+++\nVocê revisa código.\n\nSeja breve.\n",
        )
        .unwrap();

        assert_eq!(
            agent,
            AgentDefinition {
                name: "reviewer".into(),
                cli: AgentCli::Codex,
                model: Some("gpt-5".into()),
                isolation: Isolation::Worktree,
                approval: Approval::Auto,
                reserved: false,
                role: "Você revisa código.\n\nSeja breve.".into(),
            }
        );
    }

    #[test]
    fn optional_fields_take_their_defaults() {
        let agent = parse("+++\nname = \"a\"\ncli = \"agy\"\nmodel = \"\"\n+++\n").unwrap();

        assert_eq!(agent.model, None);
        assert_eq!(agent.isolation, Isolation::Shared);
        assert_eq!(agent.approval, Approval::Default);
        assert!(!agent.reserved);
        assert_eq!(agent.role, "");
    }

    #[test]
    fn the_scaffolded_chief_of_staff_parses() {
        let root = TempFolder::new("agents-chief");
        crate::regents::folder::open_or_init(&root.0).unwrap();

        let agents = list(&root.0.join(".regents"));

        assert_eq!(agents.len(), 1, "{agents:?}");
        assert_eq!(agents[0].name, "chief");
        assert_eq!(agents[0].cli, AgentCli::Claude);
        assert!(agents[0].reserved);
        assert!(agents[0].role.starts_with("Você é o Chief of Staff"));
    }

    #[test]
    fn invalid_files_are_refused() {
        for text in [
            "name = \"a\"\ncli = \"claude\"\n",
            "+++\nname = \"a\"\ncli = \"claude\"\n",
            "+++\nname = \"a\"\ncli = \"gemini\"\n+++\n",
            "+++\nname = \"A b\"\ncli = \"claude\"\n+++\n",
        ] {
            assert!(parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn list_skips_invalid_and_duplicate_files_and_sorts_by_name() {
        let folder = TempFolder::new("agents-list");
        let state = folder.0.clone();
        let dir = state.join(AGENTS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let agent = |name: &str| format!("+++\nname = \"{name}\"\ncli = \"claude\"\n+++\n");
        std::fs::write(dir.join("1-zeta.md"), agent("zeta")).unwrap();
        std::fs::write(dir.join("2-alpha.md"), agent("alpha")).unwrap();
        std::fs::write(dir.join("3-copy.md"), agent("zeta")).unwrap();
        std::fs::write(dir.join("4-broken.md"), "no front matter").unwrap();
        std::fs::write(dir.join("notes.txt"), agent("text")).unwrap();

        let names: Vec<String> = list(&state).into_iter().map(|agent| agent.name).collect();

        assert_eq!(names, ["alpha", "zeta"]);
        assert!(list(&state.join("missing")).is_empty());
    }
}
