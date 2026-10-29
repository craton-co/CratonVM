// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! One policy for the VM's exit-time narration (`[cratonvm] ...` census lines).
//!
//! HotSpot 25 prints nothing on stderr when a program ends, so neither does a
//! plain CratonVM run, in either mode (`--jdk-only` / `--compatible`). Every
//! exit census used to carry its own argument for being unconditional ("a
//! silence reads as clean"), and that argument and byte parity with HotSpot
//! cannot both hold on a default run (proposal
//! `docs/feature-designs/jit-ir-optimizations.md` item 1; the
//! audit that gated each site is
//! `docs/feature-designs/jdk-only-mode.md`).
//!
//! [`exit_report!`](crate::exit_report!) is that policy in one place:
//!
//! * a line prints only under `CRATONVM_DBG=exit-census`
//!   (`CRATONVM_DBG_EXIT_CENSUS=1`), or under the site's own switch
//!   (`exit_report!(or <expr>; ...)`), or when a sink file is named;
//! * `CRATONVM_EXIT_REPORT_FILE=<path>` appends the lines to that file instead
//!   of stderr (and turns the report on), so a battery can collect the census
//!   while diffing stderr against HotSpot byte for byte. Both spellings are
//!   supported and silent: the key itself, or `CRATONVM_DBG=exit-report-file=<path>`
//!   (a valued key, `flag_groups::VALUED_KEYS`; the bare token is refused rather
//!   than writing a file called `1`).
//!
//! The ratchet `vm-cli/tests/exit_report_ratchet.rs` freezes the number of
//! direct `eprintln!` sites that print a `[cratonvm]` line, per file; a new one
//! fails it and is either routed through this macro or added to the baseline
//! with its reason.
//!
//! Nothing here is process state: both switches are read per call (the exit
//! path is cold), and the sink is opened per line.

use std::io::Write as _;

/// The switch that names the sink file.
pub const EXIT_REPORT_FILE_KEY: &str = "CRATONVM_EXIT_REPORT_FILE";

/// Is the exit report asked for: `CRATONVM_DBG=exit-census` or a sink file?
pub fn exit_report_on() -> bool {
    crate::flags::runtime_flag_on("CRATONVM_DBG_EXIT_CENSUS") || exit_report_file().is_some()
}

/// The sink named by `CRATONVM_EXIT_REPORT_FILE`, when set and non-empty.
pub fn exit_report_file() -> Option<std::path::PathBuf> {
    crate::flags::runtime_var_os(EXIT_REPORT_FILE_KEY)
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

/// Write one exit-report line: to the sink file when one is named (stderr if
/// it cannot be opened, so a typo does not swallow the census), else stderr.
/// Callers go through [`exit_report!`](crate::exit_report!), which decides
/// WHETHER; this decides WHERE.
pub fn emit(args: std::fmt::Arguments<'_>) {
    let line = format!("{args}");
    emit_line_to(exit_report_file().as_deref(), &line);
}

/// [`emit`] with the sink given, for tests. One `write_all` per line, so lines
/// from two threads do not interleave inside a line.
pub fn emit_line_to(sink: Option<&std::path::Path>, line: &str) {
    if let Some(path) = sink {
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path);
        if let Ok(mut f) = opened {
            let mut buf = String::with_capacity(line.len() + 1);
            buf.push_str(line);
            buf.push('\n');
            if f.write_all(buf.as_bytes()).is_ok() {
                return;
            }
        }
    }
    eprintln!("{line}");
}

/// Print an exit-time narration line under the one exit-report policy
/// ([`crate::exit_report`]): nothing on a plain run, the line under
/// `CRATONVM_DBG=exit-census` or `CRATONVM_EXIT_REPORT_FILE`.
///
/// ```ignore
/// cratonvm_types::exit_report!("[cratonvm] statics scan: passes={passes}");
/// // also under the site's own switch:
/// cratonvm_types::exit_report!(or own_switch_on(); "[cratonvm] dropped: {n}");
/// ```
#[macro_export]
macro_rules! exit_report {
    (or $extra:expr; $($arg:tt)+) => {{
        if ($extra) || $crate::exit_report::exit_report_on() {
            $crate::exit_report::emit(::core::format_args!($($arg)+));
        }
    }};
    ($($arg:tt)+) => {{
        if $crate::exit_report::exit_report_on() {
            $crate::exit_report::emit(::core::format_args!($($arg)+));
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sink_line_is_appended_whole() {
        let dir = std::env::temp_dir().join(format!(
            "cratonvm-exit-report-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("report.txt");
        let _ = std::fs::remove_file(&path);
        emit_line_to(Some(&path), "[cratonvm] one");
        emit_line_to(Some(&path), "[cratonvm] two");
        let text = std::fs::read_to_string(&path).expect("sink written");
        assert_eq!(text, "[cratonvm] one\n[cratonvm] two\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_macro_spellings_expand() {
        // Both arms compile and evaluate their condition. The `or` arm's own
        // switch is evaluated even when the report is off.
        let mut asked = 0;
        crate::exit_report!(or { asked += 1; false }; "[cratonvm] x {}", 1);
        assert_eq!(asked, 1);
        crate::exit_report!("[cratonvm] y {}", 2);
    }
}
