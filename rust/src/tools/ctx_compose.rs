//! `ctx_compose` — task composer (Phase 2 of the efficiency epic).
//!
//! The biggest agent win is a single "rich per call" tool that returns ranked
//! files *with* inline bodies, replacing the typical search → read → outline →
//! read chain (3-5 calls) with one.
//!
//! lean-ctx already has the building blocks as separate tools; this composes
//! them into one response for a natural-language task:
//!   1. extracted keywords,
//!   2. hybrid-search ranked files (index built on demand),
//!   3. associative (graph spreading activation).

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use crate::core::config::{Config, IndexingMode};

use crate::core::index_pipeline::pipeline::IndexPipeline;
use crate::core::tokens::count_tokens;
use crate::tools::CrpMode;

/// Wall-time budget for the associative (graph spreading-activation) stage.
/// Opening/building the graph index is `O(corpus)` on a cold repo, so — like
/// semantic ranking — we bound it and skip the (purely additive) section on
/// overrun while the detached worker warms the index. `LEAN_CTX_COMPOSE_GRAPH_BUDGET_MS`.
const DEFAULT_GRAPH_BUDGET_MS: u64 = 1500;

fn graph_budget() -> Duration {
    let ms = std::env::var("LEAN_CTX_COMPOSE_GRAPH_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_GRAPH_BUDGET_MS);
    Duration::from_millis(ms)
}

/// Per-hop activation decay and hop count for spreading activation. Small decay
/// keeps activation local (structurally near the seeds); 3 hops covers
/// import→callee→sibling chains without diffusing across the whole graph.
const SPREAD_DECAY: f64 = 0.6;
const SPREAD_HOPS: usize = 3;
/// How many associative neighbours to surface.
const SPREAD_TOP_K: usize = 8;

/// Build the associative-relevance block: spreading activation seeded at the
/// files the task keywords resolve to, propagated over the union of the static
/// import/call graph and the *learned* Hebbian co-access graph. Returns an empty
/// string when no graph/seeds are available. Runs entirely in the worker thread
/// so [`associative_block_budgeted`] can bound it.
fn build_associative_block(project_root: &str, keywords: &[String]) -> String {
    use rusqlite::params;

    let root = Path::new(project_root);
    let db_path = crate::core::index_namespace::vectors_dir(root).join("code_index.db");
    let Ok(conn) = rusqlite::Connection::open(&db_path) else {
        return String::new();
    };

    // Seeds: find files whose symbols match any keyword via FTS5.
    let mut seed_files: Vec<String> = Vec::new();
    for kw in keywords {
        let tokens: Vec<String> = kw
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(|t| format!("\"{t}\""))
            .collect();
        if tokens.is_empty() {
            continue;
        }
        let fts_query = tokens.join(" AND ");
        if let Ok(mut stmt) = conn.prepare(
            "SELECT DISTINCT n.file_path \
             FROM nodes_fts f \
             JOIN nodes n ON n.id = f.rowid \
             WHERE nodes_fts MATCH ?1 \
             LIMIT 20",
        ) {
            if let Ok(rows) = stmt.query_map(params![fts_query], |row| row.get::<_, String>(0)) {
                for row in rows.flatten() {
                    if !seed_files.contains(&row) {
                        seed_files.push(row);
                    }
                }
            }
        }
    }
    if seed_files.is_empty() {
        return String::new();
    }

    crate::core::cooccurrence::record_access(project_root, &seed_files);

    // Adjacency from edges table (source → target file pairs).
    let mut adjacency: HashMap<String, Vec<(String, f64)>> = HashMap::new();
    let mut add_edge = |a: &str, b: &str, w: f64| {
        adjacency
            .entry(a.to_string())
            .or_default()
            .push((b.to_string(), w));
        adjacency
            .entry(b.to_string())
            .or_default()
            .push((a.to_string(), w));
    };
    if let Ok(mut stmt) = conn.prepare(
        "SELECT n1.file_path, n2.file_path \
         FROM edges e \
         JOIN nodes n1 ON n1.id = e.source_id \
         JOIN nodes n2 ON n2.id = e.target_id",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            for row in rows.flatten() {
                add_edge(&row.0, &row.1, 1.0);
            }
        }
    }

    let coaccess = crate::core::cooccurrence::load(project_root);
    for sf in &seed_files {
        for (nbr, w) in coaccess.related(sf, 16) {
            add_edge(sf, &nbr, w);
        }
    }

    let seeds: HashMap<String, f64> = seed_files.iter().map(|f| (f.clone(), 1.0)).collect();
    let ranked = crate::core::spreading_activation::related_ranked(
        &seeds,
        &adjacency,
        SPREAD_DECAY,
        SPREAD_HOPS,
        SPREAD_TOP_K,
    );
    if ranked.is_empty() {
        return String::new();
    }

    let mut s = String::from("\n## Related (associative: import/call graph + learned co-access)\n");
    for (file, activation) in ranked {
        let file = crate::core::protocol::display_path(&file);
        s.push_str(&format!("- {file} (activation {activation:.2})\n"));
    }
    s
}

