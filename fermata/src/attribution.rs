//! Which tasks a piece of text is about: `[fermata <id>]` prompt segments, `### <task-id>` reply
//! sections, and the tracker writes in a shell command. Pure text; see "Conversation" in DESIGN.md.

/// The tracker verbs that write to the task named by their first argument (baton's tracker.md).
const WRITE_VERBS: &[&str] = &[
    "update",
    "claim",
    "close",
    "reopen",
    "label-add",
    "label-remove",
    "link",
    "note",
    "state",
    "decide",
    "reply",
    "resolve",
    "record-branch",
    "update-branch",
];

/// One message in a prompt: what `[fermata <id> …]` addressed it to, or nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub task: Option<String>,
    /// What follows the id in the marker: `<decision> <kind>`.
    pub tag: String,
    pub text: String,
}

/// The fence a line opens or closes: its character and length.
fn fence(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start();
    let c = t.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let n = t.chars().take_while(|x| *x == c).count();
    (n >= 3).then_some((c, n))
}

/// Lines with whether each sits inside (or delimits) a fenced code block.
fn lines_fenced(text: &str) -> Vec<(&str, bool)> {
    let mut open: Option<(char, usize)> = None;
    text.lines()
        .map(|l| match (open, fence(l)) {
            (None, Some(f)) => {
                open = Some(f);
                (l, true)
            }
            (Some((c, n)), Some((d, m)))
                if c == d && m >= n && l.trim().chars().all(|x| x == c) =>
            {
                open = None;
                (l, true)
            }
            (o, _) => (l, o.is_some()),
        })
        .collect()
}

/// `[fermata <id>[ <decision> <kind>]]` at the start of a line.
fn marker(line: &str) -> Option<(String, String, &str)> {
    let (head, rest) = line
        .trim_start()
        .strip_prefix("[fermata ")?
        .split_once(']')?;
    let (id, tag) = head.trim().split_once(' ').unwrap_or((head.trim(), ""));
    let ok = !id.is_empty() && !id.contains(['<', '>', '[', '…']);
    ok.then(|| (id.to_string(), tag.trim().to_string(), rest))
}

/// Split a prompt on its markers. Queued messages arrive merged, so one prompt can hold several.
pub fn segments(prompt: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = vec![];
    for (line, fenced) in lines_fenced(prompt) {
        match marker(line).filter(|_| !fenced) {
            Some((id, tag, rest)) => out.push(Segment {
                task: Some(id),
                tag,
                text: rest.trim_start().to_string(),
            }),
            None => match out.last_mut() {
                Some(s) => s.text += &format!("\n{line}"),
                None => out.push(Segment {
                    task: None,
                    tag: String::new(),
                    text: line.to_string(),
                }),
            },
        }
    }
    for s in &mut out {
        s.text = s.text.trim().to_string();
    }
    out.retain(|s| s.task.is_some() || !s.text.is_empty());
    out
}

/// Whether a word could be a task id: `ctx-12`, `ctx-1v.2.8`, `ABC-12`, `#12`.
fn id_shaped(w: &str) -> bool {
    let body = w.strip_prefix('#').unwrap_or(w);
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    body.chars().all(ok)
        && body.starts_with(|c: char| c.is_ascii_alphanumeric())
        && body.ends_with(|c: char| c.is_ascii_alphanumeric())
        && (body.contains('-') || body.contains(|c: char| c.is_ascii_digit()))
}

/// Every whole word of `text` that could be a task id, in order, once each.
pub fn ids_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let sep = |c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '#' | '/'));
    for w in text.split(sep) {
        let w = w.trim_matches(|c| matches!(c, '.' | '-' | '_'));
        // Dates and paths are not ids; a slash only separates ids.
        for w in w.split('/').filter(|_| !w.starts_with('/')) {
            let letters = w.contains(|c: char| c.is_ascii_alphabetic());
            if letters && w.contains('-') && id_shaped(w) && !out.iter().any(|o| o == w) {
                out.push(w.to_string());
            }
        }
    }
    out
}

