//! Two-tier search: instant fuzzy over metadata, debounced full-text over messages.
use crate::catalog::Catalog;
use crate::model::SessionMeta;
use memchr::memmem;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use rayon::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

const PROMPT_CHARS: usize = 120;

fn haystack(meta: &SessionMeta) -> String {
    let prompt: String = meta.first_prompt.chars().take(PROMPT_CHARS).collect();
    format!(
        "{} {} {} {}",
        meta.title,
        meta.cwd.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        meta.branch.as_deref().unwrap_or(""),
        prompt
    )
}

pub fn fuzzy(catalog: &Catalog, candidates: &[usize], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return candidates.to_vec();
    }
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = candidates
        .iter()
        .filter_map(|&i| {
            let hay = haystack(&catalog.sessions[i].meta);
            pattern.score(Utf32Str::new(&hay, &mut buf), &mut matcher).map(|score| (score, i))
        })
        .collect();
    let sessions = &catalog.sessions;
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0).then(sessions[b.1].meta.last_ts.cmp(&sessions[a.1].meta.last_ts))
    });
    scored.into_iter().map(|(_, i)| i).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextHit {
    pub session: usize,
    pub message_index: usize,
    pub snippet: String,
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// ~100-char single-line window around a match. `pos`/`len` are byte offsets into `lower`.
pub fn snippet(original: &str, lower: &str, pos: usize, len: usize) -> String {
    // Offsets are only valid for `original` if lowercasing didn't change byte lengths.
    let src = if original.len() == lower.len() { original } else { lower };
    let start = floor_boundary(src, pos.saturating_sub(40));
    let end = ceil_boundary(src, (pos + len + 60).min(src.len()));
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&src[start..end].replace('\n', " "));
    if end < src.len() {
        out.push('…');
    }
    out
}

pub fn full_text(
    catalog: &Catalog,
    candidates: &[usize],
    query: &str,
    cancel: Option<(&AtomicU64, u64)>,
) -> Option<Vec<TextHit>> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Some(Vec::new());
    }
    let finder = memmem::Finder::new(needle.as_bytes());
    let cancelled = || cancel.is_some_and(|(current, mine)| current.load(Ordering::Relaxed) != mine);

    let mut hits: Vec<TextHit> = candidates
        .par_iter()
        .filter_map(|&i| {
            if cancelled() || !catalog.may_contain(i, &needle) {
                return None;
            }
            catalog.messages(i).iter().enumerate().find_map(|(mi, m)| {
                let lower = m.text.to_lowercase();
                finder.find(lower.as_bytes()).map(|pos| TextHit {
                    session: i,
                    message_index: mi,
                    snippet: snippet(&m.text, &lower, pos, needle.len()),
                })
            })
        })
        .collect();
    if cancelled() {
        return None;
    }
    let sessions = &catalog.sessions;
    hits.sort_by(|a, b| sessions[b.session].meta.last_ts.cmp(&sessions[a.session].meta.last_ts));
    Some(hits)
}

pub struct SearchWorker {
    tx: mpsc::Sender<(u64, String)>,
    pub results: mpsc::Receiver<(u64, Vec<TextHit>)>,
    generation: Arc<AtomicU64>,
}

impl SearchWorker {
    pub fn spawn(catalog: Arc<Catalog>, debounce: Duration) -> SearchWorker {
        let (tx, rx) = mpsc::channel::<(u64, String)>();
        let (result_tx, results) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        std::thread::spawn(move || {
            let all: Vec<usize> = (0..catalog.sessions.len()).collect();
            while let Ok(mut job) = rx.recv() {
                loop {
                    match rx.recv_timeout(debounce) {
                        Ok(newer) => job = newer,
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                let (job_generation, query) = job;
                if let Some(hits) = full_text(&catalog, &all, &query, Some((&current, job_generation))) {
                    if result_tx.send((job_generation, hits)).is_err() {
                        return;
                    }
                }
            }
        });
        SearchWorker { tx, results, generation }
    }

    /// Queue a query; returns its generation. Older in-flight searches are cancelled.
    pub fn submit(&self, query: &str) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.tx.send((generation, query.to_string()));
        generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake_catalog;

    fn all(c: &Catalog) -> Vec<usize> {
        (0..c.sessions.len()).collect()
    }

    #[test]
    fn empty_query_keeps_candidates() {
        let c = fake_catalog();
        assert_eq!(fuzzy(&c, &[2, 0], "  "), vec![2, 0]);
    }

    #[test]
    fn fuzzy_matches_title_case_insensitively() {
        let c = fake_catalog();
        let r = fuzzy(&c, &all(&c), "KUBER");
        assert_eq!(r.first(), Some(&1));
        assert!(!r.contains(&0));
    }

    #[test]
    fn full_text_finds_message_and_snippet() {
        let c = fake_catalog();
        let hits = full_text(&c, &all(&c), "pineapple", None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session, 1);
        assert_eq!(hits[0].message_index, 0);
        assert!(hits[0].snippet.contains("PINEAPPLE"));
    }

    #[test]
    fn full_text_respects_candidates_and_empty_query() {
        let c = fake_catalog();
        assert!(full_text(&c, &[0], "pineapple", None).unwrap().is_empty());
        assert!(full_text(&c, &all(&c), "", None).unwrap().is_empty());
    }

    #[test]
    fn cancelled_search_returns_none() {
        let c = fake_catalog();
        let generation = AtomicU64::new(5);
        assert!(full_text(&c, &all(&c), "docker", Some((&generation, 4))).is_none());
        assert!(full_text(&c, &all(&c), "docker", Some((&generation, 5))).is_some());
    }

    #[test]
    fn snippet_windows_and_ellipses() {
        let text = format!("{}needle{}", "a".repeat(100), "b".repeat(100));
        let lower = text.to_lowercase();
        let s = snippet(&text, &lower, 100, 6);
        assert!(s.starts_with('…'));
        assert!(s.ends_with('…'));
        assert!(s.contains("needle"));
        let short = snippet("line one\nneedle", "line one\nneedle", 9, 6);
        assert_eq!(short, "line one needle");
    }

    #[test]
    fn snippet_handles_multibyte() {
        let text = "é".repeat(60) + "needle";
        let lower = text.to_lowercase();
        let pos = lower.find("needle").unwrap();
        assert!(snippet(&text, &lower, pos, 6).contains("needle"));
    }

    #[test]
    fn worker_returns_latest_generation_only() {
        let worker = SearchWorker::spawn(Arc::new(fake_catalog()), Duration::from_millis(50));
        worker.submit("docker");
        let latest = worker.submit("pineapple");
        let (generation, hits) = worker.results.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(generation, latest);
        assert_eq!(hits[0].session, 1);
        assert!(worker.results.recv_timeout(Duration::from_millis(200)).is_err());
    }
}
