//! Hiding applications open only briefly from the statistics, such as tools
//! started for a moment. Everything is still recorded and the timeline shows
//! it; the rule only filters the statistics list. It is kept beside the theme
//! choice, outside the dataset.
use std::{cell::Cell, fs, path::Path};

pub const FILE: &str = "short-apps.txt";

/// The longest threshold that can be set: an hour.
const MAX_SECONDS: u32 = 3600;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rule {
    pub on: bool,
    pub seconds: u32,
}

impl Default for Rule {
    fn default() -> Self {
        Self {
            on: true,
            seconds: 30,
        }
    }
}

impl Rule {
    /// Whether an application open this long in the range is left out.
    pub fn hides(self, opened_ms: u64) -> bool {
        self.on && opened_ms < u64::from(self.seconds) * 1000
    }
}

thread_local! {
    static RULE: Cell<Rule> = Cell::new(Rule::default());
}

/// Whole seconds from 1 to an hour, as typed.
pub fn seconds(text: &str) -> Option<u32> {
    text.trim()
        .parse()
        .ok()
        .filter(|seconds| (1..=MAX_SECONDS).contains(seconds))
}

fn parse(text: &str) -> Rule {
    let mut words = text.split_whitespace();
    let default = Rule::default();
    let on = match words.next() {
        Some("on") => true,
        Some("off") => false,
        _ => return default,
    };
    let seconds = words.next().and_then(seconds).unwrap_or(default.seconds);
    Rule { on, seconds }
}

pub fn load(control: &Path) {
    let rule =
        fs::read_to_string(control.join(FILE)).map_or_else(|_| Rule::default(), |t| parse(&t));
    RULE.set(rule);
}

pub fn current() -> Rule {
    RULE.get()
}

pub fn set(control: &Path, rule: Rule) {
    RULE.set(rule);
    let text = format!("{} {}", if rule.on { "on" } else { "off" }, rule.seconds);
    let _ = fs::create_dir_all(control).and_then(|()| fs::write(control.join(FILE), text));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applications_open_under_the_threshold_are_hidden_while_the_rule_is_on() {
        let rule = Rule::default();
        assert!(rule.hides(29_999));
        assert!(!rule.hides(30_000));
        assert!(!Rule { on: false, ..rule }.hides(1));
        assert_eq!(
            parse("off 45"),
            Rule {
                on: false,
                seconds: 45
            }
        );
        assert_eq!(parse("on 0"), rule);
        assert_eq!(parse(""), rule);
        assert_eq!(seconds(" 90 "), Some(90));
        assert_eq!(seconds("3601"), None);
        assert_eq!(seconds("半分钟"), None);
    }
}