/// The heading level of a line (`### x` → 3) and its text.
fn heading(line: &str) -> Option<(usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let t = line.trim_start_matches(' ');
    let n = t.chars().take_while(|c| *c == '#').count();
    let rest = &t[n..];
    (indent < 4 && (1..=6).contains(&n) && (rest.is_empty() || rest.starts_with([' ', '\t'])))
        .then(|| (n, rest.trim()))
}

/// The task ids a section heading opens with: `ctx-3`, `` `ctx-3`: done ``, `ctx-3, ctx-7`.
fn heading_ids(text: &str) -> Vec<String> {
    let mut out = vec![];
    for w in text.split_whitespace() {
        let joins = matches!(w, "and" | "&" | "+" | "/");
        if joins && !out.is_empty() {
            continue;
        }
        let parts: Vec<&str> = w
            .split([',', '/'])
            .map(|p| p.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '#'))
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty() || !parts.iter().all(|p| id_shaped(p)) {
            break;
        }
        out.extend(parts.iter().map(|p| p.to_string()));
        // Only a comma or a joiner continues the list; anything else ends it.
        if w.ends_with([':', '—', '.']) {
            break;
        }
    }
    out
}

/// Whether a heading's id-shaped word is taken for another task's id by readers who know the
/// task as `names`: `ctx-7` is; `Next-steps`, `Phase-1` and `2` are plain words.
fn is_task_id(id: &str, names: &[&str]) -> bool {
    if names.contains(&id) || id.starts_with('#') {
        return true;
    }
    let Some((prefix, rest)) = id.split_once('-') else {
        return false;
    };
    let mut cs = prefix.chars();
    let title_case =
        cs.next().is_some_and(|c| c.is_ascii_uppercase()) && cs.any(|c| c.is_ascii_lowercase());
    let same = |n: &&str| n.split_once('-').is_some_and(|(p, _)| p == prefix);
    names.iter().any(same)
        || prefix.starts_with(|c: char| c.is_ascii_alphabetic())
            && !title_case
            && rest.contains(|c: char| c.is_ascii_digit())
}

/// A reply cut at its `### <task-id>` headings; text outside every section has no ids.
pub fn sections(reply: &str) -> Vec<(Vec<String>, String)> {
    let mut v = split(&[], reply);
    v.retain(|s| !s.1.is_empty());
    v
}

/// `sections`, starting inside the section `open`. The last element is the section left open.
fn split(open: &[String], reply: &str) -> Vec<(Vec<String>, String)> {
    let mut out: Vec<(Vec<String>, String)> = vec![(open.to_vec(), String::new())];
    for (line, fenced) in lines_fenced(reply) {
        if !fenced
            && let Some((level, text)) = heading(line)
            && level <= 3
        {
            let ids = if level == 3 {
                heading_ids(text)
            } else {
                vec![]
            };
            // A plain heading outside any section changes nothing.
            if !ids.is_empty() || !out.last().is_some_and(|s| s.0.is_empty()) {
                out.push((ids, String::new()));
            }
        }
        let body = &mut out.last_mut().expect("never empty").1;
        body.push_str(line);
        body.push('\n');
    }
    for s in &mut out {
        s.1 = s.1.trim().to_string();
    }
    out
}

/// The part of a reply that is about the task known by `names`: its own sections and, when the
/// prompt addressed it, text outside any. `open` carries the section on to the next block.
pub fn reply_for(
    reply: &str,
    names: &[&str],
    prompted: bool,
    open: &mut Vec<String>,
) -> Option<String> {
    let parts = split(open, reply);
    *open = parts.last().map(|s| s.0.clone()).unwrap_or_default();
    let v: Vec<String> = parts
        .into_iter()
        .filter(|(ids, text)| {
            let own = ids.iter().any(|i| names.contains(&i.as_str()));
            let others = ids.iter().any(|i| is_task_id(i, names));
            !text.is_empty() && (own || prompted && !others)
        })
        .map(|s| s.1)
        .collect();
    (!v.is_empty()).then(|| v.join("\n\n"))
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Sep,
}

