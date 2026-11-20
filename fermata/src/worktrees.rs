//! Task → worktree/PR links from directory stores: the baton identity carriers git keeps in each
//! member repo's `<common dir>/worktrees/*/`. Plain file reads; never `git`, never the tracker.
use crate::config::{Context, glob};
use crate::model::Branch;
use crate::util::expand_tilde;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One parsed `baton-identity` file.
#[derive(Debug, Default, PartialEq)]
pub struct Carrier {
    pub leaf: String,
    pub branch: String,
    pub kind: String,
    pub pr: Option<String>,
    pub written: Option<String>,
}

pub fn parse_carrier(text: &str) -> Option<Carrier> {
    let mut c = Carrier::default();
    for l in text.lines().filter(|l| !l.starts_with('#')) {
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        let v = v.trim().to_string();
        match k.trim() {
            "leaf" => c.leaf = v,
            "branch" => c.branch = v,
            "kind" => c.kind = v,
            "pr" => c.pr = Some(v).filter(|s| !s.is_empty()),
            "written" => c.written = Some(v).filter(|s| !s.is_empty()),
            _ => {}
        }
    }
    (!c.leaf.is_empty()).then_some(c)
}

/// Member repo directories: `~` expanded, globs matched one path component at a time.
pub fn member_repo_dirs(patterns: &[String]) -> Vec<PathBuf> {
    let mut out = vec![];
    for p in patterns {
        let p = expand_tilde(p);
        let mut cur = vec![PathBuf::new()];
        for comp in p.components() {
            let s = comp.as_os_str().to_string_lossy();
            if !s.contains(['*', '?']) {
                cur.iter_mut().for_each(|c| c.push(comp));
                continue;
            }
            cur = cur
                .iter()
                .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten())
                .filter(|e| glob(&s, &e.file_name().to_string_lossy()))
                .map(|e| e.path())
                .collect();
            cur.sort();
        }
        out.extend(cur.into_iter().filter(|d| d.is_dir()));
    }
    out.dedup();
    out
}

/// A repo's git common dir: `.git` itself, or where a `.git` file's `gitdir:` points.
fn common_dir(repo: &Path) -> Option<PathBuf> {
    let g = repo.join(".git");
    if g.is_dir() {
        return Some(g);
    }
    let t = std::fs::read_to_string(&g).ok()?;
    let p = PathBuf::from(t.trim().strip_prefix("gitdir:")?.trim());
    let p = if p.is_relative() { repo.join(p) } else { p };
    // A linked worktree's gitdir is `<common>/worktrees/<name>`; `commondir` says so.
    match std::fs::read_to_string(p.join("commondir")) {
        Ok(c) => Some(p.join(c.trim())),
        Err(_) => Some(p),
    }
}

/// Every live baton worktree of one repo, as (leaf, entry), oldest carrier first.
pub fn scan_repo(repo: &Path) -> Vec<(String, Branch)> {
    let Some(wts) = common_dir(repo).map(|d| d.join("worktrees")) else {
        return vec![];
    };
    let name = repo
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut out: Vec<(String, Branch)> = std::fs::read_dir(wts)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let d = e.path();
            let c = parse_carrier(&std::fs::read_to_string(d.join("baton-identity")).ok()?)?;
            let gitdir = std::fs::read_to_string(d.join("gitdir")).ok()?;
            let wt = Path::new(gitdir.trim()).parent()?.to_path_buf();
            if !wt.is_dir() {
                return None;
            }
            let head = std::fs::read_to_string(d.join("HEAD")).unwrap_or_default();
            let branch = head
                .trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_string)
                .unwrap_or(c.branch);
            let b = Branch {
                repo: name.clone(),
                branch,
                worktree: wt.to_string_lossy().into_owned(),
                status: "open".into(),
                pr: c.pr,
                updated: c.written,
                ..Default::default()
            };
            Some((c.leaf, b))
        })
        .collect();
    out.sort_by(|a, b| a.1.updated.cmp(&b.1.updated));
    out
}

