use super::*;

/// The readability probe must reject a non-pointer.
///
/// This is the regression for the defect that truncated every hardware-fault
/// report whose registers held a non-pointer: the probe used to write to
/// /dev/null, whose driver returns the byte count without ever reading the
/// user buffer, so it called address `0x1` readable and the handler then
/// dereferenced it and died. A pipe copies for real and returns EFAULT.
#[cfg(unix)]
#[test]
fn probe_readable_rejects_a_non_pointer_through_a_pipe() {
    let mut fds = [-1i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    unsafe { libc::fcntl(fds[1], libc::F_SETFL, libc::O_NONBLOCK) };

    let live = [0u64; 8];
    assert!(
        probe_readable(fds[1], live.as_ptr() as u64, 64),
        "a live stack buffer must probe readable"
    );
    assert!(
        !probe_readable(fds[1], 1, 64),
        "address 0x1 must NOT probe readable - this is the whole point"
    );
    assert!(!probe_readable(fds[1], 0, 8), "null is never readable");
    assert!(
        !probe_readable(-1, live.as_ptr() as u64, 8),
        "no fd, no verdict"
    );

    unsafe {
        libc::close(fds[0]);
        libc::close(fds[1]);
    }
}

/// /dev/null is what the probe MUST NOT use. Pinning the reason down in a
/// test keeps someone from "simplifying" the pipe back into an open of
/// /dev/null: this asserts the broken behaviour is real.
#[cfg(target_os = "linux")]
#[test]
fn dev_null_reports_even_a_bad_address_as_readable() {
    let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY) };
    assert!(fd >= 0);
    assert!(
        probe_readable(fd, 1, 64),
        "if this ever fails, /dev/null started faulting on bad buffers and \
         the pipe is no longer required"
    );
    unsafe { libc::close(fd) };
}

#[cfg(target_os = "linux")]
#[test]
fn maps_range_parses_a_real_line() {
    let (start, end) = parse_maps_range(b"709281c12000-709281c14000 r-xp 00000000 00:00 0 ");
    assert_eq!(start, 0x709281c12000);
    assert_eq!(end, 0x709281c14000);
}

#[cfg(target_os = "linux")]
#[test]
fn maps_range_rejects_non_range_lines() {
    assert_eq!(parse_maps_range(b""), (0, 0));
    assert_eq!(parse_maps_range(b"not a maps line"), (0, 0));
    // A start with no end is not a usable range.
    assert_eq!(parse_maps_range(b"7f0000000000-"), (0, 0));
}

fn sample_crash_info() -> CrashInfo {
    CrashInfo {
        signal: 11,
        signal_name: "SIGSEGV".to_string(),
        pid: 12345,
        tid: 67890,
        timestamp: SystemTime::UNIX_EPOCH,
        thread_name: Some("main".to_string()),
        panic_message: Some("null pointer dereference".to_string()),
        panic_location: Some("vm/src/runtime/interpreter.rs:42".to_string()),
    }
}

#[test]
fn crash_info_creation() {
    let info = sample_crash_info();
    assert_eq!(info.signal, 11);
    assert_eq!(info.signal_name, "SIGSEGV");
    assert_eq!(info.pid, 12345);
    assert_eq!(info.tid, 67890);
    assert_eq!(info.thread_name.as_deref(), Some("main"));
    assert_eq!(
        info.panic_message.as_deref(),
        Some("null pointer dereference")
    );
    assert_eq!(
        info.panic_location.as_deref(),
        Some("vm/src/runtime/interpreter.rs:42")
    );
}

#[test]
fn crash_info_from_panic_fields() {
    // We cannot easily construct a PanicHookInfo, so test the
    // manual-construction path instead.
    let info = CrashInfo {
        signal: 0,
        signal_name: "RUST_PANIC".to_string(),
        pid: get_pid(),
        tid: get_tid(),
        timestamp: SystemTime::now(),
        thread_name: Some("test-thread".to_string()),
        panic_message: Some("assertion failed".to_string()),
        panic_location: Some("test.rs:1".to_string()),
    };
    assert_eq!(info.signal, 0);
    assert_eq!(info.signal_name, "RUST_PANIC");
    assert!(info.pid > 0);
    assert!(info.panic_message.is_some());
}

