//! Just enough Markdown for reading AI answers: headings, lists, quotes, code
//! blocks and rules. Inline marks are dropped, since a Slint text has one style.
use std::rc::Rc;

use slint::{ModelRc, VecModel};

use crate::MdBlock;

const PARAGRAPH: i32 = 0;
const TITLE: i32 = 1;
const HEADING: i32 = 2;
const ITEM: i32 = 3;
const QUOTE: i32 = 4;
const CODE: i32 = 5;
const RULE: i32 = 6;

/// The text without emphasis, inline code marks and link targets.
fn inline(text: &str) -> String {
    let mut out = text.replace("**", "").replace("__", "").replace('`', "");
    // [label](target) reads as the label.
    while let Some(open) = out.find('[') {
        let Some(close) = out[open..].find("](").map(|i| open + i) else {
            break;
        };
        let Some(end) = out[close..].find(')').map(|i| close + i) else {
            break;
        };
        out.replace_range(close..=end, "");
        out.remove(open);
    }
    out
}

fn block(kind: i32, text: String, marker: &str, indent: usize) -> MdBlock {
    MdBlock {
        kind,
        text: text.into(),
        marker: marker.into(),
        indent: indent.min(4) as i32,
    }
}

/// A list item's marker and text, when the line is one.
fn list_item(line: &str) -> Option<(String, &str)> {
    if let Some(rest) = ["- ", "* ", "+ "].iter().find_map(|m| line.strip_prefix(m)) {
        return Some(("•".into(), rest));
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    let rest = line[digits..]
        .strip_prefix(". ")
        .or_else(|| line[digits..].strip_prefix(") "))?;
    (digits > 0 && digits < 4).then(|| (format!("{}.", &line[..digits]), rest))
}

pub fn blocks(text: &str) -> Vec<MdBlock> {
    let mut out = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut code: Option<Vec<&str>> = None;
    let flush = |paragraph: &mut Vec<&str>, out: &mut Vec<MdBlock>| {
        if !paragraph.is_empty() {
            out.push(block(PARAGRAPH, inline(&paragraph.join("\n")), "", 0));
            paragraph.clear();
        }
    };
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            match code.take() {
                Some(lines) => out.push(block(CODE, lines.join("\n"), "", 0)),
                None => {
                    flush(&mut paragraph, &mut out);
                    code = Some(Vec::new());
                }
            }
            continue;
        }
        if let Some(lines) = &mut code {
            lines.push(line);
            continue;
        }
        let indent = (line.len() - trimmed.len()) / 2;
        let trimmed = trimmed.trim_end();
        if trimmed.is_empty() {
            flush(&mut paragraph, &mut out);
            continue;
        }
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            flush(&mut paragraph, &mut out);
            let kind = if hashes == 1 { TITLE } else { HEADING };
            out.push(block(kind, inline(trimmed[hashes..].trim()), "", 0));
        } else if trimmed.len() >= 3
            && ["-", "*", "_"]
                .iter()
                .any(|m| trimmed.chars().all(|c| c.to_string() == *m || c == ' '))
        {
            flush(&mut paragraph, &mut out);
            out.push(block(RULE, String::new(), "", 0));
        } else if let Some((marker, rest)) = list_item(trimmed) {
            flush(&mut paragraph, &mut out);
            out.push(block(ITEM, inline(rest), &marker, indent));
        } else if let Some(rest) = trimmed.strip_prefix('>') {
            flush(&mut paragraph, &mut out);
            out.push(block(QUOTE, inline(rest.trim_start()), "", 0));
        } else {
            paragraph.push(trimmed);
        }
    }
    if let Some(lines) = code {
        // An answer still streaming may not have closed its code block yet.
        out.push(block(CODE, lines.join("\n"), "", 0));
    }
    flush(&mut paragraph, &mut out);
    out
}

pub fn model(text: &str) -> ModelRc<MdBlock> {
    Rc::new(VecModel::from(blocks(text))).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<(i32, String, String)> {
        blocks(text)
            .into_iter()
            .map(|b| (b.kind, b.text.to_string(), b.marker.to_string()))
            .collect()
    }

    #[test]
    fn reads_common_answer_structure() {
        let text = "# 今天\n\n上午主要在 **写代码**，\n下午开会。\n\n## 要点\n- 第一\n2. 第二\n> 提醒\n---\n```\nlet x = 1;\n```";
        assert_eq!(
            kinds(text),
            vec![
                (TITLE, "今天".into(), "".into()),
                (
                    PARAGRAPH,
                    "上午主要在 写代码，\n下午开会。".into(),
                    "".into()
                ),
                (HEADING, "要点".into(), "".into()),
                (ITEM, "第一".into(), "•".into()),
                (ITEM, "第二".into(), "2.".into()),
                (QUOTE, "提醒".into(), "".into()),
                (RULE, "".into(), "".into()),
                (CODE, "let x = 1;".into(), "".into()),
            ]
        );
    }

    #[test]
    fn drops_inline_marks_and_link_targets() {
        assert_eq!(
            inline("看 [文档](https://example.com) 的 `code`"),
            "看 文档 的 code"
        );
        assert_eq!(kinds("#标签不是标题")[0].0, PARAGRAPH);
        assert_eq!(kinds("```\n未结束")[0], (CODE, "未结束".into(), "".into()));
    }
}