/// A rough shell word split: quotes and escapes removed, `;|&()` and newlines as separators.
/// How deep `$(…)` inside `$(…)` is read; a hook must not run out of stack on a command.
const MAX_DEPTH: usize = 16;

fn tokens(cmd: &str) -> Vec<Tok> {
    tokens_at(cmd, 0)
}

fn tokens_at(cmd: &str, depth: usize) -> Vec<Tok> {
    let mut out = vec![];
    let mut cur: Option<String> = None;
    let mut subs: Vec<String> = vec![];
    let mut it = cmd.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\'' => {
                let w = cur.get_or_insert_default();
                w.extend(it.by_ref().take_while(|x| *x != '\''));
            }
            '"' => {
                let w = cur.get_or_insert_default();
                while let Some(x) = it.next() {
                    match x {
                        '"' => break,
                        '\\' => w.extend(it.next()),
                        '$' if it.peek() == Some(&'(') => {
                            it.next();
                            subs.push(substitution(&mut it));
                        }
                        x => w.push(x),
                    }
                }
            }
            '\\' => match it.next() {
                Some('\n') | None => {}
                Some(x) => cur.get_or_insert_default().push(x),
            },
            ' ' | '\t' => out.extend(cur.take().map(Tok::Word)),
            ';' | '|' | '&' | '(' | ')' | '\n' | '`' => {
                out.extend(cur.take().map(Tok::Word));
                out.push(Tok::Sep);
            }
            '#' if cur.is_none() => {
                while it.peek().is_some_and(|x| *x != '\n') {
                    it.next();
                }
            }
            c => cur.get_or_insert_default().push(c),
        }
    }
    out.extend(cur.take().map(Tok::Word));
    // A quoted `$(…)` runs a command of its own.
    for sub in subs.iter().filter(|_| depth < MAX_DEPTH) {
        out.push(Tok::Sep);
        out.extend(tokens_at(sub, depth + 1));
    }
    out
}

/// The inside of `$(…)`, up to its closing parenthesis outside quotes.
fn substitution(it: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let (mut out, mut depth, mut quote) = (String::new(), 1, None);
    while let Some(c) = it.next() {
        match (quote, c) {
            (_, '\\') if quote != Some('\'') => {
                out.push(c);
                out.extend(it.next());
                continue;
            }
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if q == c => quote = None,
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            break;
        }
        out.push(c);
    }
    out
}

fn var_name(w: &str) -> Option<&str> {
    let n = w.strip_prefix('$')?;
    let n = n
        .strip_prefix('{')
        .map(|x| x.trim_end_matches('}'))
        .unwrap_or(n);
    let n = n.split([':', '#', '%']).next()?;
    let ok = n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    (ok && !n.is_empty()).then_some(n)
}

/// The values a variable is given in the command: `for n in …` and `n=…`.
fn values_of<'a>(toks: &'a [Tok], name: &str) -> Vec<&'a str> {
    let mut out = vec![];
    for (i, t) in toks.iter().enumerate() {
        let Tok::Word(w) = t else { continue };
        if let Some(v) = w.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            out.push(v);
        }
        let is = |k: usize, s: &str| matches!(toks.get(k), Some(Tok::Word(x)) if x == s);
        if w == "for" && is(i + 1, name) && is(i + 2, "in") {
            out.extend(toks[i + 3..].iter().map_while(|t| match t {
                Tok::Word(x) => Some(x.as_str()),
                Tok::Sep => None,
            }));
        }
    }
    out
}