#[test]
fn crash_report_contains_header() {
    let info = sample_crash_info();
    let report = generate_crash_report(&info);
    assert!(
        report.contains("A fatal error has been detected by the CratonVM Runtime Environment")
    );
    assert!(report.contains("SIGSEGV"));
    assert!(report.contains("pid=12345"));
    assert!(report.contains("tid=67890"));
}

#[test]
fn crash_report_contains_thread_section() {
    let info = sample_crash_info();
    let report = generate_crash_report(&info);
    assert!(report.contains("T H R E A D"));
    assert!(report.contains("\"main\""));
    assert!(report.contains("null pointer dereference"));
}

#[test]
fn crash_report_contains_process_section() {
    let info = sample_crash_info();
    let report = generate_crash_report(&info);
    assert!(report.contains("P R O C E S S"));
    assert!(report.contains("VM state:"));
    assert!(report.contains("Heap:"));
}

#[test]
fn crash_report_contains_system_section() {
    let info = sample_crash_info();
    let report = generate_crash_report(&info);
    assert!(report.contains("S Y S T E M"));
    assert!(report.contains("OS:"));
    assert!(report.contains("CPU:"));
    assert!(report.contains("Memory:"));
}

#[test]
fn crash_report_contains_jre_version() {
    let info = sample_crash_info();
    let report = generate_crash_report(&info);
    assert!(report.contains("JRE version: CratonVM 25.0"));
}

#[test]
fn crash_report_rust_panic_format() {
    let info = CrashInfo {
        signal: 0,
        signal_name: "RUST_PANIC".to_string(),
        pid: 999,
        tid: 111,
        timestamp: SystemTime::now(),
        thread_name: Some("worker-1".to_string()),
        panic_message: Some("index out of bounds".to_string()),
        panic_location: Some("src/lib.rs:100".to_string()),
    };
    let report = generate_crash_report(&info);
    assert!(report.contains("RUST_PANIC"));
    assert!(report.contains("src/lib.rs:100"));
    assert!(report.contains("index out of bounds"));
    assert!(report.contains("\"worker-1\""));
}

#[test]
fn write_crash_report_to_file() {
    let info = sample_crash_info();
    let dir = std::env::temp_dir();
    let path = dir.join("hs_err_pid_test.log");

    write_crash_report(&info, &path).expect("write should succeed");

    let content = std::fs::read_to_string(&path).expect("read back");
    assert!(content.contains("SIGSEGV"));
    assert!(content.contains("T H R E A D"));
    assert!(content.contains("S Y S T E M"));

    // Cleanup.
    let _ = std::fs::remove_file(&path);
}

#[test]
fn os_info_non_empty() {
    let info = get_os_info();
    assert!(!info.is_empty(), "OS info should not be empty");
    assert!(info.len() > 3, "OS info should have meaningful content");
}

#[test]
fn cpu_info_non_empty() {
    let info = get_cpu_info();
    assert!(!info.is_empty(), "CPU info should not be empty");
    assert!(
        info.contains("core") || info.contains("thread") || info.contains("unknown"),
        "CPU info should mention cores/threads: {}",
        info
    );
}

#[test]
fn memory_info_non_empty() {
    let info = get_memory_info();
    assert!(!info.is_empty(), "Memory info should not be empty");
}

#[test]
fn vm_state_non_empty() {
    let state = get_vm_state();
    assert!(!state.is_empty());
    assert!(state.contains("CratonVM"));
}

#[test]
fn timestamp_formatting() {
    let t = SystemTime::UNIX_EPOCH;
    let s = format_timestamp(t);
    assert_eq!(s, "1970-01-01T00:00:00Z");
}

#[test]
fn days_to_ymd_epoch() {
    let (y, m, d) = days_to_ymd(0);
    assert_eq!((y, m, d), (1970, 1, 1));
}

#[test]
fn itoa_into_buf_zero() {
    let mut buf = [0u8; 8];
    let n = itoa_into_buf(&mut buf, 0);
    assert_eq!(&buf[..n], b"0");
}