/// Run [`build_associative_block`] under [`graph_budget`]. The Hebbian record is
/// a side effect of the worker, so it persists even when we time out and drop
/// the (optional) section.
fn associative_block_budgeted(project_root: &str, keywords: &[String]) -> String {
    if keywords.is_empty() {
        return String::new();
    }
    let (tx, rx) = mpsc::channel::<String>();
    let root = project_root.to_string();
    let kws = keywords.to_vec();
    std::thread::spawn(move || {
        let block = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build_associative_block(&root, &kws)
        }))
        .unwrap_or_else(|_| {
            tracing::warn!("[ctx_compose: associative block panicked; omitting section]");
            String::new()
        });
        let _ = tx.send(block);
    });
    rx.recv_timeout(graph_budget()).unwrap_or_default()
}

/// Words that carry no retrieval signal — dropped from keyword extraction.
const STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "that",
    "this",
    "from",
    "into",
    "how",
    "where",
    "what",
    "does",
    "are",
    "was",
    "use",
    "used",
    "uses",
    "add",
    "all",
    "any",
    "can",
    "get",
    "set",
    "via",
    "out",
    "its",
    "his",
    "her",
    "you",
    "your",
    "our",
    "find",
    "show",
    "list",
    "make",
    "when",
    "then",
    "has",
    "have",
    "had",
    "not",
    "but",
    "see",
    "function",
    "method",
    "class",
    "code",
    "file",
    "files",
    "implement",
    "implementation",
];

/// Extract up to `max` distinct identifier-ish keywords from a task, preserving
/// original case (symbol lookups are case-sensitive) and first-seen order.
fn extract_keywords(task: &str, max: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in task.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if raw.len() < 3 {
            continue;
        }
        if STOPWORDS.contains(&raw.to_ascii_lowercase().as_str()) {
            continue;
        }
        if seen.insert(raw.to_string()) {
            out.push(raw.to_string());
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

/// Build the index synchronously the first time `ctx_compose` is called for a
/// project. One-time ~12s on a cold repo; incremental builds thereafter.
fn ensure_index_sync(project_root: &str) {
    let root = Path::new(project_root);
    let db_path = crate::core::index_namespace::vectors_dir(root).join("code_index.db");
    if db_path.exists() {
        return;
    }
    tracing::info!(
        "[ctx_compose] code_index.db not found — building index for {project_root} \
         (one-time ~12s; incremental after)"
    );
    let mode = IndexingMode::effective(&Config::load());
    match IndexPipeline::new(root.to_path_buf())
        .with_mode(mode)
        .build()
    {
        Ok(pipeline) => {
            if let Err(e) = pipeline.run() {
                tracing::warn!("[ctx_compose] index build failed: {e}");
            }
        }
        Err(e) => {
            tracing::warn!("[ctx_compose] index pipeline setup failed: {e}");
        }
    }
}

/// Compose a single rich response for `task`.
#[must_use]
#[allow(unused_variables)]
pub fn handle(task: &str, project_root: &str, crp_mode: CrpMode) -> (String, usize) {
    let task = task.trim();
    if task.is_empty() {
        return ("ERROR: task is required".to_string(), 0);
    }

    let keywords = extract_keywords(task, 6);

    let mut out = String::new();
    out.push_str(&format!("TASK: {task}\n"));
    if keywords.is_empty() {
        out.push_str("KEYWORDS: (none extracted — using full task for ranking)\n");
    } else {
        out.push_str(&format!("KEYWORDS: {}\n", keywords.join(", ")));
    }

    // Ensure index exists — build synchronously if missing (one-time ~12s cost).
    // After this, all searches hit a warm index.
    ensure_index_sync(project_root);

    // 1. Hybrid search on the full task — BM25 + dense vectors.
    out.push_str("\n## Ranked files\n");
    if let Ok(results) =
        crate::tools::ctx_semantic_search::search_hits(task, project_root, 20, "hybrid", None, None)
    {
        for r in &results {
            let score = format!("{:.1}", r.rrf_score);
            out.push_str(&format!(
                "  {}:{}  {}  ({})\n",
                r.file_path, r.start_line, r.symbol_name, score
            ));
            // First line of snippet for context
            if let Some(first_line) = r.snippet.lines().next() {
                let trimmed = first_line.trim();
                if !trimmed.is_empty() {
                    out.push_str(&format!("    {trimmed}\n"));
                }
            }
        }
    }
    out.push('\n');

    // 2. Associative neighbours via spreading activation over the import/call
    //    graph unified with the learned Hebbian co-access graph (budgeted,
    //    additive — surfaces structurally-close files lexical search misses).
    out.push_str(&associative_block_budgeted(project_root, &keywords));

    let sent = count_tokens(&out);
    (out, sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_keywords_drops_stopwords_and_short_tokens() {
        let kw = extract_keywords("How does the BM25Index cache work for ctx_search?", 6);
        assert!(kw.contains(&"BM25Index".to_string()));
        assert!(kw.contains(&"cache".to_string()));
        assert!(kw.contains(&"ctx_search".to_string()));
        assert!(!kw.iter().any(|k| k == "the" || k == "How" || k == "for"));
    }

    #[test]
    fn extract_keywords_dedups_and_caps() {
        let kw = extract_keywords("alpha alpha beta gamma delta epsilon zeta eta", 3);
        assert_eq!(kw.len(), 3);
        assert_eq!(kw[0], "alpha");
    }

    #[test]
    fn empty_task_is_rejected() {
        let (out, tok) = handle("   ", "/tmp", CrpMode::Off);
        assert!(out.starts_with("ERROR"));
        assert_eq!(tok, 0);
    }
}
