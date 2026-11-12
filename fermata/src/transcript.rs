//! Claude Code `.jsonl` transcripts: streamed, never loaded whole.
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TEXT: usize = 20_000;
const MAX_ENTRIES: usize = 20_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    User,
    Assistant,
    Tool,
    ToolResult,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub kind: Kind,
    pub ts: Option<String>,
    /// One-line summary (tool calls) or the full text (turns).
    pub head: String,
    pub body: String,
}

impl Entry {
    pub fn collapsible(&self) -> bool {
        matches!(self.kind, Kind::Tool | Kind::ToolResult)
    }
}

pub struct Transcript {
    pub entries: Vec<Entry>,
    pub skipped_bytes: u64,
}

fn cap(s: &str) -> String {
    if s.len() <= MAX_TEXT {
        return s.to_string();
    }
    let mut end = MAX_TEXT;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… [truncated {} bytes]", &s[..end], s.len() - end)
}

fn result_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn tool_summary(name: &str, input: &Value) -> String {
    let key = [
        "description",
        "command",
        "file_path",
        "pattern",
        "prompt",
        "url",
        "query",
    ]
    .iter()
    .find_map(|k| input.get(*k).and_then(Value::as_str));
    let s = key.unwrap_or("");
    format!(
        "{name}: {}",
        crate::util::truncate(&crate::util::one_line(s), 160)
    )
}

/// Entries for one transcript line.
pub fn parse_line(line: &str) -> Vec<Entry> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return vec![];
    };
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    if ty != "user" && ty != "assistant" {
        return vec![];
    }
    let ts = v
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string);
    let content = &v["message"]["content"];
    let turn = if ty == "user" {
        Kind::User
    } else {
        Kind::Assistant
    };
    let mut out = vec![];
    let mk = |kind: Kind, head: String, body: String| Entry {
        kind,
        ts: ts.clone(),
        head,
        body,
    };
    match content {
        Value::String(s) => out.push(mk(turn, String::new(), cap(s))),
        Value::Array(blocks) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" => {
                        let t = b.get("text").and_then(Value::as_str).unwrap_or("");
                        if !t.trim().is_empty() {
                            out.push(mk(turn.clone(), String::new(), cap(t)));
                        }
                    }
                    "tool_use" => {
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let input = b.get("input").cloned().unwrap_or(Value::Null);
                        let body = serde_json::to_string_pretty(&input).unwrap_or_default();
                        out.push(mk(Kind::Tool, tool_summary(name, &input), cap(&body)));
                    }
                    "tool_result" => {
                        let t = result_text(b.get("content").unwrap_or(&Value::Null));
                        let first = t.lines().next().unwrap_or("").to_string();
                        let head = format!(
                            "result: {} ({} lines)",
                            crate::util::truncate(&first, 120),
                            t.lines().count()
                        );
                        out.push(mk(Kind::ToolResult, head, cap(&t)));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

/// Open from the end of very large files so memory stays bounded.
fn open_tail(path: &Path, max: u64) -> std::io::Result<(BufReader<File>, u64)> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut skipped = 0;
    if len > max {
        skipped = len - max;
        f.seek(SeekFrom::Start(skipped))?;
    }
    let mut r = BufReader::new(f);
    if skipped > 0 {
        let mut junk = Vec::new();
        skipped += r.read_until(b'\n', &mut junk)? as u64;
    }
    Ok((r, skipped))
}

pub fn load(path: &Path) -> std::io::Result<Transcript> {
    let (mut r, skipped_bytes) = open_tail(path, MAX_BYTES)?;
    let mut entries = vec![];
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        entries.extend(parse_line(&line));
        if entries.len() > MAX_ENTRIES {
            entries.drain(..entries.len() - MAX_ENTRIES);
        }
    }
    Ok(Transcript {
        entries,
        skipped_bytes,
    })
}

/// A worker's report: its final SubagentHandback message, else its last assistant text.
pub fn worker_report(path: &Path) -> Option<String> {
    let (mut r, _) = open_tail(path, 1024 * 1024).ok()?;
    let mut buf = String::new();
    r.read_to_string(&mut buf).ok()?;
    let mut last_text = None;
    for line in buf.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        for b in v["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .rev()
        {
            match b.get("type").and_then(Value::as_str) {
                Some("tool_use") if b["name"] == "SubagentHandback" => {
                    return b["input"]["message"].as_str().map(str::to_string);
                }
                Some("text") if last_text.is_none() => {
                    last_text = b["text"].as_str().map(str::to_string);
                }
                _ => {}
            }
        }
        if last_text.is_some() {
            return last_text;
        }
    }
    last_text
}

/// Indices of entries whose head or body contains `q` (case-insensitive).
pub fn search(entries: &[Entry], q: &str) -> Vec<usize> {
    let q = q.to_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.head.to_lowercase().contains(&q) || e.body.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = r#"{"type":"user","timestamp":"t1","message":{"role":"user","content":"do the thing"}}
{"type":"assistant","message":{"content":[{"type":"text","text":"ok"},{"type":"tool_use","name":"Bash","input":{"command":"ls -la","description":"List files"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","content":"a\nb"}]}}
{"type":"system","message":{}}
not json
{"type":"assistant","message":{"content":[{"type":"text","text":"done; report"}]}}
"#;

    fn tmp(name: &str, body: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("fermata-{name}-{}.jsonl", std::process::id()));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn parses_and_collapses() {
        let p = tmp("tr", T);
        let t = load(&p).unwrap();
        let kinds: Vec<_> = t.entries.iter().map(|e| e.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                Kind::User,
                Kind::Assistant,
                Kind::Tool,
                Kind::ToolResult,
                Kind::Assistant
            ]
        );
        assert_eq!(t.entries[2].head, "Bash: List files");
        assert_eq!(t.entries[3].head, "result: a (2 lines)");
        assert_eq!(search(&t.entries, "THING"), vec![0]);
        assert_eq!(worker_report(&p).as_deref(), Some("done; report"));
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn handback_wins() {
        let body = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"SubagentHandback","input":{"message":"REPORT"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}
"#;
        let p = tmp("hb", body);
        assert_eq!(worker_report(&p).as_deref(), Some("REPORT"));
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn tail_skips_partial_line() {
        let p = tmp("tail", T);
        let (mut r, skipped) = open_tail(&p, 40).unwrap();
        let mut s = String::new();
        r.read_to_string(&mut s).unwrap();
        assert!(skipped > 0);
        assert!(s.is_empty() || s.starts_with('{') || s.starts_with('n'));
        std::fs::remove_file(p).unwrap();
    }
}