#[test]
fn itoa_into_buf_small() {
    let mut buf = [0u8; 8];
    let n = itoa_into_buf(&mut buf, 1);
    assert_eq!(&buf[..n], b"1");

    let n = itoa_into_buf(&mut buf, 9);
    assert_eq!(&buf[..n], b"9");

    let n = itoa_into_buf(&mut buf, 10);
    assert_eq!(&buf[..n], b"10");

    let n = itoa_into_buf(&mut buf, 42);
    assert_eq!(&buf[..n], b"42");
}

#[test]
fn itoa_into_buf_large() {
    let mut buf = [0u8; 32];
    let n = itoa_into_buf(&mut buf, 1_000_000);
    assert_eq!(&buf[..n], b"1000000");

    let n = itoa_into_buf(&mut buf, u64::MAX);
    assert_eq!(&buf[..n], b"18446744073709551615");
}

#[test]
fn itoa_into_buf_empty_returns_zero() {
    let mut buf = [];
    assert_eq!(itoa_into_buf(&mut buf, 123), 0);
}

#[test]
fn days_to_ymd_known_date() {
    // 2024-01-01 is day 19723 since epoch.
    let (y, m, d) = days_to_ymd(19723);
    assert_eq!((y, m, d), (2024, 1, 1));
}

// The async-signal-safe `write_all` only exists on Unix.
#[cfg(unix)]
#[test]
fn write_all_handles_short_writes_via_pipe() {
    use super::async_signal_safe::write_all;
    use std::io::Read;

    // A pipe's kernel buffer is finite, so writing a payload larger than
    // the buffer forces `write(2)` to return short — exactly the partial
    // write the resume-from-offset loop must handle. A reader thread drains
    // the pipe so the writes can complete.
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() failed");
    let (read_fd, write_fd) = (fds[0], fds[1]);

    // Build a payload comfortably larger than a typical 64 KiB pipe buffer
    // and with a recognizable, position-dependent byte pattern so any
    // dropped/duplicated/reordered chunk is detected.
    let payload: Vec<u8> = (0..(512 * 1024)).map(|i| (i % 251) as u8).collect();

    let reader = {
        let expected_len = payload.len();
        std::thread::spawn(move || {
            let mut f = unsafe {
                <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(read_fd)
            };
            let mut got = Vec::with_capacity(expected_len);
            f.read_to_end(&mut got).expect("read pipe");
            got
        })
    };

    write_all(write_fd, &payload);
    // Close the write end so the reader sees EOF.
    unsafe { libc::close(write_fd) };

    let got = reader.join().expect("reader thread");
    assert_eq!(got.len(), payload.len(), "byte count mismatch");
    assert_eq!(got, payload, "payload corrupted across short writes");
}

// Writing to a closed/invalid fd must return promptly (real error, not
// EINTR) rather than spinning forever.
#[cfg(unix)]
#[test]
fn write_all_on_bad_fd_returns() {
    use super::async_signal_safe::write_all;
    // -1 is never a valid fd; `write` returns EBADF, which is not EINTR,
    // so the loop must give up immediately.
    write_all(-1, b"this should not hang");
}

// ── VM diagnostic snapshot ─────────────────────────────────────────────
//
// The publication cells are process-global `OnceLock`s / atomics, so these
// tests are written to be order-independent: they assert invariants that
// hold whether or not some earlier test in the same binary already
// published a mode. `publish_and_report_are_consistent` does the one
// publish and then pins every consumer against it.

#[test]
fn jdk_mode_bytes_is_one_of_three_static_literals() {
    // Must never allocate and never panic, at any publication state — the
    // Unix signal handler calls it.
    let b = jdk_mode_bytes();
    assert!(
        b == b"real-jdk" || b == b"synthetic-jdk" || b == b"<unpublished>",
        "unexpected jdk_mode_bytes(): {:?}",
        String::from_utf8_lossy(b)
    );
}

