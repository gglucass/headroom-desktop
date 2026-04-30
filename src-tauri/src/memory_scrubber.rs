//! Strips noisy `traffic_learner` error_recovery patterns from MEMORY.md files
//! and the SQLite memory store.
//!
//! Background: the upstream traffic_learner emits "error recovery" patterns
//! from any failed-then-succeeded tool pair within 5 history entries, with no
//! semantic check that the calls are related. This produces contradictory and
//! one-shot rules that bloat MEMORY.md and conversation context every turn.
//! Until upstream tightens the matcher and the fix lands in a release, the
//! desktop scrubs the bad output on launch.
//!
//! Other learned categories (environment / architecture / preference) are
//! left alone — they are net-positive in practice. The scrub is idempotent,
//! safe to run on every launch, and a no-op when nothing matches.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

const ERROR_RECOVERY_HEADING: &str = "### Learned: error recovery";

/// Scrub all known places where `traffic_learner` error_recovery patterns
/// land. Logs progress via `log::*`; never panics. Errors from any individual
/// file or DB are logged and do not abort the rest of the scrub.
pub fn scrub_all(memory_db_path: &Path) {
    let projects_root = dirs::home_dir().map(|h| h.join(".claude").join("projects"));
    scrub_all_in(projects_root.as_deref(), memory_db_path);
}

/// Same as [`scrub_all`] but with an explicit Claude projects root; for tests.
fn scrub_all_in(projects_root: Option<&Path>, memory_db_path: &Path) {
    let md_files = projects_root
        .map(discover_memory_md_files_in)
        .unwrap_or_default();
    log::info!(
        "memory_scrubber: discovered {} MEMORY.md candidates",
        md_files.len()
    );
    let mut md_total_removed = 0usize;
    for path in &md_files {
        match scrub_memory_md_file(path) {
            Ok(0) => {}
            Ok(removed) => {
                md_total_removed += removed;
                log::info!(
                    "memory_scrubber: stripped {removed} error_recovery line(s) from {}",
                    path.display()
                );
            }
            Err(e) => log::warn!(
                "memory_scrubber: failed to scrub {}: {e}",
                path.display()
            ),
        }
    }

    match scrub_memory_db(memory_db_path) {
        Ok(0) => {}
        Ok(n) => log::info!(
            "memory_scrubber: deleted {n} error_recovery row(s) from {}",
            memory_db_path.display()
        ),
        Err(e) => log::warn!(
            "memory_scrubber: failed to scrub {}: {e}",
            memory_db_path.display()
        ),
    }

    if md_total_removed > 0 {
        log::info!("memory_scrubber: scrub complete");
    }
}

/// Enumerate `<projects_root>/*/memory/MEMORY.md` paths that exist.
fn discover_memory_md_files_in(projects_root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(projects_root) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let candidate = path.join("memory").join("MEMORY.md");
        if candidate.is_file() {
            out.push(candidate);
        }
    }
    out
}

/// Strip the `### Learned: error recovery` subsection from a MEMORY.md file.
/// Returns the number of lines removed (0 if the section was absent).
fn scrub_memory_md_file(path: &Path) -> std::io::Result<usize> {
    let original = fs::read_to_string(path)?;
    let (cleaned, removed) = strip_error_recovery_subsection(&original);
    if removed == 0 {
        return Ok(0);
    }
    fs::write(path, cleaned)?;
    Ok(removed)
}

/// Remove every block that starts with `### Learned: error recovery` and
/// continues until the next `### `, `## ` or end of file. Returns
/// `(cleaned_text, lines_removed)`.
fn strip_error_recovery_subsection(input: &str) -> (String, usize) {
    let lines: Vec<&str> = input.split_inclusive('\n').collect();
    let mut out = String::with_capacity(input.len());
    let mut removed = 0usize;
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        if line.trim_end_matches(['\n', '\r']) == ERROR_RECOVERY_HEADING {
            // Consume this heading and everything until the next heading
            // (### or ##) or EOF.
            i += 1;
            removed += 1;
            while i < lines.len() {
                let trimmed = lines[i].trim_start();
                if trimmed.starts_with("### ") || trimmed.starts_with("## ") {
                    break;
                }
                i += 1;
                removed += 1;
            }
            continue;
        }
        out.push_str(line);
        i += 1;
    }
    (out, removed)
}

