//! Just enough Markdown for reading AI answers: headings, lists, quotes, code
//! blocks, rules and tables. Inline marks are dropped, since a Slint text has
//! one style. The reader puts a conversation together from its stored turns
//! with [`question`], [`speaker`] and [`topic`], so text that only looks like a
//! turn stays text.
use std::rc::Rc;

use slint::{ModelRc, SharedString, VecModel};

use crate::MdBlock;

const PARAGRAPH: i32 = 0;
const TITLE: i32 = 1;
const HEADING: i32 = 2;
const ITEM: i32 = 3;
const QUOTE: i32 = 4;
const CODE: i32 = 5;
const RULE: i32 = 6;
const TABLE_HEAD: i32 = 7;
const TABLE_ROW: i32 = 8;
const QUESTION: i32 = 9;
const SPEAKER: i32 = 10;
const TOPIC: i32 = 11;

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
        cells: ModelRc::default(),
    }
}

/// Whether a line has a pipe that is not escaped as `\|`.
fn piped(line: &str) -> bool {
    let mut escaped = false;
    line.chars().any(|c| {
        let pipe = c == '|' && !escaped;
        escaped = c == '\\' && !escaped;
        pipe
    })
}

/// A table row's cells, without the outer pipes. `\|` is a pipe inside its
/// cell and `\\` a backslash, so a pipe after an even run of backslashes
/// still divides cells.
fn cells(line: &str) -> Vec<String> {
    let line = line.trim();
    let mut cells = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let cell = cells.last_mut().unwrap();
                match chars.next() {
                    Some(next @ ('|' | '\\')) => cell.push(next),
                    next => cell.extend(Some('\\').into_iter().chain(next)),
                }
            }
            '|' => cells.push(String::new()),
            _ => cells.last_mut().unwrap().push(c),
        }
    }
    // Outer pipes leave an empty cell at either end.
    if line.starts_with('|') {
        cells.remove(0);
    }
    if cells.len() > 1 && cells.last().is_some_and(String::is_empty) {
        cells.pop();
    }
    cells
        .into_iter()
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// Whether a row only divides the header from the body, like `|---|:--:|`.
fn divider(cells: &[String]) -> bool {
    cells.iter().all(|cell| {
        !cell.is_empty() && cell.contains('-') && cell.chars().all(|c| matches!(c, '-' | ':'))
    })
}

/// Whether a line and the one after it open a table: a header row, then a
/// divider row with as many cells.
fn table_start(line: &str, next: Option<&&str>) -> bool {
    let Some(next) = next else {
        return false;
    };
    let divide = cells(next);
    piped(line) && divider(&divide) && cells(line).len() == divide.len()
}