#[test]
fn jdk_mode_line_never_silently_omits_the_mode() {
    // Either it names a mode, or it says out loud that it could not — the
    // one thing it must never do is render an empty/ambiguous string, which
    // is what the pre-change hardware-fault report effectively did.
    let line = jdk_mode_line();
    assert!(line.starts_with("jdk mode: "), "got {line:?}");
    assert!(line.len() > "jdk mode: ".len(), "got {line:?}");
}

#[test]
fn publish_and_report_are_consistent() {
    // First-write-wins, so whichever test/VM published first owns the
    // value; publishing again must not change it and must not make the
    // byte form disagree with the rich form.
    publish_jdk_mode(crate::config::JdkMode::Synthetic, Some("/nonexistent/jdk"));
    publish_jdk_mode(crate::config::JdkMode::Real, None);

    let mode = active_jdk_mode().expect("a mode is published by now");
    let expected: &[u8] = match mode {
        crate::config::JdkMode::Real => b"real-jdk",
        crate::config::JdkMode::Synthetic => b"synthetic-jdk",
    };
    assert_eq!(
        jdk_mode_bytes(),
        expected,
        "signal-safe byte form disagrees with the OnceLock form"
    );
    assert!(
        jdk_mode_line().contains(mode.as_str()),
        "jdk_mode_line() must name the published mode"
    );
}

#[test]
fn gc_state_lines_report_collector_and_the_moving_verdict() {
    let joined = gc_state_lines_for("generational").join("\n");
    assert!(joined.contains("gc collector: generational"), "{joined}");
    // The policy/actual split is the whole point: a report that only said
    // "generational" would not distinguish a compacting young generation
    // from one permanently diverted to the non-moving sweep.
    assert!(joined.contains("gc young-gen policy:"), "{joined}");
    assert!(joined.contains("gc young-gen actual:"), "{joined}");
    assert!(
        joined.contains("moving cycle(s)") && joined.contains("NON-MOVING sweep"),
        "the actual line must carry both counters: {joined}"
    );
    // And the collector line is not merely echoed back: an unpublished
    // collector must not be reported as a known one.
    assert!(gc_state_lines_for("<unpublished>")
        .join("\n")
        .contains("gc collector: <unpublished>"));
}

/// The counters behind `young-gen policy` / `young-gen actual` are
/// incremented only by `gen_heap.rs`, so under any other collector they are
/// zero by construction. Printing them there reads as a measurement of that
/// collector and is how the Tomcat G1 SIGSEGV page
/// (g1-sigsegv-unguarded-callee-jit-frame-FIXED.md)
/// came to blame a moving young collector that was never running.
#[test]
fn gc_state_lines_omit_generational_only_counters_under_another_collector() {
    for collector in ["g1", "zgc", "<unpublished>"] {
        let joined = gc_state_lines_for(collector).join("\n");
        assert!(
            joined.contains(&format!("gc collector: {collector}")),
            "{joined}"
        );
        assert!(
            !joined.contains("gc young-gen policy:"),
            "{collector}: the generational policy line must not appear: {joined}"
        );
        assert!(
            !joined.contains("gc young-gen actual:"),
            "{collector}: the generational counters must not appear: {joined}"
        );
        assert!(
            !joined.contains("Cheney young copy"),
            "{collector}: must not name the generational young collector: {joined}"
        );
        assert!(
            joined.contains("gc young-gen policy/actual: n/a"),
            "{collector}: their absence must be stated, not silent: {joined}"
        );
    }
}

#[test]
fn jit_state_lines_attribute_a_faulting_pc() {
    // No `fault_pc`: the pc-attribution line must be absent rather than
    // fabricated.
    let without = jit_state_lines(None).join("\n");
    assert!(without.contains("guarded compiled frames live process-wide:"));
    assert!(!without.contains("faulting pc"), "{without}");

    // With a pc that is certainly not in any compiled code range, the
    // report must say so explicitly (or explain that names are off) rather
    // than staying silent.
    let with = jit_state_lines(Some(1)).join("\n");
    assert!(with.contains("faulting pc"), "{with}");
}

