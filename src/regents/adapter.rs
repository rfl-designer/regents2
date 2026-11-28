//! CLI adapters (Regents): they turn an agent definition and the prompt of a
//! thread turn into the command line of the agent CLI (a new session or a
//! resumed one), and read the last reply of a thread turn from the CLI
//! session. Only `last_reply` and `has_session` read files. Only the claude
//! adapter exists in this slice.

use std::path::Path;

use super::agents::{AgentCli, AgentDefinition};
use super::channel::{AuthorKind, Message};

/// How many recent channel messages the initial prompt carries.
pub(crate) const RECENT_MESSAGES: u32 = 20;
/// The last line of each initial prompt: the reply on a screen starts after
/// its echo.
pub(crate) const PROMPT_LAST_LINE: &str = "A wiki do workspace começa em .regents/wiki/index.md.";

/// The reply of the last turn of a CLI session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnReply {
    /// Index of the turn in the session, from 0.
    pub(crate) turn: u32,
    /// The last text of the agent in the turn; `None` when the turn has no
    /// text, for example only tool calls.
    pub(crate) text: Option<String>,
}

/// The reply of the last turn of the CLI session `session_id`, from its
/// transcript under `cli_home` (for claude, `~/.claude`). `None` when the
/// session has no transcript or no turn.
pub(crate) fn last_reply(cli: AgentCli, session_id: &str, cli_home: &Path) -> Option<TurnReply> {
    match cli {
        AgentCli::Claude => claude_last_reply(
            &std::fs::read_to_string(claude_transcript(cli_home, session_id)?).ok()?,
        ),
        AgentCli::Codex | AgentCli::Agy => None,
    }
}

/// True when the CLI session `session_id` has a transcript under
/// `cli_home`, so the CLI can resume it.
pub(crate) fn has_session(cli: AgentCli, session_id: &str, cli_home: &Path) -> bool {
    match cli {
        AgentCli::Claude => claude_transcript(cli_home, session_id).is_some(),
        AgentCli::Codex | AgentCli::Agy => false,
    }
}