/// The tracker writes in a shell command, as (task id, verb). An id held in a variable the
/// command does not set counts as every id the command mentions: too many, never none.
pub fn tracker_writes(cmd: &str) -> Vec<(String, String)> {
    let toks = tokens(cmd);
    // `VAR=…tracker.sh` in the same command, or the name every baton skill uses.
    let mut aliases = vec!["TRK".to_string()];
    for t in &toks {
        if let Tok::Word(w) = t
            && let Some((n, v)) = w.split_once('=')
            && v.trim_end_matches(')').ends_with("tracker.sh")
        {
            aliases.push(n.to_string());
        }
    }
    let is_tracker = |w: &str| {
        w == "tracker.sh"
            || w.ends_with("/tracker.sh") && !w.contains('=')
            || var_name(w).is_some_and(|n| aliases.iter().any(|a| a == n))
    };
    let mut out: Vec<(String, String)> = vec![];
    let mut push = |id: &str, verb: &str| {
        let p = (id.to_string(), verb.to_string());
        if !out.contains(&p) {
            out.push(p);
        }
    };
    for (i, t) in toks.iter().enumerate() {
        if !matches!(t, Tok::Word(w) if is_tracker(w)) {
            continue;
        }
        let mut args = toks[i + 1..].iter().map_while(|t| match t {
            Tok::Word(w) => Some(w.as_str()),
            Tok::Sep => None,
        });
        let mut verb = args.next();
        while let Some(v) = verb.filter(|v| v.starts_with('-')) {
            if !v.contains('=') {
                args.next();
            }
            verb = args.next();
        }
        let Some(verb) = verb.filter(|v| WRITE_VERBS.contains(v)) else {
            continue;
        };
        let Some(id) = args.next() else { continue };
        if !id.contains('$') {
            push(id, verb);
            continue;
        }
        let vals: Vec<String> = var_name(id)
            .map(|n| values_of(&toks, n))
            .unwrap_or_default()
            .into_iter()
            .filter(|v| !v.contains('$'))
            // A quoted list (`IDS="a b"`) is one word.
            .flat_map(|v| v.split_whitespace().map(str::to_string))
            .collect();
        let vals = if vals.is_empty() { ids_in(cmd) } else { vals };
        for v in vals {
            push(&v, verb);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(reply: &str, names: &[&str], prompted: bool) -> Option<String> {
        reply_for(reply, names, prompted, &mut vec![])
    }

    #[test]
    fn plain_headings_do_not_hide_a_reply() {
        for h in [
            "Follow-up",
            "Next-steps",
            "Phase-1",
            "2. Results",
            "e2e-tests",
            "3-way merge",
        ] {
            let r = format!("### {h}\nmine");
            assert_eq!(one(&r, &["ctx-1"], true).as_deref(), Some(r.as_str()));
            assert_eq!(one(&r, &["ctx-1"], false), None);
        }
        // Another task's, with or without a digit, in this tracker or another.
        for h in [
            "ctx-kpik",
            "ctx-9",
            "other-7",
            "AB-12",
            "#12",
            "Phase-1, ctx-9",
        ] {
            assert_eq!(one(&format!("### {h}\nx"), &["ctx-1"], true), None, "{h}");
        }
        assert!(one("### ctx-kpik: done\nx", &["ctx-kpik"], false).is_some());
    }

    #[test]
    fn a_section_stays_open_across_blocks() {
        let blocks = ["both\n### ctx-7\nlooking", "fixed", "## Summary\nall"];
        let see = |names: &[&str], prompted| -> Vec<Option<String>> {
            let mut open = vec![];
            blocks
                .map(|b| reply_for(b, names, prompted, &mut open))
                .into()
        };
        let some = |s: &str| Some(s.to_string());
        assert_eq!(
            see(&["ctx-1"], true),
            [some("both"), None, some("## Summary\nall")]
        );
        assert_eq!(
            see(&["ctx-7"], false),
            [some("### ctx-7\nlooking"), some("fixed"), None]
        );
    }

    #[test]
    fn deep_substitutions_do_not_overflow() {
        let n = 20_000;
        let cmd = format!(
            "{}tracker.sh note ctx-1 x{}",
            "echo \"$(".repeat(n),
            ")\"".repeat(n)
        );
        tracker_writes(&cmd);
        let two = r#"echo "$(echo "$(tracker.sh note ctx-1 x)")""#;
        assert_eq!(
            tracker_writes(two),
            [("ctx-1".to_string(), "note".to_string())]
        );
    }

    fn seg(task: &str, tag: &str, text: &str) -> Segment {
        Segment {
            task: (!task.is_empty()).then(|| task.into()),
            tag: tag.into(),
            text: text.into(),
        }
    }

    #[test]
    fn merged_prompts_split_on_every_segment() {
        assert_eq!(
            segments("[fermata ctx-1] one\nmore\n\n[fermata ctx-2 d4f2a91 answer] Option 1\n"),
            vec![
                seg("ctx-1", "", "one\nmore"),
                seg("ctx-2", "d4f2a91 answer", "Option 1")
            ]
        );
        assert_eq!(
            segments("typed first\n  [fermata ctx-1v.2] go"),
            vec![seg("", "", "typed first"), seg("ctx-1v.2", "", "go")]
        );
        assert_eq!(segments("plain"), vec![seg("", "", "plain")]);
        assert!(segments(" \n").is_empty());
        // The same task twice stays two messages.
        assert_eq!(segments("[fermata a-1] x\n[fermata a-1] y").len(), 2);
    }

    #[test]
    fn quoted_markers_are_not_segments() {
        for p in [
            "see `[fermata ctx-12] text` in the doc",
            "```\n[fermata ctx-12] text\n```",
            "[fermata <id>] is the form",
            "[fermata ] x",
            "[fermata ctx-12 never closed",
            "[fermatas ctx-12] x",
        ] {
            assert!(segments(p).iter().all(|s| s.task.is_none()), "{p:?}");
        }
        // A fence only closes on its own kind, at least as long.
        let p = "````\n```\n[fermata a-1] x\n````\n[fermata a-2] y";
        let got: Vec<_> = segments(p).into_iter().filter_map(|s| s.task).collect();
        assert_eq!(got, vec!["a-2"]);
    }

    #[test]
    fn sections_split_by_task() {
        let r = "Swept.\n\n### ctx-3\nmerged\n#### detail\nx\n\n### ctx-7: waiting\nCI red\n\n\
                 ## Totals\n2 done";
        let s = sections(r);
        assert_eq!(s.len(), 4);
        assert_eq!(s[1].0, vec!["ctx-3"]);
        assert!(s[1].1.contains("#### detail\nx"));
        assert_eq!(s[2].0, vec!["ctx-7"]);
        assert!(s[3].0.is_empty());
        assert_eq!(s[3].1, "## Totals\n2 done");
    }

    #[test]
    fn section_headings_are_exact() {
        let ids = |r: &str| -> Vec<String> { sections(r).into_iter().flat_map(|s| s.0).collect() };
        assert_eq!(ids("### `ctx-3`"), vec!["ctx-3"]);
        assert_eq!(ids("### [ctx-3] title words"), vec!["ctx-3"]);
        assert_eq!(ids("### **ctx-1v.2.8** — done"), vec!["ctx-1v.2.8"]);
        assert_eq!(
            ids("### ctx-3, ctx-7 and ctx-9"),
            ["ctx-3", "ctx-7", "ctx-9"]
        );
        assert_eq!(ids("### ctx-3/ctx-7"), ["ctx-3", "ctx-7"]);
        assert_eq!(ids("### ctx-3: ctx-7 blocks it"), vec!["ctx-3"]);
        assert_eq!(ids("### #12 fix"), vec!["#12"]);
        for r in [
            "### Summary",
            "###ctx-3",
            "## ctx-3",
            "#### ctx-3",
            "    ### ctx-3",
            "```md\n### ctx-3\n```",
            "text ### ctx-3",
            "### The ctx-3 task",
            "###",
        ] {
            assert!(ids(r).is_empty(), "{r:?}");
        }
        // An unclosed fence swallows the rest: too little shown as a section, never a wrong one.
        assert!(ids("```\nx\n### ctx-3").is_empty());
    }

    #[test]
    fn a_reply_is_cut_for_each_task() {
        let r = "Both looked at.\n\n### ctx-1\none\n\n### ctx-1.2\ntwo\n";
        assert_eq!(one(r, &["ctx-1"], false).unwrap(), "### ctx-1\none");
        assert_eq!(
            one(r, &["ctx-1"], true).unwrap(),
            "Both looked at.\n\n### ctx-1\none"
        );
        assert_eq!(one(r, &["ctx-1.2"], false).unwrap(), "### ctx-1.2\ntwo");
        assert_eq!(one(r, &["ctx-9"], false), None);
        assert_eq!(one(r, &["ctx-9"], true).unwrap(), "Both looked at.");
        assert_eq!(one("just text", &["ctx-9"], true).unwrap(), "just text");
        assert_eq!(one("### ctx-1\nx", &["ctx-9"], true), None);
        assert_eq!(
            one("### AB-1\nx", &["ctx-9", "AB-1"], false).unwrap(),
            "### AB-1\nx"
        );
    }

    fn w(cmd: &str) -> Vec<String> {
        tracker_writes(cmd)
            .into_iter()
            .map(|(i, v)| format!("{v} {i}"))
            .collect()
    }

    #[test]
    fn writes_with_literal_ids() {
        assert_eq!(
            w(r#""$TRK" note ctx-5 "blocked by ctx-9""#),
            vec!["note ctx-5"]
        );
        assert_eq!(
            w("${TRK} state 'ctx-5' working; echo done"),
            vec!["state ctx-5"]
        );
        assert_eq!(
            w(
                r#"/x/baton/scripts/tracker.sh --context - decide ctx-5 --question "a; b" <<<"$CTX""#
            ),
            vec!["decide ctx-5"]
        );
        assert_eq!(
            w(
                "T=\"$B/scripts/tracker.sh\"\n\"$T\" reply ctx-5 d1 --kind response --text x && \"$T\" resolve ctx-5 d1"
            ),
            vec!["reply ctx-5", "resolve ctx-5"]
        );
        assert_eq!(
            w(r#"X="$("$TRK" close ctx-5 --reason "a (b)")" || exit 1"#),
            vec!["close ctx-5"]
        );
        assert_eq!(
            w(r#"TRK="$(ls -d /x/baton | tail -1)/scripts/tracker.sh"; "$TRK" note a-1 x"#),
            vec!["note a-1"]
        );
        assert_eq!(w(r#"X=$("$TRK" claim ctx-5)"#), vec!["claim ctx-5"]);
    }

    #[test]
    fn writes_with_variable_ids() {
        assert_eq!(
            w(r#"for id in a-1 a-2; do "$TRK" note "$id" "x"; done"#),
            vec!["note a-1", "note a-2"]
        );
        assert_eq!(
            w(r#"LEAF=a-1; $TRK state ${LEAF} clear"#),
            vec!["state a-1"]
        );
        // Unset here: every id the command mentions.
        assert_eq!(
            w("while read -r i; do \"$TRK\" note \"$i\" x; done <<<\"a-1\nb-2\""),
            vec!["note a-1", "note b-2"]
        );
    }

    #[test]
    fn reads_and_mentions_are_not_writes() {
        for c in [
            "\"$TRK\" get ctx-5",
            "$TRK decisions ctx-5 --all | jq .",
            "grep -n 'tracker.sh note ctx-5' x",
            "cat baton/scripts/tracker.sh note",
            "# $TRK note ctx-5 x",
            "echo note ctx-5",
            "\"$TRK\" create 'note ctx-5'",
            "$TRK note",
            "",
        ] {
            assert!(
                w(c).is_empty() || c.starts_with("cat"),
                "{c:?} -> {:?}",
                w(c)
            );
        }
    }

    #[test]
    fn ids_are_whole_words() {
        assert_eq!(ids_in("ctx-9: build"), vec!["ctx-9"]);
        assert_eq!(ids_in("[XY] ctx-1v.2.8 resume."), vec!["ctx-1v.2.8"]);
        assert_eq!(
            ids_in("on 2026-11-23 see /tmp/ctx-5 plain"),
            Vec::<String>::new()
        );
    }
}