/// zt t24/hunt6: h3_P2's own record. `flags=0x0` (never published, never
/// entered), a DATA read at 0xB, and a report that could not say whether a
/// live buffer covered the pc. The reading must call the record a discarded
/// attempt and the pc remapped, never a use-after-free.
#[test]
fn a_data_fault_in_a_freed_discarded_attempt_reads_as_a_remapped_range() {
    for live in [FreedPcLive::Unknown, FreedPcLive::Uncovered] {
        let lines = freed_code_pc_reading(0, live, "read").join("\n");
        assert!(
            lines.contains("NEVER PUBLISHED and never entered"),
            "{lines}"
        );
        assert!(lines.contains("DATA read"), "{lines}");
        assert!(lines.contains("was FETCHED"), "{lines}");
        assert!(
            !lines.contains("use-after-free of executable memory"),
            "{lines}"
        );
    }
}

/// A live buffer covering the pc is reuse whatever the record's flags.
#[test]
fn a_live_buffer_over_a_freed_pc_reads_as_reuse() {
    let all = cratonvm_jit::CODE_FREE_PUBLISHED | cratonvm_jit::CODE_FREE_AUTHORISED;
    for (flags, access) in [(0, "read"), (all, "execute"), (all, "")] {
        let lines =
            freed_code_pc_reading(flags, FreedPcLive::Covered(0x1_0000, 0xA6C0), access)
                .join("\n");
        assert!(lines.contains("REUSED"), "{lines}");
        assert!(lines.contains("base=0x10000 len=0xA6C0"), "{lines}");
    }
}

/// The positive control: an instruction FETCH at a pc no live buffer
/// covers is the genuine stale transfer, and an unqueued published release
/// still names itself a use-after-free.
#[test]
fn a_fetch_from_an_uncovered_freed_pc_reads_as_a_stale_transfer() {
    let lines = freed_code_pc_reading(
        cratonvm_jit::CODE_FREE_PUBLISHED,
        FreedPcLive::Uncovered,
        "execute",
    )
    .join("\n");
    assert!(
        lines.contains("use-after-free of executable memory"),
        "{lines}"
    );
    assert!(
        lines.contains("control transferred into the freed range"),
        "{lines}"
    );
    assert!(!lines.contains("REUSED"), "{lines}");
    assert!(!lines.contains("was FETCHED"), "{lines}");
}

/// zt t25/hunt8: h3_P2's own registers. `[0xB]` decomposes two ways --
/// `rax(4) + 7`, the receiver check's gc_flags byte through a non-null word,
/// and `{rcx,rbx,r12,r13,r14}(0) + 0xB`, a genuine null -- and the report
/// must print both, so the bytes at pc can choose.
#[test]
fn a_null_page_fault_address_is_decomposed_over_the_registers() {
    let regs = [
        ("rax", 0x4),
        ("rcx", 0x0),
        ("rdx", 0x2DF_D954_2908),
        ("rbx", 0x0),
        ("rsp", 0xE9_1E7F_4230),
        ("rbp", 0xE9_1E7F_46A0),
        ("r8", 0x294),
        ("r12", 0x0),
        ("r13", 0x0),
        ("r14", 0x0),
        ("r15", 0x1),
        ("rip", 0x2DF_E381_1F21),
    ];
    let lines = fault_address_register_readings(0xB, &regs);
    assert_eq!(lines.len(), 3, "{lines:?}");
    let all = lines.join("\n");
    // 0xB - 4 = 7 = GC_FLAGS_BYTE_OFFSET (MARK_WORD_OFFSET 4 + 3).
    assert!(
        all.contains("rax(0x4) + 0x7 (+0x7 is the gc_flags byte"),
        "{all}"
    );
    // 0xB - 1 = 0xA: r15 is a candidate too, with no field reading.
    assert!(
        all.contains("r15(0x1) + 0xA: a small NON-NULL base"),
        "{all}"
    );
    assert!(
        all.contains("rcx/rbx/r12/r13/r14(0) + 0xB: a NULL base"),
        "{all}"
    );
    // r8 = 0x294 is above the address, and heap/stack words never qualify.
    assert!(!all.contains("r8"), "{all}");
    assert!(!all.contains("rdx") && !all.contains("rbp"), "{all}");
    // Past the null page there is nothing to decompose.
    assert!(fault_address_register_readings(0x2DF_D954_2068, &regs).is_empty());
}

