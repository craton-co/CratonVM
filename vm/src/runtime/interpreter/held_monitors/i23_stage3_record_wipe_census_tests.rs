use std::path::{Path, PathBuf};

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The lines of `text` outside `#[cfg(test)] mod ... { ... }` blocks
/// written at column 0 (rustfmt's layout: the block ends at the first line
/// that is exactly `}`), comment lines dropped.
fn production_lines(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line == "#[cfg(test)]"
            && lines.get(i + 1).is_some_and(|next| {
                next.starts_with("mod ")
                    || next.starts_with("pub mod ")
                    || next.starts_with("pub(crate) mod ")
            })
        {
            while i < lines.len() && lines[i] != "}" {
                i += 1;
            }
            i += 1;
            continue;
        }
        if !line.trim_start().starts_with("//") {
            out.push(line);
        }
        i += 1;
    }
    out
}

/// `(raw wipes, entry-only rewrites)` of a `.held_monitors` record in the
/// production code of `file` (a path below the vm crate's `src`).
fn wipes_in(file: &str) -> (usize, usize) {
    let path = format!("{}/src/{file}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).expect("a vm source file");
    let lines = production_lines(&text);
    let count = |needle: &str| lines.iter().filter(|l| l.contains(needle)).count();
    (
        count(".held_monitors.clear()"),
        count(".held_monitors.clear_entries()"),
    )
}

#[test]
fn i23_only_frame_reuse_wipes_a_record_raw_and_only_the_osr_transfers_rewrite_entries() {
    // Frame reuse (`reset_cached_tail`) is the one raw wipe: a recycled
    // frame is a new activation, not a resumed one.
    assert_eq!(wipes_in("runtime/frame.rs"), (1, 0));
    // The two OSR in-place transfers rewrite the entries but keep the
    // method-monitor-exited bit and re-mark the record themselves
    // (`note_seeded_record`), each in the arm that names at least one lock;
    // the arm that names none goes through `clear_untrusted_record`.
    assert_eq!(
        wipes_in("runtime/interpreter/deopt_resume/static_handed_hold_is_the_mirror.rs"),
        (0, 2)
    );

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    // A walk that found almost nothing is a broken scan, not a clean one.
    assert!(
        files.len() > 100,
        "only {} sources under {}",
        files.len(),
        src.display()
    );
    let mut stray = Vec::new();
    for path in files {
        let rel = path
            .strip_prefix(&src)
            .expect("below src")
            .to_string_lossy()
            .replace('\\', "/");
        if rel == "runtime/interpreter/held_monitors/i23_stage3_record_wipe_census_tests.rs"
            || rel == "runtime/frame.rs"
            || rel == "runtime/interpreter/deopt_resume/static_handed_hold_is_the_mirror.rs"
        {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("a readable source");
        for line in production_lines(&text) {
            if line.contains(".held_monitors.clear()")
                || line.contains(".held_monitors.clear_entries()")
                || line.contains("held_monitors = ")
            {
                stray.push(format!("{rel}: {}", line.trim()));
            }
        }
    }
    assert!(
        stray.is_empty(),
        "a production path rewrites a frame's held-monitor record without a marking door \
         (clear_untrusted_record / note_trusted_record): {stray:?}"
    );
}