/// The header (first line) and body rows, cut or padded to the header's
/// width so their columns line up; the divider (second line) is dropped.
fn table(lines: &[&str], out: &mut Vec<MdBlock>) {
    let mut rows = lines.iter().map(|line| cells(line)).collect::<Vec<_>>();
    rows.remove(1);
    let width = rows[0].len();
    for (index, mut row) in rows.into_iter().enumerate() {
        row.resize(width, String::new());
        let row = row.iter().map(|cell| SharedString::from(inline(cell)));
        let mut cells = block(
            if index == 0 { TABLE_HEAD } else { TABLE_ROW },
            String::new(),
            "",
            0,
        );
        cells.cells = Rc::new(VecModel::from_iter(row)).into();
        out.push(cells);
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
    let mut rows: Vec<&str> = Vec::new();
    let flush = |paragraph: &mut Vec<&str>, out: &mut Vec<MdBlock>| {
        if !paragraph.is_empty() {
            out.push(block(PARAGRAPH, inline(&paragraph.join("\n")), "", 0));
            paragraph.clear();
        }
    };
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            match code.take() {
                Some(lines) => out.push(block(CODE, lines.join("\n"), "", 0)),
                None => {
                    flush(&mut paragraph, &mut out);
                    if !rows.is_empty() {
                        table(&rows, &mut out);
                        rows.clear();
                    }
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
        // A table runs from its header and divider to the first line without a pipe.
        if rows.is_empty() && table_start(trimmed, lines.peek()) {
            flush(&mut paragraph, &mut out);
            rows.push(trimmed);
            rows.extend(lines.next().map(str::trim));
            continue;
        }
        if !rows.is_empty() && piped(trimmed) {
            rows.push(trimmed);
            continue;
        }
        if !rows.is_empty() {
            table(&rows, &mut out);
            rows.clear();
        }
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
    if !rows.is_empty() {
        table(&rows, &mut out);
    }
    if let Some(lines) = code {
        // An answer still streaming may not have closed its code block yet.
        out.push(block(CODE, lines.join("\n"), "", 0));
    }
    flush(&mut paragraph, &mut out);
    out
}

/// A follow-up question, shown as written.
pub fn question(text: &str) -> MdBlock {
    block(QUESTION, text.trim().to_owned(), "", 0)
}

/// The start of a follow-up answer; `state` names one still coming.
pub fn speaker(state: &str) -> MdBlock {
    block(SPEAKER, state.to_owned(), "", 0)
}

/// Where a new topic starts.
pub fn topic(note: &str) -> MdBlock {
    block(TOPIC, note.to_owned(), "", 0)
}

pub fn model(blocks: Vec<MdBlock>) -> ModelRc<MdBlock> {
    Rc::new(VecModel::from(blocks)).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model as _;

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
    fn reads_tables_as_aligned_rows() {
        let text = "前言
| 应用 | 时长 |
|---|:--:|
| **zen** | 4h |
| claude |
结尾";
        let blocks = blocks(text);
        let rows = blocks
            .iter()
            .map(|b| {
                let cells = (0..b.cells.row_count())
                    .map(|i| b.cells.row_data(i).unwrap().to_string())
                    .collect::<Vec<_>>();
                (b.kind, cells)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                (PARAGRAPH, vec![]),
                (TABLE_HEAD, vec!["应用".into(), "时长".into()]),
                (TABLE_ROW, vec!["zen".into(), "4h".into()]),
                (TABLE_ROW, vec!["claude".into(), "".into()]),
                (PARAGRAPH, vec![]),
            ]
        );
    }

    #[test]
    fn tables_need_a_divider_and_keep_their_place() {
        let rows = |text: &str| {
            blocks(text)
                .iter()
                .map(|b| {
                    let cells = (0..b.cells.row_count())
                        .map(|i| b.cells.row_data(i).unwrap().to_string())
                        .collect::<Vec<_>>();
                    (b.kind, b.text.to_string(), cells)
                })
                .collect::<Vec<_>>()
        };
        // Outer pipes are optional; an escaped pipe stays in its cell.
        assert_eq!(
            rows("条件 | 结果\n--- | ---\na \\| b | 真"),
            vec![
                (TABLE_HEAD, "".into(), vec!["条件".into(), "结果".into()]),
                (TABLE_ROW, "".into(), vec!["a | b".into(), "真".into()]),
            ]
        );
        // `\\` is a backslash, so the pipe after it still divides; the header
        // sets the width, so extra body cells are dropped.
        assert_eq!(
            rows("| 左 | 右 |\n| --- | --- |\n| a \\\\| b | c |"),
            vec![
                (TABLE_HEAD, "".into(), vec!["左".into(), "右".into()]),
                (TABLE_ROW, "".into(), vec!["a \\".into(), "b".into()]),
            ]
        );
        // A pipe without a divider row below is ordinary text.
        assert_eq!(rows("|x| = 3"), vec![(PARAGRAPH, "|x| = 3".into(), vec![])]);
        // A code block right after a table comes after it.
        assert_eq!(
            rows("| A |\n| --- |\n```\ncode\n```"),
            vec![
                (TABLE_HEAD, "".into(), vec!["A".into()]),
                (CODE, "code".into(), vec![]),
            ]
        );
    }

    #[test]
    fn text_that_looks_like_a_turn_stays_text() {
        assert_eq!(
            kinds("你：\nAI：\n```\nAI（回答中…）：\n```"),
            vec![
                (PARAGRAPH, "你：\nAI：".into(), "".into()),
                (CODE, "AI（回答中…）：".into(), "".into()),
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