#[test]
fn java_stack_lines_never_block_and_always_explain_themselves() {
    // Unpublished, empty, or contended — every branch must produce a line
    // that says which case it is. A crash report with a silently missing
    // Java stack is indistinguishable from one with no Java frames.
    let lines = java_stack_lines(8);
    assert!(!lines.is_empty());
    assert!(lines[0].starts_with("Java frames"), "got {:?}", lines[0]);
}

/// CR-VXC-1. Each of these runs inside its own spawned thread: the
/// publication cell is thread-local and cargo's harness reuses threads, so
/// publishing on the harness thread would leak into unrelated tests.
fn published_trace(class: &str, method: &str, line: i32) -> crate::vm::PublishedTraceHandle {
    std::sync::Arc::new(parking_lot::Mutex::new(vec![
        cratonvm_native_api::StackTraceEntry {
            class_name: class.into(),
            method_name: method.into(),
            method_descriptor: None,
            source_file: Some("Probe.java".into()),
            line_number: line,
            byte_code_index: 0,
            class_id: None,
            method_index: None,
        },
    ]))
}

#[test]
fn a_worker_thread_crash_now_renders_that_workers_java_frames() {
    // The whole point of CR-VXC-1: before it, a fault anywhere but the
    // primordial thread rendered the primordial thread's frames (or the
    // "not published" placeholder) and the reader had no way to tell.
    let lines = std::thread::spawn(|| {
        let _guard = crate::vm::PublishedFrameTrace::publish(
            "worker-7",
            7,
            published_trace("com/example/Worker", "run", 42),
        );
        java_stack_lines(8)
    })
    .join()
    .expect("worker thread");

    let joined = lines.join("\n");
    assert!(joined.contains("worker-7"), "{joined}");
    assert!(joined.contains("tid 7"), "{joined}");
    assert!(joined.contains("com/example/Worker.run"), "{joined}");
    assert!(joined.contains("Probe.java:42"), "{joined}");
    assert!(
        !joined.contains("primordial"),
        "the faulting thread's frames must not be labelled primordial: {joined}"
    );
}

#[test]
fn an_unpublished_thread_still_falls_back_to_the_primordial_path() {
    // Strictly additive: the helper returns `None` wherever nothing is
    // published (the primordial thread included), so today's behaviour is
    // preserved everywhere it was the right behaviour.
    let lines = std::thread::spawn(|| java_stack_lines(8))
        .join()
        .expect("worker thread");
    assert!(!lines.is_empty());
    assert!(lines[0].starts_with("Java frames"), "got {:?}", lines[0]);
    assert!(
        !lines[0].contains("faulting thread"),
        "unpublished thread must not claim a faulting-thread trace: {:?}",
        lines[0]
    );
}

#[test]
fn the_full_vm_state_section_carries_the_faulting_threads_frames() {
    // `vm_diagnostic_lines` is what both allocating report paths call
    // (`CrashReport::render` and the Windows VEH), so the wiring has to be
    // visible from there and not only from `java_stack_lines` directly.
    let lines = std::thread::spawn(|| {
        let _guard = crate::vm::PublishedFrameTrace::publish(
            "carrier-3",
            3,
            published_trace("com/example/Mounted", "loop", 9),
        );
        vm_diagnostic_lines(None)
    })
    .join()
    .expect("carrier thread");

    let joined = lines.join("\n");
    assert!(joined.contains("jdk mode: "), "{joined}");
    assert!(joined.contains("carrier-3"), "{joined}");
    assert!(joined.contains("com/example/Mounted.loop"), "{joined}");
}