/// The context's local worktrees by task id.
pub fn local_branches(c: &Context) -> HashMap<String, Vec<Branch>> {
    let mut m: HashMap<String, Vec<Branch>> = HashMap::new();
    for r in member_repo_dirs(&c.member_repos) {
        for (leaf, b) in scan_repo(&r) {
            m.entry(leaf).or_default().push(b);
        }
    }
    m
}

/// Registry entries first, then local worktrees the registry does not already name.
pub fn merge(registry: Vec<Branch>, local: Vec<Branch>) -> Vec<Branch> {
    let mut out = registry;
    for l in local {
        let known = out.iter_mut().find(|r| {
            (!r.worktree.is_empty() && crate::sessions::same_path(&r.worktree, &l.worktree))
                || (r.repo == l.repo && r.branch == l.branch && !r.branch.is_empty())
        });
        match known {
            Some(r) if r.worktree.is_empty() => r.worktree = l.worktree,
            Some(_) => {}
            None => out.push(l),
        }
    }
    out
}

/// A review carrier's bare `pr=` number becomes the task's own PR URL when it names that number,
/// so the task's PR lookup and review rank share one PR cache entry.
pub fn adopt_pr_url(branches: &mut [Branch], task_pr_url: Option<&str>) {
    let Some(u) = task_pr_url else { return };
    for b in branches.iter_mut().filter(|b| b.pr_url.is_none()) {
        if b.pr
            .as_deref()
            .is_some_and(|n| u.ends_with(&format!("/pull/{n}")))
        {
            b.pr_url = Some(u.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("fermata-wt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d.canonicalize().unwrap())
    }

    /// A fake linked worktree: `<repo>/.git/worktrees/<name>/` plus the worktree directory.
    fn fake_wt(repo: &Path, name: &str, carrier: Option<&str>, head: &str) -> PathBuf {
        let gd = repo.join(".git/worktrees").join(name);
        std::fs::create_dir_all(&gd).unwrap();
        let wt = repo.with_file_name(format!(
            "{}-worktrees/{name}",
            repo.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(gd.join("gitdir"), format!("{}/.git\n", wt.display())).unwrap();
        std::fs::write(gd.join("HEAD"), head).unwrap();
        if let Some(c) = carrier {
            std::fs::write(gd.join("baton-identity"), c).unwrap();
        }
        wt
    }

    #[test]
    fn carrier_parses_and_needs_a_leaf() {
        let c = parse_carrier(
            "# comment\nschema=1\nleaf=ctx-1\nbranch=ctx-1-x\nkind=review\npr=42\nwritten=2026-01-02T00:00:00Z\n",
        )
        .unwrap();
        assert_eq!(
            (c.leaf.as_str(), c.kind.as_str(), c.pr.as_deref()),
            ("ctx-1", "review", Some("42"))
        );
        assert_eq!(parse_carrier("leaf=ctx-2\npr=\n").unwrap().pr, None);
        assert_eq!(parse_carrier("schema=1\nleaf=\n"), None);
    }

    #[test]
    fn scans_carriers_skipping_plain_and_gone_worktrees() {
        let t = tmp("scan");
        let repo = t.0.join("code/app");
        std::fs::create_dir_all(&repo).unwrap();
        let a = fake_wt(
            &repo,
            "ctx-1-a",
            Some("leaf=ctx-1\nbranch=old\nwritten=2026-01-02T00:00:00Z\n"),
            "ref: refs/heads/ctx-1-a\n",
        );
        let r = fake_wt(
            &repo,
            "ctx-2-r",
            Some("leaf=ctx-2\nbranch=ctx-2-r\nkind=review\npr=7\nwritten=2026-01-01T00:00:00Z\n"),
            "0123abcd\n",
        );
        fake_wt(&repo, "plain", None, "ref: refs/heads/plain\n");
        let gone = fake_wt(
            &repo,
            "ctx-3-g",
            Some("leaf=ctx-3\n"),
            "ref: refs/heads/g\n",
        );
        std::fs::remove_dir_all(&gone).unwrap();

        let v = scan_repo(&repo);
        let ids: Vec<&str> = v.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            ids,
            ["ctx-2", "ctx-1"],
            "oldest carrier first; plain and gone skipped"
        );
        let (_, b1) = &v[1];
        assert_eq!(b1.worktree, a.to_string_lossy());
        assert_eq!((b1.repo.as_str(), b1.branch.as_str()), ("app", "ctx-1-a"));
        assert_eq!(b1.status, "open");
        let (_, b2) = &v[0];
        assert_eq!(b2.worktree, r.to_string_lossy());
        assert_eq!(
            b2.branch, "ctx-2-r",
            "detached HEAD falls back to the carrier"
        );
        assert_eq!(b2.pr.as_deref(), Some("7"));
    }

    #[test]
    fn local_branches_expand_member_globs() {
        let t = tmp("globs");
        let code = t.0.join("code");
        for r in ["ctx-one", "ctx-two", "other"] {
            let repo = code.join(r);
            std::fs::create_dir_all(&repo).unwrap();
            fake_wt(
                &repo,
                &format!("t-{r}"),
                Some(&format!("leaf=t-{r}\n")),
                "ref: refs/heads/x\n",
            );
        }
        let c = Context {
            name: "c".into(),
            workspace: t.0.clone(),
            tracker_dir: None,
            github_owner: None,
            home: None,
            member_repos: vec![format!("{}/ctx-*", code.display())],
            ctx_json: String::new(),
            adapter: None,
            adapter_error: None,
        };
        let m = local_branches(&c);
        let mut keys: Vec<_> = m.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["t-ctx-one", "t-ctx-two"]);
    }

    #[test]
    fn a_dot_git_file_resolves_to_the_common_dir() {
        let t = tmp("gitfile");
        let real = t.0.join("store");
        let wt = fake_wt(
            &real,
            "ctx-5-x",
            Some("leaf=ctx-5\n"),
            "ref: refs/heads/b\n",
        );
        let repo = t.0.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join(".git"),
            format!("gitdir: {}/.git\n", real.display()),
        )
        .unwrap();
        let v = scan_repo(&repo);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].1.worktree, wt.to_string_lossy());
    }

    #[test]
    fn merge_keeps_registry_first_without_duplicates() {
        let reg = vec![
            Branch {
                repo: "app".into(),
                branch: "b1".into(),
                worktree: "/w/1".into(),
                pr_url: Some("u".into()),
                ..Default::default()
            },
            Branch {
                repo: "app".into(),
                branch: "b2".into(),
                ..Default::default()
            },
        ];
        let local = vec![
            Branch {
                repo: "app".into(),
                branch: "b1".into(),
                worktree: "/w/1".into(),
                ..Default::default()
            },
            Branch {
                repo: "app".into(),
                branch: "b2".into(),
                worktree: "/w/2".into(),
                ..Default::default()
            },
            Branch {
                repo: "app".into(),
                branch: "b3".into(),
                worktree: "/w/3".into(),
                ..Default::default()
            },
        ];
        let m = merge(reg, local);
        let wts: Vec<_> = m.iter().map(|b| b.worktree.as_str()).collect();
        assert_eq!(wts, ["/w/1", "/w/2", "/w/3"]);
        assert_eq!(m[0].pr_url.as_deref(), Some("u"));
    }

    #[test]
    fn review_pr_number_adopts_the_task_url() {
        let mut b = vec![Branch {
            pr: Some("7".into()),
            ..Default::default()
        }];
        adopt_pr_url(&mut b, Some("https://h/o/r/pull/77"));
        assert_eq!(b[0].pr_url, None, "a different number is not adopted");
        adopt_pr_url(&mut b, Some("https://h/o/r/pull/7"));
        assert_eq!(b[0].pr_url.as_deref(), Some("https://h/o/r/pull/7"));
    }
}