/// Delete `traffic_learner`-sourced error_recovery rows from the memory DB.
/// Returns the number of rows deleted.
fn scrub_memory_db(path: &Path) -> rusqlite::Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let conn = Connection::open(path)?;
    // Filter on JSON metadata: the top-level `category` column is empty for
    // traffic_learner rows; classification lives in metadata.category.
    let n = conn.execute(
        "DELETE FROM memories \
         WHERE json_extract(metadata, '$.source') = 'traffic_learner' \
           AND json_extract(metadata, '$.category') = 'error_recovery'",
        [],
    )?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_removes_error_recovery_subsection_only() {
        let input = "## Headroom Learned Patterns\n\n\
            ### Learned: error recovery\n\
            - bogus rule 1\n\
            - bogus rule 2\n\n\
            ### Subagent Usage\n\
            - keep this\n\n\
            ## Manual memory index\n\
            - keep this too\n";
        let (cleaned, removed) = strip_error_recovery_subsection(input);
        assert!(!cleaned.contains("### Learned: error recovery"));
        assert!(!cleaned.contains("bogus rule"));
        assert!(cleaned.contains("### Subagent Usage"));
        assert!(cleaned.contains("- keep this"));
        assert!(cleaned.contains("## Manual memory index"));
        assert_eq!(removed, 4); // heading + 2 bullets + 1 blank line
    }

    #[test]
    fn strip_is_noop_when_section_absent() {
        let input = "## Other\n- nothing to scrub\n";
        let (cleaned, removed) = strip_error_recovery_subsection(input);
        assert_eq!(cleaned, input);
        assert_eq!(removed, 0);
    }

    #[test]
    fn strip_handles_section_at_eof() {
        let input = "## Top\n\n### Learned: error recovery\n- rule\n- rule2\n";
        let (cleaned, removed) = strip_error_recovery_subsection(input);
        assert_eq!(cleaned, "## Top\n\n");
        assert_eq!(removed, 3);
    }

    #[test]
    fn strip_removes_multiple_occurrences() {
        let input = "### Learned: error recovery\n- a\n### Keep\n- b\n### Learned: error recovery\n- c\n## End\n- d\n";
        let (cleaned, removed) = strip_error_recovery_subsection(input);
        assert!(!cleaned.contains("Learned: error recovery"));
        assert!(cleaned.contains("### Keep"));
        assert!(cleaned.contains("## End"));
        assert!(cleaned.contains("- b"));
        assert!(cleaned.contains("- d"));
        assert!(!cleaned.contains("- a"));
        assert!(!cleaned.contains("- c"));
        assert_eq!(removed, 4); // 2 headings + 2 bullets
    }

    #[test]
    fn strip_does_not_match_partial_heading() {
        // Only an exact `### Learned: error recovery` line triggers stripping.
        let input = "### Learned: error recovery (manual)\n- keep this\n";
        let (cleaned, removed) = strip_error_recovery_subsection(input);
        assert_eq!(cleaned, input);
        assert_eq!(removed, 0);
    }

    #[test]
    fn scrub_db_is_noop_on_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist.db");
        assert_eq!(scrub_memory_db(&missing).unwrap(), 0);
    }

    #[test]
    fn scrub_db_deletes_only_traffic_learner_error_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE memories (\
                id TEXT PRIMARY KEY, content TEXT, user_id TEXT, \
                created_at TEXT, valid_from TEXT, category TEXT, \
                importance REAL, metadata TEXT)",
            [],
        )
        .unwrap();
        let rows = [
            ("a", r#"{"source":"traffic_learner","category":"error_recovery"}"#),
            ("b", r#"{"source":"traffic_learner","category":"environment"}"#),
            ("c", r#"{"source":"traffic_learner","category":"preference"}"#),
            ("d", r#"{"source":"manual","category":"error_recovery"}"#),
            ("e", r#"{"source":"traffic_learner","category":"error_recovery"}"#),
        ];
        for (id, meta) in rows {
            conn.execute(
                "INSERT INTO memories (id, content, user_id, created_at, valid_from, category, importance, metadata) \
                 VALUES (?, '', 'u', '', '', '', 0.5, ?)",
                [id, meta],
            )
            .unwrap();
        }
        drop(conn);

        let deleted = scrub_memory_db(&db_path).unwrap();
        assert_eq!(deleted, 2);

        let conn = Connection::open(&db_path).unwrap();
        let remaining: Vec<String> = conn
            .prepare("SELECT id FROM memories ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(remaining, vec!["b", "c", "d"]);
    }

    #[test]
    fn discover_walks_projects_root() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path();
        // Two projects with MEMORY.md, one without, one with a non-dir entry.
        let p1 = projects.join("proj1").join("memory");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("MEMORY.md"), "# p1\n").unwrap();
        let p2 = projects.join("proj2").join("memory");
        fs::create_dir_all(&p2).unwrap();
        fs::write(p2.join("MEMORY.md"), "# p2\n").unwrap();
        fs::create_dir_all(projects.join("proj3")).unwrap(); // no memory/ subdir
        fs::write(projects.join("loose-file.txt"), "ignore me").unwrap();

        let mut found = discover_memory_md_files_in(projects);
        found.sort();
        assert_eq!(
            found,
            vec![p1.join("MEMORY.md"), p2.join("MEMORY.md")]
        );
    }

    #[test]
    fn discover_returns_empty_on_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(discover_memory_md_files_in(&missing).is_empty());
    }

    #[test]
    fn scrub_all_in_strips_md_and_db_together() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");

        // Two project dirs, one with the noisy section and one without.
        let dirty = projects.join("dirty").join("memory");
        fs::create_dir_all(&dirty).unwrap();
        fs::write(
            dirty.join("MEMORY.md"),
            "## Top\n\n### Learned: error recovery\n- noise\n\n### Keep\n- signal\n",
        )
        .unwrap();
        let clean = projects.join("clean").join("memory");
        fs::create_dir_all(&clean).unwrap();
        let clean_md_before = "## Manual\n- nothing to scrub\n";
        fs::write(clean.join("MEMORY.md"), clean_md_before).unwrap();

        // DB with two error_recovery rows and one keeper.
        let db_path = tmp.path().join("memory.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE memories (\
                id TEXT PRIMARY KEY, content TEXT, user_id TEXT, \
                created_at TEXT, valid_from TEXT, category TEXT, \
                importance REAL, metadata TEXT)",
            [],
        )
        .unwrap();
        for (id, meta) in [
            ("a", r#"{"source":"traffic_learner","category":"error_recovery"}"#),
            ("b", r#"{"source":"traffic_learner","category":"error_recovery"}"#),
            ("keep", r#"{"source":"traffic_learner","category":"environment"}"#),
        ] {
            conn.execute(
                "INSERT INTO memories (id, content, user_id, created_at, valid_from, category, importance, metadata) \
                 VALUES (?, '', 'u', '', '', '', 0.5, ?)",
                [id, meta],
            )
            .unwrap();
        }
        drop(conn);

        scrub_all_in(Some(&projects), &db_path);

        // Dirty MEMORY.md cleaned; clean MEMORY.md untouched.
        let dirty_after = fs::read_to_string(dirty.join("MEMORY.md")).unwrap();
        assert!(!dirty_after.contains("Learned: error recovery"));
        assert!(dirty_after.contains("### Keep"));
        let clean_after = fs::read_to_string(clean.join("MEMORY.md")).unwrap();
        assert_eq!(clean_after, clean_md_before);

        // DB error_recovery rows gone, keeper remains.
        let conn = Connection::open(&db_path).unwrap();
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM memories ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(ids, vec!["keep"]);
    }

    #[test]
    fn scrub_all_in_tolerates_missing_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let missing_projects = tmp.path().join("nope");
        let missing_db = tmp.path().join("absent.db");
        // Should not panic.
        scrub_all_in(Some(&missing_projects), &missing_db);
        scrub_all_in(None, &missing_db);
    }

    #[test]
    fn scrub_md_file_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("MEMORY.md");
        fs::write(
            &path,
            "## Top\n\n### Learned: error recovery\n- noise\n\n### Keep\n- signal\n",
        )
        .unwrap();
        let removed = scrub_memory_md_file(&path).unwrap();
        assert!(removed > 0);
        let after = fs::read_to_string(&path).unwrap();
        assert!(!after.contains("Learned: error recovery"));
        assert!(after.contains("### Keep"));
        assert!(after.contains("- signal"));

        // Idempotent on second run.
        assert_eq!(scrub_memory_md_file(&path).unwrap(), 0);
    }
}