#[test]
fn a_held_frame_trace_mutex_is_reported_rather_than_waited_on() {
    // The actual crash-time situation: the faulting thread died in the
    // middle of republishing its own trace. A crash handler that blocks
    // here turns a diagnosable crash into a hang.
    let lines = std::thread::spawn(|| {
        let trace = published_trace("com/example/Wedged", "spin", 1);
        let _guard = crate::vm::PublishedFrameTrace::publish("wedged", 11, trace.clone());
        let _held = trace.lock();
        java_stack_lines(8)
    })
    .join()
    .expect("wedged thread");

    let joined = lines.join("\n");
    assert!(joined.contains("not waiting on it"), "{joined}");
    assert!(joined.contains("wedged"), "{joined}");
}

#[test]
fn crash_report_carries_the_vm_state_section() {
    let info = sample_crash_info();
    let report = generate_crash_report(&info);
    assert!(report.contains("V M  S T A T E"), "{report}");
    assert!(report.contains("jdk mode: "), "{report}");
    assert!(report.contains("gc collector: "), "{report}");
    assert!(report.contains("jit: "), "{report}");
    assert!(report.contains("Java frames"), "{report}");
}

#[test]
fn crash_report_header_names_the_class_library() {
    // Truncated reports lose the tail first, so the mode is duplicated into
    // the header. Guard the duplication so a future refactor cannot quietly
    // drop it back to the VM section only.
    let info = sample_crash_info();
    let report = generate_crash_report(&info);
    let header_end = report
        .find("---------------  T H R E A D")
        .expect("thread section marker");
    assert!(
        report[..header_end].contains("jdk mode: "),
        "header must name the class library: {}",
        &report[..header_end]
    );
}

#[test]
fn vm_state_summary_reports_mode_and_collector() {
    let s = get_vm_state();
    assert!(s.starts_with("CratonVM 25.0 ("), "{s}");
    assert!(s.contains("gc="), "{s}");
    assert!(
        !s.contains("detailed VM info unavailable"),
        "the mode and collector are published lock-free; they are available: {s}"
    );
}

/// Every pseudo-index reads its OWN machine register, and an index the
/// table does not know reports 0 rather than a plausible wrong one.
///
/// The accessor used to end in `_ => libc::REG_RBP`. With four callers that
/// was harmless. With the whole general-purpose file now printed, one
/// mistyped arm would put `rbp`'s value under `r13`'s label — and a crash
/// report is read once, long after the process is gone, by someone who
/// cannot re-run the fault to check. The mapping is therefore stated twice,
/// here and in the accessor, so a typo in either shows up as a mismatch.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn every_gp_register_index_reads_its_own_slot() {
    let table: [(usize, libc::c_int, &str); 16] = [
        (GREG_RAX, libc::REG_RAX, "rax"),
        (GREG_RBX, libc::REG_RBX, "rbx"),
        (GREG_RCX, libc::REG_RCX, "rcx"),
        (GREG_RDX, libc::REG_RDX, "rdx"),
        (GREG_RSI, libc::REG_RSI, "rsi"),
        (GREG_RDI, libc::REG_RDI, "rdi"),
        (GREG_R8, libc::REG_R8, "r8"),
        (GREG_R9, libc::REG_R9, "r9"),
        (GREG_R10, libc::REG_R10, "r10"),
        (GREG_R11, libc::REG_R11, "r11"),
        (GREG_R12, libc::REG_R12, "r12"),
        (GREG_R13, libc::REG_R13, "r13"),
        (GREG_R14, libc::REG_R14, "r14"),
        (GREG_R15, libc::REG_R15, "r15"),
        (GREG_RSP, libc::REG_RSP, "rsp"),
        (GREG_RBP, libc::REG_RBP, "rbp"),
    ];

    let mut uc: libc::ucontext_t = unsafe { std::mem::zeroed() };
    // A distinct, non-zero value per machine register, written through the
    // libc index, so nothing the accessor does can produce it by accident.
    for (i, (_, libc_idx, _)) in table.iter().enumerate() {
        uc.uc_mcontext.gregs[*libc_idx as usize] = 0x1000 + i as libc::greg_t;
    }
    let ucp = (&mut uc as *mut libc::ucontext_t).cast::<std::ffi::c_void>();

    for (i, (pseudo, _, name)) in table.iter().enumerate() {
        assert_eq!(
            greg_from_ucontext(ucp, *pseudo),
            0x1000 + i as u64,
            "the pseudo-index for {name} read a different register's slot"
        );
    }

    // The pseudo-indices are 0..=15; anything past the table is a caller
    // bug and must report 0, not the last arm's register.
    assert_eq!(
        greg_from_ucontext(ucp, table.len()),
        0,
        "an unknown pseudo-index must report 0, never a plausible register"
    );
    assert_eq!(
        greg_from_ucontext(std::ptr::null_mut(), GREG_RAX),
        0,
        "a null ucontext must report 0"
    );
}