/// claude keeps a session in `projects/<folder>/<session>.jsonl`; the
/// session ID is unique, so the folder name needs no rule.
fn claude_transcript(cli_home: &Path, session_id: &str) -> Option<std::path::PathBuf> {
    let file = format!("{session_id}.jsonl");
    std::fs::read_dir(cli_home.join("projects"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(&file))
        .find(|path| path.is_file())
}

/// The last turn of a claude transcript (JSONL, checked with claude
/// 2.1.332). A turn starts at a `user` record with a prompt, not only tool
/// results. Its reply is the last `text` block of the `assistant` records
/// after it: thinking, tool calls, sidechain (subagent) and metadata records
/// are not part of it.
fn claude_last_reply(transcript: &str) -> Option<TurnReply> {
    let mut prompts = 0u32;
    let mut text = None;
    for line in transcript.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if record["isSidechain"] == true {
            continue;
        }
        let content = &record["message"]["content"];
        match record["type"].as_str() {
            Some("user") if record["isMeta"] != true && is_prompt(content) => {
                prompts += 1;
                text = None;
            }
            Some("assistant") => {
                let blocks = content.as_array().into_iter().flatten();
                for block in blocks.filter(|block| block["type"] == "text") {
                    if let Some(value) = block["text"].as_str() {
                        text = Some(value.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    Some(TurnReply {
        turn: prompts.checked_sub(1)?,
        text: text.filter(|text| !text.trim().is_empty()),
    })
}

fn is_prompt(content: &serde_json::Value) -> bool {
    content.is_string()
        || content
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|block| block["type"] != "tool_result"))
}

/// The reply on the screen of a thread whose session has no transcript:
/// the text after the echo of the initial prompt. `None` without the echo,
/// because then the screen shows no turn, only the CLI.
pub(crate) fn screen_reply(screen: &str) -> Option<String> {
    let start = screen.rfind(PROMPT_LAST_LINE)? + PROMPT_LAST_LINE.len();
    let reply = screen[start..].trim();
    (!reply.is_empty()).then(|| reply.to_owned())
}

/// True when threads of `cli` can start: `launch_argv` and `resume_argv`
/// give a command line.
pub(crate) fn has_thread_adapter(cli: AgentCli) -> bool {
    matches!(cli, AgentCli::Claude)
}

/// The command line of a new thread of `agent`, or `None` when the CLI of the
/// agent has no adapter yet. `session_id` names the CLI session, so the thread
/// can resume it later (`claude --resume <id>`).
pub(crate) fn launch_argv(
    agent: &AgentDefinition,
    prompt: &str,
    session_id: &str,
) -> Option<Vec<String>> {
    match agent.cli {
        AgentCli::Claude => Some(claude_argv(agent, prompt, "--session-id", session_id)),
        AgentCli::Codex | AgentCli::Agy => None,
    }
}

/// The command line that resumes the CLI session `session_id` of a done
/// thread of `agent` with `prompt`, or `None` when the CLI of the agent has
/// no adapter yet.
pub(crate) fn resume_argv(
    agent: &AgentDefinition,
    prompt: &str,
    session_id: &str,
) -> Option<Vec<String>> {
    match agent.cli {
        AgentCli::Claude => Some(claude_argv(agent, prompt, "--resume", session_id)),
        AgentCli::Codex | AgentCli::Agy => None,
    }
}

/// claude takes the role through `--append-system-prompt`, which keeps its
/// own system prompt, and the prompt as the positional argument of an
/// interactive session. The role and the model hold only for one run
/// (claude 2.1.332), so a resume passes them again.
fn claude_argv(
    agent: &AgentDefinition,
    prompt: &str,
    session_flag: &str,
    session_id: &str,
) -> Vec<String> {
    let mut argv = vec![
        "claude".to_owned(),
        session_flag.to_owned(),
        session_id.to_owned(),
    ];
    if !agent.role.is_empty() {
        argv.push("--append-system-prompt".to_owned());
        argv.push(agent.role.clone());
    }
    if let Some(model) = &agent.model {
        argv.push("--model".to_owned());
        argv.push(model.clone());
    }
    argv.push(prompt.to_owned());
    argv
}

/// A new random session ID: a version 4 UUID, the form that
/// `claude --session-id` takes.
pub(crate) fn new_session_id() -> String {
    let mut bytes = super::random_bytes(b"session");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The first prompt of a thread opened by `mention`: the mention, the most
/// recent messages of the channel (oldest first) and how to read more and
/// answer. The wiki is never copied into the prompt, only pointed at.
pub(crate) fn initial_prompt(mention: &Message, recent: &[Message]) -> String {
    let channel = &mention.channel;
    let mut prompt = format!(
        "Você foi mencionado no canal #{channel} do Regents.\n\n\
         Mensagem que mencionou você ({}):\n{}\n\n\
         Últimas mensagens do canal #{channel}, da mais antiga para a mais nova:\n",
        author_label(mention),
        mention.body,
    );
    for message in recent {
        prompt.push_str(&format!(
            "[{}] {}: {}\n",
            message.id,
            author_label(message),
            message.body
        ));
    }
    prompt.push_str(&format!(
        "\nPara ler mensagens mais antigas: regents channel read {channel} --before <id> --limit <n>\n\
         Para responder no canal: regents channel post {channel} \"<resposta>\"\n\
         {PROMPT_LAST_LINE}\n"
    ));
    prompt
}

/// The prompt of a resumed thread: the threaded reply and the channel
/// messages posted since the previous turn started (`new`, oldest first).
/// The session already has the older context.
pub(crate) fn resume_prompt(reply: &Message, new: &[Message]) -> String {
    let channel = &reply.channel;
    let mut prompt = format!(
        "Responderam a uma mensagem sua no canal #{channel} do Regents.\n\n\
         Resposta ({}):\n{}\n\n\
         Mensagens novas do canal #{channel} desde o seu último turno, da mais antiga para a mais nova:\n",
        author_label(reply),
        reply.body,
    );
    for message in new {
        prompt.push_str(&format!(
            "[{}] {}: {}\n",
            message.id,
            author_label(message),
            message.body
        ));
    }
    prompt.push_str(&format!(
        "\nPara responder no canal: regents channel post {channel} \"<resposta>\"\n\
         {PROMPT_LAST_LINE}\n"
    ));
    prompt
}

fn author_label(message: &Message) -> String {
    let kind = match message.author.kind {
        AuthorKind::Human => "humano",
        AuthorKind::Agent => "agente",
        AuthorKind::System => "sistema",
        AuthorKind::Unknown => "desconhecido",
    };
    format!("{}, {kind}", message.author.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regents::agents::{Approval, Isolation};
    use crate::regents::channel::Author;

    fn agent(cli: AgentCli, model: Option<&str>) -> AgentDefinition {
        AgentDefinition {
            name: "chief".into(),
            cli,
            model: model.map(str::to_owned),
            isolation: Isolation::Shared,
            approval: Approval::Default,
            reserved: true,
            role: "Você é o Chief of Staff.".into(),
        }
    }

    fn message(id: u64, body: &str) -> Message {
        Message {
            id,
            channel: "geral".into(),
            author: Author {
                kind: AuthorKind::Human,
                id: "rafael".into(),
            },
            body: body.into(),
            reply_to: None,
            origin_thread_id: None,
            mentions: Vec::new(),
            created_at: "2026-11-27T18:04:05Z".into(),
            captured: false,
        }
    }

    /// A sanitized transcript of claude 2.1.332.
    fn transcript(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/regents/claude-2.1.332/{name}.jsonl",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn the_reply_is_the_last_text_of_the_turn_after_its_tool_calls() {
        assert_eq!(
            claude_last_reply(&transcript("one-turn")),
            Some(TurnReply {
                turn: 0,
                text: Some("pronto, linha 1\nlinha 2".into()),
            })
        );
    }

    #[test]
    fn the_second_turn_has_its_own_index_and_reply() {
        assert_eq!(
            claude_last_reply(&transcript("two-turns")),
            Some(TurnReply {
                turn: 1,
                text: Some("segundo turno @reviewer".into()),
            })
        );
    }

    #[test]
    fn a_turn_with_only_tool_calls_has_no_reply() {
        assert_eq!(
            claude_last_reply(&transcript("tool-calls-only")),
            Some(TurnReply {
                turn: 0,
                text: None
            })
        );
    }

    #[test]
    fn a_sidechain_record_is_not_the_reply() {
        assert_eq!(
            claude_last_reply(&transcript("one-turn-then-sidechain"))
                .and_then(|reply| reply.text)
                .as_deref(),
            Some("pronto, linha 1\nlinha 2")
        );
    }

    #[test]
    fn a_transcript_without_a_prompt_has_no_turn() {
        assert_eq!(claude_last_reply(""), None);
        assert_eq!(claude_last_reply("not json\n{\"type\":\"mode\"}"), None);
    }

    #[test]
    fn last_reply_finds_the_session_in_any_project_folder() {
        let home = crate::regents::store::TempFolder::new("adapter-transcript");
        let project = home.0.join("projects").join("-tmp-projeto");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("sid.jsonl"), transcript("one-turn")).unwrap();

        let reply = last_reply(AgentCli::Claude, "sid", &home.0);

        assert_eq!(reply.map(|reply| reply.turn), Some(0));
        assert_eq!(last_reply(AgentCli::Claude, "other", &home.0), None);
        assert_eq!(last_reply(AgentCli::Codex, "sid", &home.0), None);
    }

    #[test]
    fn the_screen_reply_starts_after_the_prompt_echo() {
        let screen = format!("> prompt\n{PROMPT_LAST_LINE}\n\n  resposta\n");

        assert_eq!(screen_reply(&screen).as_deref(), Some("resposta"));
        assert_eq!(screen_reply("só a tela do CLI"), None);
        assert_eq!(screen_reply(&format!("{PROMPT_LAST_LINE}\n  ")), None);
    }

    #[test]
    fn claude_gets_the_role_the_model_the_session_and_the_prompt() {
        let argv = launch_argv(&agent(AgentCli::Claude, Some("opus")), "o prompt", "sid").unwrap();

        assert_eq!(
            argv,
            [
                "claude",
                "--session-id",
                "sid",
                "--append-system-prompt",
                "Você é o Chief of Staff.",
                "--model",
                "opus",
                "o prompt",
            ]
        );
    }

    #[test]
    fn a_resume_passes_the_session_the_role_and_the_model_again() {
        let argv = resume_argv(&agent(AgentCli::Claude, Some("haiku")), "p", "sid").unwrap();

        assert_eq!(
            argv,
            [
                "claude",
                "--resume",
                "sid",
                "--append-system-prompt",
                "Você é o Chief of Staff.",
                "--model",
                "haiku",
                "p",
            ]
        );
        assert_eq!(resume_argv(&agent(AgentCli::Codex, None), "p", "sid"), None);
    }

    #[test]
    fn a_session_exists_only_with_its_transcript() {
        let home = crate::regents::store::TempFolder::new("adapter-has-session");
        let project = home.0.join("projects").join("-tmp-projeto");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("sid.jsonl"), "").unwrap();

        assert!(has_session(AgentCli::Claude, "sid", &home.0));
        assert!(!has_session(AgentCli::Claude, "other", &home.0));
        assert!(!has_session(AgentCli::Codex, "sid", &home.0));
    }

    #[test]
    fn the_resume_prompt_carries_the_reply_and_only_the_new_messages() {
        let reply = message(7, "e o resto?");
        let new = [message(6, "nova"), reply.clone()];

        let prompt = resume_prompt(&reply, &new);

        assert!(
            prompt.contains("(rafael, humano):\ne o resto?\n"),
            "{prompt}"
        );
        let first = prompt.find("[6] rafael, humano: nova").unwrap();
        let second = prompt.find("[7] rafael, humano: e o resto?").unwrap();
        assert!(first < second, "{prompt}");
        assert!(prompt.contains("regents channel post geral"), "{prompt}");
        assert!(
            prompt.trim_end().ends_with(PROMPT_LAST_LINE),
            "the screen reply starts after its echo: {prompt}"
        );
    }

    #[test]
    fn claude_without_a_model_uses_its_default() {
        let argv = launch_argv(&agent(AgentCli::Claude, None), "p", "sid").unwrap();

        assert!(!argv.contains(&"--model".to_owned()), "{argv:?}");
        assert_eq!(argv.last().map(String::as_str), Some("p"));
    }

    #[test]
    fn codex_and_agy_have_no_adapter_yet() {
        assert_eq!(launch_argv(&agent(AgentCli::Codex, None), "p", "sid"), None);
        assert_eq!(launch_argv(&agent(AgentCli::Agy, None), "p", "sid"), None);
    }

    #[test]
    fn a_session_id_is_a_version_4_uuid() {
        let first = new_session_id();
        let parts: Vec<&str> = first.split('-').collect();

        assert_eq!(
            parts.iter().map(|part| part.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "{first}");
        assert!(matches!(&parts[3][..1], "8" | "9" | "a" | "b"), "{first}");
        assert_ne!(first, new_session_id());
    }

    #[test]
    fn the_prompt_carries_the_mention_and_the_recent_messages_in_order() {
        let mention = message(3, "@chief olá");
        let recent = [message(1, "contexto"), message(2, "mais"), mention.clone()];

        let prompt = initial_prompt(&mention, &recent);

        assert!(prompt.contains("#geral"), "{prompt}");
        assert!(
            prompt.contains("(rafael, humano):\n@chief olá\n"),
            "{prompt}"
        );
        let first = prompt.find("[1] rafael, humano: contexto").unwrap();
        let second = prompt.find("[2] rafael, humano: mais").unwrap();
        let third = prompt.find("[3] rafael, humano: @chief olá").unwrap();
        assert!(first < second && second < third, "{prompt}");
        assert!(
            prompt.contains("regents channel read geral --before"),
            "{prompt}"
        );
        assert!(prompt.contains("regents channel post geral"), "{prompt}");
        assert!(prompt.contains(".regents/wiki/index.md"), "{prompt}");
    }
}
