//! CLI adapters (Regents): they turn an agent definition and the initial
//! prompt of a thread into the command line of the agent CLI. Pure: no I/O.
//! Only the claude adapter exists in this slice.

use super::agents::{AgentCli, AgentDefinition};
use super::channel::{AuthorKind, Message};

/// How many recent channel messages the initial prompt carries.
pub(crate) const RECENT_MESSAGES: u32 = 20;

/// The command line of a new thread of `agent`, or `None` when the CLI of the
/// agent has no adapter yet. `session_id` names the CLI session, so the thread
/// can resume it later (`claude --resume <id>`).
pub(crate) fn launch_argv(
    agent: &AgentDefinition,
    prompt: &str,
    session_id: &str,
) -> Option<Vec<String>> {
    match agent.cli {
        AgentCli::Claude => Some(claude_argv(agent, prompt, session_id)),
        AgentCli::Codex | AgentCli::Agy => None,
    }
}

/// claude takes the role through `--append-system-prompt`, which keeps its
/// own system prompt, and the first prompt as the positional argument of an
/// interactive session.
fn claude_argv(agent: &AgentDefinition, prompt: &str, session_id: &str) -> Vec<String> {
    let mut argv = vec![
        "claude".to_owned(),
        "--session-id".to_owned(),
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
         A wiki do workspace começa em .regents/wiki/index.md.\n"
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
        }
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
