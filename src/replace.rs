//! Words you say, and the text yapper writes instead.
//!
//! Dictation is bad at symbols. Saying "minus minus" and getting `--` is the
//! sort of thing a keyboard does without being asked, and no model will ever do
//! it, because what you want written is not what you said.
//!
//! Rules are matched word by word rather than as raw text, so a rule survives
//! the model's own punctuation and capitals: "minus minus" catches "Minus,
//! minus" too. What it will not do is span a sentence, because two separate
//! sentences ending and starting with the same word are not the phrase you
//! meant.

use serde::{Deserialize, Serialize};

/// One rule: say this, write that.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replacement {
    /// The words to listen for, as you would say them.
    pub say: String,
    /// What to write in their place, exactly as written here.
    pub write: String,
}

/// Rewrite a transcript with every rule that matches.
///
/// Longer phrases win, so a rule for "minus minus" is not spoiled by one for
/// "minus". What a rule writes is never matched again, so rules cannot chain
/// into each other.
pub fn apply(text: &str, rules: &[Replacement]) -> String {
    let prepared = prepare(rules);
    if prepared.is_empty() || text.is_empty() {
        return text.to_string();
    }

    let words = words(text);
    let mut out = String::with_capacity(text.len());
    // Where we have copied up to, and which word we are looking at.
    let mut copied = 0;
    let mut at = 0;

    while at < words.len() {
        match prepared
            .iter()
            .find_map(|rule| matches_at(text, &words, at, rule).then_some(rule))
        {
            Some(rule) => {
                let last = at + rule.words.len() - 1;
                let mut before = &text[copied..words[at].start];
                let after = &text[words[last].end..];
                // A rule that writes nothing is there to drop a word. Leaving
                // the space from each side of it would leave a gap where the
                // word was.
                if rule.write.is_empty() && before.ends_with(' ') && after.starts_with(' ') {
                    before = &before[..before.len() - 1];
                }
                out.push_str(before);
                out.push_str(&rule.write);
                copied = words[last].end;
                at = last + 1;
            }
            None => at += 1,
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// A rule split into the words it listens for, lowercased once rather than
/// once per transcript.
struct Prepared {
    words: Vec<String>,
    write: String,
}

fn prepare(rules: &[Replacement]) -> Vec<Prepared> {
    let mut prepared: Vec<Prepared> = rules
        .iter()
        .map(|rule| Prepared {
            words: words(&rule.say)
                .into_iter()
                .map(|word| word.lower)
                .collect(),
            write: rule.write.clone(),
        })
        // A rule with nothing to listen for would match everywhere.
        .filter(|rule| !rule.words.is_empty())
        .collect();
    // Longest first, so the more specific rule is the one that gets to match.
    prepared.sort_by(|a, b| b.words.len().cmp(&a.words.len()));
    prepared
}

/// A word in the text: where it sits, and how it compares.
struct Word {
    start: usize,
    end: usize,
    lower: String,
}

/// Split into words, where a word is letters and digits with any apostrophes
/// that fall inside it — everything else is what separates them.
fn words(text: &str) -> Vec<Word> {
    let mut words = Vec::new();
    let mut start = None;
    for (index, character) in text.char_indices() {
        let part_of_word =
            character.is_alphanumeric() || character == '\'' || character == '\u{2019}';
        match (part_of_word, start) {
            (true, None) => start = Some(index),
            (false, Some(from)) => {
                words.push(word(text, from, index));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        words.push(word(text, from, text.len()));
    }
    words
}

fn word(text: &str, start: usize, end: usize) -> Word {
    Word {
        start,
        end,
        lower: text[start..end].to_lowercase(),
    }
}

/// Whether a rule's words run from `at`, allowing for whatever the model put
/// between them — a comma, a hyphen, a stray space — but not the end of a
/// sentence, which a line break counts as.
fn matches_at(text: &str, words: &[Word], at: usize, rule: &Prepared) -> bool {
    if at + rule.words.len() > words.len() {
        return false;
    }
    for (offset, wanted) in rule.words.iter().enumerate() {
        let word = &words[at + offset];
        if word.lower != *wanted {
            return false;
        }
        if offset > 0 {
            let between = &text[words[at + offset - 1].end..word.start];
            if between.contains(['.', '!', '?', '\n']) {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(pairs: &[(&str, &str)]) -> Vec<Replacement> {
        pairs
            .iter()
            .map(|(say, write)| Replacement {
                say: say.to_string(),
                write: write.to_string(),
            })
            .collect()
    }

    #[test]
    fn a_phrase_becomes_the_text_you_asked_for() {
        let rules = rules(&[("minus minus", "--")]);
        assert_eq!(
            apply("run it with minus minus force", &rules),
            "run it with -- force"
        );
        assert_eq!(apply("minus minus", &rules), "--");
    }

    #[test]
    fn the_models_own_capitals_and_punctuation_do_not_get_in_the_way() {
        let rules = rules(&[("minus minus", "--")]);
        // Sentence case, and a comma the model heard in the pause.
        assert_eq!(apply("Minus minus force", &rules), "-- force");
        assert_eq!(apply("Try minus, minus force", &rules), "Try -- force");
        assert_eq!(apply("MINUS MINUS", &rules), "--");
        // Whatever fell between the words goes with them.
        assert_eq!(apply("minus - minus", &rules), "--");
    }

    #[test]
    fn a_rule_does_not_reach_across_a_sentence() {
        let rules = rules(&[("minus minus", "--")]);
        let text = "It was a minus. Minus signs everywhere.";
        assert_eq!(apply(text, &rules), text);
        // A line break is a break too.
        assert_eq!(apply("minus\nminus", &rules), "minus\nminus");
    }

    #[test]
    fn only_whole_words_count() {
        let rules = rules(&[("dash", "-")]);
        assert_eq!(
            apply("the dashboard is fine", &rules),
            "the dashboard is fine"
        );
        assert_eq!(apply("a dash of salt", &rules), "a - of salt");
    }

    #[test]
    fn the_longer_phrase_wins() {
        let rules = rules(&[("minus", "-"), ("minus minus", "--")]);
        assert_eq!(apply("minus minus", &rules), "--");
        assert_eq!(apply("one minus two", &rules), "one - two");
        // And the order they were written in makes no difference.
        let reversed = super::tests::rules(&[("minus minus", "--"), ("minus", "-")]);
        assert_eq!(apply("minus minus", &reversed), "--");
    }

    #[test]
    fn what_a_rule_writes_is_not_matched_again() {
        // Without this, "dash" would eat its own output forever.
        let rules = rules(&[("minus", "minus minus"), ("minus minus", "--")]);
        assert_eq!(apply("a minus here", &rules), "a minus minus here");
    }

    #[test]
    fn several_rules_and_several_matches() {
        let rules = rules(&[("new line", "\n"), ("open bracket", "(")]);
        assert_eq!(
            apply("first new line open bracket second", &rules),
            "first \n ( second"
        );
        assert_eq!(apply("new line new line", &rules), "\n \n");
    }

    #[test]
    fn nothing_to_do_leaves_the_transcript_alone() {
        let text = "nothing here matches";
        assert_eq!(apply(text, &[]), text);
        assert_eq!(apply(text, &rules(&[("minus minus", "--")])), text);
        // A half-written rule matches nothing rather than everything.
        assert_eq!(apply(text, &rules(&[("", "--")])), text);
        assert_eq!(apply("", &rules(&[("minus", "-")])), "");
    }

    #[test]
    fn a_rule_can_delete_what_you_said() {
        // An empty replacement is a way to drop a verbal tic, and the space it
        // was sitting in goes with it.
        let rules = rules(&[("you know", ""), ("um", "")]);
        assert_eq!(apply("it is um fine", &rules), "it is fine");
        assert_eq!(apply("you know it is fine", &rules), " it is fine");
        // Punctuation the model put around it is not ours to tidy away.
        assert_eq!(apply("it is, you know, fine", &rules), "it is, , fine");
    }

    #[test]
    fn words_with_apostrophes_and_other_alphabets_survive() {
        let rules = rules(&[("don't", "do not"), ("straße", "street")]);
        assert_eq!(apply("I don't mind", &rules), "I do not mind");
        assert_eq!(apply("Straße", &rules), "street");
    }
}
