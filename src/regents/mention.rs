//! Mentions (`@agent`) in channel messages. Pure: no I/O.

/// Names of the agents of `known` that `body` mentions, in the order of
/// their first mention, each one once. A mention of a name that is not in
/// `known` is ignored.
pub(crate) fn resolve<'a>(body: &str, known: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let known: Vec<&str> = known.into_iter().collect();
    let mut resolved: Vec<String> = Vec::new();
    for name in mentioned_names(body) {
        if known.contains(&name.as_str()) && !resolved.contains(&name) {
            resolved.push(name);
        }
    }
    resolved
}

/// Every `@name` token of `body`, lowercased. An `@` counts only at the start
/// of the text or after a character that is not a letter, digit, `.` or `_`,
/// so `rafael@example.com` is not a mention.
fn mentioned_names(body: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut previous: Option<char> = None;
    for (index, ch) in body.char_indices() {
        let starts_mention = ch == '@'
            && previous
                .is_none_or(|before| !(before.is_alphanumeric() || matches!(before, '.' | '_')));
        previous = Some(ch);
        if !starts_mention {
            continue;
        }
        let rest = &body[index + 1..];
        let length = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
            .unwrap_or(rest.len());
        let name = rest[..length].trim_end_matches(['-', '_']);
        if !name.is_empty() {
            names.push(name.to_ascii_lowercase());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENTS: [&str; 3] = ["chief", "reviewer", "code-bot"];

    #[test]
    fn a_mention_of_a_known_agent_resolves_to_its_name() {
        assert_eq!(resolve("@chief olá", AGENTS), ["chief"]);
        assert_eq!(resolve("olá @chief, tudo bem?", AGENTS), ["chief"]);
        assert_eq!(resolve("(@code-bot)", AGENTS), ["code-bot"]);
    }

    #[test]
    fn several_mentions_keep_their_order_and_appear_once() {
        assert_eq!(
            resolve("@reviewer veja; @chief decide. @reviewer de novo", AGENTS),
            ["reviewer", "chief"]
        );
    }

    #[test]
    fn a_mention_of_an_unknown_agent_is_ignored() {
        assert_eq!(resolve("@ninguem oi @chief", AGENTS), ["chief"]);
        assert!(resolve("@ninguem oi", AGENTS).is_empty());
    }

    #[test]
    fn text_that_only_contains_an_at_sign_is_not_a_mention() {
        for body in ["rafael@chief.com", "a_@chief", "@ sozinho", "@", "x.@chief"] {
            assert!(resolve(body, AGENTS).is_empty(), "{body:?}");
        }
    }

    #[test]
    fn a_mention_ignores_case_and_trailing_punctuation() {
        assert_eq!(resolve("@Chief: oi", AGENTS), ["chief"]);
        assert_eq!(resolve("fale com @chief-", AGENTS), ["chief"]);
        assert_eq!(resolve("oi\n@chief\n", AGENTS), ["chief"]);
    }
}
