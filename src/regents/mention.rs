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

/// Mentioned names of `body` that are not in `known`, in the order of their
/// first mention, each one once. Same tokens as `resolve`, so text that is
/// not a mention there is not one here.
pub(crate) fn unknown<'a>(body: &str, known: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let known: Vec<&str> = known.into_iter().collect();
    let mut unknown: Vec<String> = Vec::new();
    for name in mentioned_names(body) {
        if !known.contains(&name.as_str()) && !unknown.contains(&name) {
            unknown.push(name);
        }
    }
    unknown
}

/// Every `@name` token of `body`, lowercased.
fn mentioned_names(body: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut previous: Option<char> = None;
    for (index, ch) in body.char_indices() {
        let starts = ch == '@' && starts_mention(previous);
        previous = Some(ch);
        if !starts {
            continue;
        }
        let rest = &body[index + 1..];
        let length = rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len());
        let name = rest[..length].trim_end_matches(['-', '_']);
        if !name.is_empty() {
            names.push(name.to_ascii_lowercase());
        }
    }
    names
}

/// The mention that `text` ends in while the user types it: the byte index
/// of its `@` and the name typed so far, which can be empty. Same rule as
/// `resolve`.
pub(crate) fn trailing_mention(text: &str) -> Option<(usize, &str)> {
    let at = text.rfind('@')?;
    let name = &text[at + 1..];
    (name.chars().all(is_name_char) && starts_mention(text[..at].chars().next_back()))
        .then_some((at, name))
}

/// An `@` starts a mention only at the start of the text or after a
/// character that is not a letter, digit, `.` or `_`, so
/// `rafael@example.com` is not a mention.
fn starts_mention(previous: Option<char>) -> bool {
    previous.is_none_or(|before| !(before.is_alphanumeric() || matches!(before, '.' | '_')))
}

pub(crate) fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_')
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
            assert!(unknown(body, AGENTS).is_empty(), "{body:?}");
        }
    }

    #[test]
    fn unknown_names_are_the_mentions_that_resolve_ignores() {
        assert_eq!(unknown("@ninguem oi @chief", AGENTS), ["ninguem"]);
        assert_eq!(
            unknown("@Ninguem, @outro e @ninguem de novo", AGENTS),
            ["ninguem", "outro"]
        );
        assert!(unknown("@chief e @code-bot", AGENTS).is_empty());
        assert_eq!(unknown("fale com @nada-", AGENTS), ["nada"]);
    }

    #[test]
    fn a_mention_ignores_case_and_trailing_punctuation() {
        assert_eq!(resolve("@Chief: oi", AGENTS), ["chief"]);
        assert_eq!(resolve("fale com @chief-", AGENTS), ["chief"]);
        assert_eq!(resolve("oi\n@chief\n", AGENTS), ["chief"]);
    }

    #[test]
    fn a_trailing_mention_follows_the_same_rule_as_resolve() {
        assert_eq!(trailing_mention("@"), Some((0, "")));
        assert_eq!(trailing_mention("oi @Ch"), Some((3, "Ch")));
        assert_eq!(trailing_mention("(@code-"), Some((1, "code-")));
        assert_eq!(trailing_mention("@a e\n@re"), Some((5, "re")));
        for text in ["", "oi", "@ch ", "rafael@ch", "x.@ch", "a_@ch", "@ch,"] {
            assert_eq!(trailing_mention(text), None, "{text:?}");
        }
    }
}
