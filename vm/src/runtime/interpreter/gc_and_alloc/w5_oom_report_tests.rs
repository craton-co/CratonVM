use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use std::sync::atomic::Ordering;

/// gen r4w5/thrash5: `Arguments::copy_expand_pid`.
#[test]
fn on_error_commands_expand_the_pid_like_hotspot() {
    assert_eq!(
        expand_on_error_pid("echo oom-hook %p", 4242),
        "echo oom-hook 4242"
    );
    assert_eq!(expand_on_error_pid("kill -9 %p; %%p", 7), "kill -9 7; %p");
    assert_eq!(
        expand_on_error_pid("100%x", 7),
        "100%x",
        "an unknown escape is kept"
    );
    assert_eq!(expand_on_error_pid("trailing %", 7), "trailing %");
}

/// gen r4w5/thrash5: `next_OnError_command`: `;` separates, blanks and
/// empty commands are skipped.
#[test]
fn on_error_commands_split_on_semicolons() {
    assert_eq!(
        on_out_of_memory_error_commands("  echo a %p;; ;echo b", 9),
        vec!["echo a 9".to_string(), "echo b".to_string()]
    );
    assert!(on_out_of_memory_error_commands(" ; ;", 9).is_empty());
    assert_eq!(
        on_out_of_memory_error_commands("echo one\necho two", 9),
        vec!["echo one\necho two".to_string()],
        "a newline (two accumulated options) is left for the shell"
    );
}

/// gen r4w5/thrash5: with none of the four report options configured, the
/// report is a no-op and does not even take the latch — the byte-for-byte
/// guarantee for every default configuration, `--compatible` included.
#[test]
fn an_unconfigured_report_does_nothing_and_takes_no_latch() {
    let shared = crate::vm::SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let mut thread = JvmThread::new(ThreadId(0), "w5-oom-report");
    assert!(!shared.config.exit_on_out_of_memory_error);
    assert!(!shared.config.crash_on_out_of_memory_error);
    assert!(shared.config.on_out_of_memory_error.is_none());
    maybe_dump_heap_on_oom_for(&shared, &mut thread, "Java heap space");
    assert!(!shared.debug.oom_dump_written.load(Ordering::SeqCst));
}

/// gen r4w5/thrash5: the command runs once per VM. The first report takes
/// the latch and runs it; the second finds the latch taken. The command
/// is a harmless `exit 0` so the test needs no shell output.
#[test]
fn the_on_oom_command_runs_once_per_vm() {
    let shared = crate::vm::SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        on_out_of_memory_error: Some("exit 0".to_string()),
        ..VmConfig::default()
    });
    let mut thread = JvmThread::new(ThreadId(0), "w5-oom-once");
    maybe_dump_heap_on_oom_for(&shared, &mut thread, "Java heap space");
    assert!(
        shared.debug.oom_dump_written.load(Ordering::SeqCst),
        "first report latches"
    );
    // A second report must return at the latch (nothing observable to
    // assert beyond not hanging or exiting; the latch stays set).
    maybe_dump_heap_on_oom_for(&shared, &mut thread, "Java heap space");
    assert!(shared.debug.oom_dump_written.load(Ordering::SeqCst));
    // A second VM in the same process has its own latch.
    let other = crate::vm::SharedVm::new(VmConfig::default());
    assert!(!other.debug.oom_dump_written.load(Ordering::SeqCst));
}