/// `si_addr` is only an address when `si_code` says a fault produced it.
///
/// The signal handler's decode and its NULL-receiver line both hang off
/// this. Get it wrong in the permissive direction and a `kill -SEGV` — the
/// ordinary way to prod a wedged VM — prints a confident verdict about a
/// fault that never happened, in a report whose whole value is that it is
/// trustworthy.
#[cfg(unix)]
#[test]
fn only_a_hardware_fault_has_a_faulting_address() {
    // Per-signal fault codes: SEGV_MAPERR/SEGV_ACCERR/SEGV_BNDERR/
    // SEGV_PKUERR, and BUS_ADRALN/BUS_ADRERR/BUS_OBJERR, are 1..=4.
    for code in 1..=4 {
        assert!(
            si_code_means_a_faulting_address(code),
            "si_code {code} is a hardware fault"
        );
    }
    // SI_USER (kill), SI_QUEUE (sigqueue), SI_TKILL (tgkill): sent, so
    // si_addr is an unrelated union member.
    for code in [0, -1, -6] {
        assert!(
            !si_code_means_a_faulting_address(code),
            "si_code {code} was SENT, si_addr is not an address"
        );
    }
    // SI_KERNEL is kernel-raised but still carries no faulting address.
    assert!(!si_code_means_a_faulting_address(0x80));
}

/// The indexed-load search, against the registers a real crash carried, and
/// against the two that must NOT match it.
///
/// `decode_indexed_load` had its own test for the hibernate row already;
/// what is new here is that the search is now shared with the signal
/// handler, so this pins the part both callers depend on — including that
/// the total hit count is the TOTAL, not the number rendered, since the
/// Windows path prints `...` off exactly that difference.
#[test]
fn the_shared_indexed_load_search_matches_what_the_windows_reporter_reports() {
    // hs_err_pid3512, g1: fault == r10 + rax*4.
    let regs = [("rax", 0xEAF8_2DA0u64), ("r10", 0x7FF6_35D7_9014u64)];
    let mut seen = Vec::new();
    let hits = for_each_indexed_load_match(&regs, 0x0000_7FF9_E1B8_4694, |b, i, s, n| {
        seen.push((b.to_string(), i.to_string(), s, n))
    });
    assert_eq!(hits, 1, "exactly one pair explains this address");
    assert_eq!(seen[0].0, "r10");
    assert_eq!(seen[0].1, "rax");
    assert_eq!(seen[0].2, 4);
    // The rendering the Windows path builds on top of the same search.
    assert!(decode_indexed_load(&regs, 0x0000_7FF9_E1B8_4694)
        .expect("must decode")
        .contains("[r10+rax*4]"));

    // The two Tomcat SIGSEGVs of 2026-08-22. Neither is an indexed load,
    // and that is the point: nothing may attach the hibernate family's
    // analysis to them.
    for (addr, r10) in [
        (0xfusize, 0x708e_467f_6e48u64),
        (0x5, 0x8000_0000_0000_0000),
    ] {
        let regs = [("r10", r10), ("rax", 0)];
        assert_eq!(
            for_each_indexed_load_match(&regs, addr, |_, _, _, _| {}),
            0,
            "addr {addr:#x} must not decode as an indexed load"
        );
        assert!(decode_indexed_load(&regs, addr).is_none());
    }

    // A zero address is not decodable, however degenerate the registers.
    assert_eq!(
        for_each_indexed_load_match(&[("rax", 7), ("rbx", 9)], 0, |_, _, _, _| {}),
        0
    );
}
