//! The one palette. Every colour in the UI comes from here (see DESIGN.md, "Layout and colour").
//! Named ANSI colours only, so each terminal theme keeps them readable on its own background.
use crate::model::{Badge, JoinedTask};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use regex::Regex;

pub const ID: Color = Color::Magenta;
pub const PR: Color = Color::Blue;
pub const ACCENT: Color = Color::Cyan;
pub const DIM: Color = Color::DarkGray;
pub const MUTED: Color = Color::Gray;
pub const SELECTED_BG: Color = Color::DarkGray;
pub const PIN: Color = Color::Yellow;
pub const ALERT: Color = Color::Red;
pub const WARN: Color = Color::Yellow;
pub const GOOD: Color = Color::Green;

pub fn id() -> Style {
    Style::default().fg(ID)
}
pub fn pr() -> Style {
    Style::default().fg(PR)
}
pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}
pub fn dim() -> Style {
    Style::default().fg(DIM)
}
pub fn muted() -> Style {
    Style::default().fg(MUTED)
}
pub fn pin() -> Style {
    Style::default().fg(PIN)
}
pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}
pub fn selected() -> Style {
    Style::default().bg(SELECTED_BG)
}
pub fn header_row() -> Style {
    Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}
pub fn tab_on() -> Style {
    Style::default().fg(Color::Black).bg(ACCENT)
}
pub fn banner() -> Style {
    Style::default().fg(Color::Black).bg(WARN)
}
pub fn alert() -> Style {
    Style::default().fg(ALERT).add_modifier(Modifier::BOLD)
}
pub fn border(focused: bool) -> Style {
    if focused {
        accent().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

pub fn badge(b: Badge) -> Style {
    let c = match b {
        Badge::NeedsYou => Color::Red,
        Badge::Overdue => Color::Magenta,
        Badge::CiFailing => Color::LightRed,
        Badge::ReadyToMerge => Color::Green,
        Badge::WaitingOnCi => Color::Yellow,
        Badge::WorkerRunning => Color::Cyan,
        Badge::Interactive => Color::Blue,
        Badge::Parked => Color::DarkGray,
        Badge::Idle => Color::Gray,
    };
    let s = Style::default().fg(c);
    if matches!(b, Badge::NeedsYou | Badge::Overdue) {
        s.add_modifier(Modifier::BOLD)
    } else {
        s
    }
}

/// Finds ids and PR numbers in free text: the known task ids and ticket/epic keys as whole
/// words, and `#<digits>`.
#[derive(Clone)]
pub struct Marks {
    re: Regex,
}

impl Default for Marks {
    fn default() -> Self {
        Self::new(std::iter::empty::<String>())
    }
}

impl Marks {
    pub fn new<I: IntoIterator<Item = S>, S: AsRef<str>>(ids: I) -> Self {
        let mut ids: Vec<String> = ids
            .into_iter()
            .map(|s| s.as_ref().trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        // Longest first, so `abc-1.2` wins over `abc-1`.
        ids.sort_by_key(|s| std::cmp::Reverse(s.len()));
        ids.dedup();
        let alts: Vec<String> = ids.iter().map(|s| regex::escape(s)).collect();
        let pat = if alts.is_empty() {
            r"(?P<pr>#\d+\b)".to_string()
        } else {
            format!(
                r"(?P<pr>#\d+\b)|(?P<id>(?:^|[^\w.-])(?:{})(?:$|[^\w.-]|\.\W|\.$))",
                alts.join("|")
            )
        };
        Self {
            re: Regex::new(&pat).expect("escaped ids form a valid regex"),
        }
    }

    pub fn from_tasks(tasks: &[JoinedTask]) -> Self {
        let mut ids = vec![];
        for t in tasks {
            ids.push(t.task.id.clone());
            ids.extend(t.ticket_key.clone());
            if let Some(e) = t.ticket.as_ref().and_then(|i| i.epic.as_ref()) {
                ids.push(e.key.clone());
            }
        }
        Self::new(ids)
    }

    /// `text` split into spans: ids and PR numbers in their colours, the rest in `base`.
    pub fn spans(&self, text: &str, base: Style) -> Vec<Span<'static>> {
        let mut out = vec![];
        let mut at = 0;
        for c in self.re.captures_iter(text) {
            let (m, st) = match (c.name("pr"), c.name("id")) {
                (Some(m), _) => (m.start()..m.end(), base.patch(pr())),
                (None, Some(m)) => (trim_edges(text, m.start(), m.end()), base.patch(id())),
                _ => continue,
            };
            if m.start > at {
                out.push(Span::styled(text[at..m.start].to_string(), base));
            }
            out.push(Span::styled(text[m.clone()].to_string(), st));
            at = m.end;
        }
        if at < text.len() || out.is_empty() {
            out.push(Span::styled(text[at..].to_string(), base));
        }
        out
    }
}

/// The id alternative also consumes one boundary character on each side; drop them.
fn trim_edges(text: &str, mut s: usize, mut e: usize) -> std::ops::Range<usize> {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let first = text[s..e].chars().next();
    if first.is_some_and(|c| !word(c)) {
        s += first.unwrap().len_utf8();
    }
    while let Some(c) = text[s..e].chars().next_back() {
        if word(c) {
            break;
        }
        e -= c.len_utf8();
    }
    s..e
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked(m: &Marks, text: &str) -> Vec<(String, Option<Color>)> {
        m.spans(text, Style::default())
            .into_iter()
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect()
    }

    #[test]
    fn ids_and_prs_get_their_own_colours() {
        let m = Marks::new(["ctx-ab1", "ABC-12"]);
        assert_eq!(
            marked(&m, "ctx-ab1: review #42 for ABC-12."),
            vec![
                ("ctx-ab1".into(), Some(ID)),
                (": review ".into(), None),
                ("#42".into(), Some(PR)),
                (" for ".into(), None),
                ("ABC-12".into(), Some(ID)),
                (".".into(), None),
            ]
        );
        assert_ne!(ID, PR);
    }

    #[test]
    fn ids_match_whole_words_only() {
        let m = Marks::new(["ctx-ab1", "ABC-1"]);
        assert_eq!(marked(&m, "ctx-ab12 ABC-10 xctx-ab1").len(), 1);
        // A child id is not its parent followed by text.
        assert_eq!(marked(&m, "ctx-ab1.2")[0].1, None);
        // A longer hyphenated word (a branch name) is not the id.
        assert_eq!(
            marked(&m, "ctx-ab1-some-slug")[0],
            ("ctx-ab1-some-slug".into(), None)
        );
        assert_eq!(marked(&m, "(ctx-ab1)")[1], ("ctx-ab1".into(), Some(ID)));
    }

    #[test]
    fn no_ids_still_marks_prs() {
        let m = Marks::default();
        assert_eq!(marked(&m, "PR #7")[1], ("#7".into(), Some(PR)));
        assert_eq!(marked(&m, ""), vec![("".into(), None)]);
    }
}
