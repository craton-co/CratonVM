use super::*;
use crate::test_support::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Serializes the tests that call `spawn_and_wrap`.
///
/// `set_spawn_policy_hook` installs a PROCESS-GLOBAL hook, and
/// `spawn_policy_hook_is_consulted_and_can_refuse_the_fork` spends part of
/// its run with that hook set to refuse everything. Any other test
/// spawning through `spawn_and_wrap` at that moment is refused too, which
/// showed up as a flaky `SecurityException` from an unrelated test.
static SPAWN_TEST_LOCK: Mutex<()> = Mutex::new(());

/// The table set the mock context's natives use (no VM slot), which
/// the direct calls below share with them.
fn tt() -> &'static Arc<ProcessTables> {
    detached_tables_ref()
}

/// A stand-in `TimeUnit` for the mock context (interpreter round i5 wave
/// 102): an object whose next `toMillis(J)J` answers `millis`. The timed
/// `waitFor` stand-in now throws the JDK bodies' NPE for a null unit.
fn mock_millis_unit(ctx: &mut MockNativeContext, millis: i64) -> ObjectRef {
    ctx.script("toMillis", "(J)J", Ok(Some(Value::Long(millis))));
    ctx.alloc_object(1)
}

/// Interpreter round i5 wave 102, lane L1: `waitFor(long, null)` throws
/// the NPE every JDK body throws first (`unit.toNanos(timeout)`, "throw
/// NPE before other conditions"), for a VM-spawned child that has exited
/// and for a foreign receiver alike.
#[test]
fn w102_l1_a_null_unit_is_the_jdk_bodies_npe() {
    let mut ctx = MockNativeContext::new();
    let foreign = ctx.alloc_object(2);
    for timeout in [0i64, 5] {
        let thrown = native_process_wait_for_timeout(
            &mut ctx,
            &[
                Value::Object(Some(foreign)),
                Value::Long(timeout),
                Value::Object(None),
            ],
        )
        .expect_err("a null unit throws");
        assert!(
            matches!(
                thrown,
                MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::NullPointerException { message: Some(ref m) }
                )) if m == WAIT_FOR_NULL_UNIT_NPE
            ),
            "{thrown:?}"
        );
    }
}

/// Interpreter round i5 wave 101, lane L1: two VMs' registries hold two
/// table sets. A child installed in one is not found by pid, handle,
/// liveness or destroy through the other, and its exit is published in
/// its own set only.
#[test]
fn w101_l1_two_registries_hold_apart_process_tables() {
    let reg_a = NativeMethodRegistry::new();
    let reg_b = NativeMethodRegistry::new();
    let a = tables_in(reg_a.process_tables_slot());
    let b = tables_in(reg_b.process_tables_slot());
    assert!(!Arc::ptr_eq(&a, &b), "one table set per registry");
    assert!(
        Arc::ptr_eq(&a, &tables_in(reg_a.process_tables_slot())),
        "the slot keeps its set"
    );
    #[cfg(windows)]
    let child = Command::new("cmd")
        .args(["/c", "exit", "4"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cmd");
    #[cfg(not(windows))]
    let child = Command::new("/bin/sh")
        .args(["-c", "exit 4"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn /bin/sh");
    let pid = child.id() as i64;
    let handle = a.next_handle.fetch_add(1, Ordering::Relaxed);
    a.children.lock().insert(handle, Arc::new(Mutex::new(child)));
    a.exits.lock().insert(
        handle,
        ExitCache {
            pid,
            exit_code: None,
        },
    );
    assert_eq!(handle_for_pid(&a, pid), Some(handle));
    assert_eq!(handle_for_pid(&b, pid), None, "B never saw A's child");
    assert_eq!(pid_for_handle(&b, handle), -1);
    assert!(!destroy_handle(&b, handle, true), "B cannot destroy it");
    let seen_b = process_exit_generation(&b);
    assert_eq!(wait_for_handle(&a, handle), 4);
    assert_eq!(try_exit_handle(&a, handle), Some(4));
    assert_eq!(try_exit_handle(&b, handle), None);
    assert_eq!(process_exit_generation(&b), seen_b, "A's exit is A's");
    drop(reg_a);
    assert_eq!(Arc::strong_count(&a), 1, "the registry released its set");
}

/// Interpreter round i5 wave 101, lane L1: a spawn failure's OS half is
/// the JDK spawn native's text (`ProcessBuilder.start` adds `Cannot run
/// program ...`), and Rust's code-less "program not found" (Windows `PATH`
/// resolution) reads as `ERROR_FILE_NOT_FOUND`.
#[test]
fn w101_l1_spawn_failure_detail_is_the_jdk_natives_text() {
    let enoent = spawn_failure_detail(&std::io::Error::from_raw_os_error(2));
    if cfg!(windows) {
        assert!(enoent.starts_with("CreateProcess error=2, "), "{enoent}");
        assert!(!enoent.ends_with('.'), "{enoent}");
    } else {
        assert_eq!(enoent, "Exec failed, error: 2 (No such file or directory) ");
    }
    assert!(!enoent.contains("os error"), "{enoent}");
    let unresolved = std::io::Error::new(std::io::ErrorKind::NotFound, "program not found");
    assert_eq!(spawn_failure_detail(&unresolved), enoent);
    assert_eq!(
        cannot_run_program("nosuch", Some("d"), "error=2, x"),
        "Cannot run program \"nosuch\" (in directory \"d\"): error=2, x"
    );
}

fn install_child_for_test(child: Child) -> (i64, i64) {
    let handle = tt().next_handle.fetch_add(1, Ordering::Relaxed);
    let pid = child.id() as i64;
    tt().children.lock().insert(handle, Arc::new(Mutex::new(child)));
    tt().exits.lock().insert(
        handle,
        ExitCache {
            pid,
            exit_code: None,
        },
    );
    (handle, pid)
}

/// The merged one-`OpenProcess` probe must answer the SAME start time the
/// separate `os_process_start_time` does.
///
/// This is not a tautology and it is the whole reason the merge is safe to
/// make. `ProcessHandleImpl$Info.info(pid, startTime)` — real JDK bytecode
/// — compares the handle's start time with the one `info0` wrote using a
/// bare `!=` with no `STARTTIME_ANY` wildcard, and wipes `command`,
/// `arguments`, `startTime`, `totalTime` and `user` off the record on a
/// mismatch of one millisecond. `isAlive0` supplies the handle's number and
/// `info0` supplies the other, so the moment `isAlive0` sources its start
/// time from a *different* probe than `start_time_or_any`, W5-2's two
/// silently-skipped `RJdkProcess` checks come straight back.
///
/// The other two rows are the arms the old boolean `foreign_pid_is_alive`
/// carried and this probe inherited: a pid outside the `DWORD` range is
/// refused without a syscall (which is what keeps
/// `ProcessHandle.of(Long.MAX_VALUE)` cheap and empty), and a pid no
/// process holds is not alive.
#[test]
#[cfg(windows)]
fn one_open_reports_the_same_start_time_as_the_separate_probe() {
    let me = std::process::id() as i64;
    let (alive, start) = win_liveness_and_start_time(me);
    assert!(alive, "this process is running");
    assert_eq!(
        start,
        os_process_start_time(me),
        "the merged probe and `start_time_or_any`'s source must be ONE \
         number; `Info.info(pid, startTime)` compares them with a bare `!=`"
    );
    assert_eq!(
        foreign_start_time_or_dead(me),
        start_time_or_any(me),
        "and so must what `isAlive0` hands back"
    );

    assert_eq!(
        win_liveness_and_start_time(i64::MAX),
        (false, None),
        "a pid outside the DWORD range cannot name a process, and is \
         refused without a syscall"
    );
    assert_eq!(
        foreign_start_time_or_dead(i64::MAX),
        -1,
        "-1 is the only value `ProcessHandleImpl.isAlive()` reads as dead"
    );
}

/// The Linux twin of the test above, asserting the same identity for the
/// same reason — and it is the only scheduled witness this arm has.
///
/// The Linux merge has no Java-visible witness: what it removes is a
/// pid-recycle attribution window and a race between two `/proc` reads,
/// neither of which a regression-suite vector can provoke on demand. What
/// IS assertable, and is the thing that would break if the merge were wrong,
/// is the identity `Info.info(pid, startTime)` depends on — `isAlive0`'s
/// start time and `start_time_or_any`'s must be ONE number, compared there
/// with a bare `!=` that wipes five fields on a mismatch of one
/// millisecond. The moment `linux_liveness_and_start_time` parses field 22
/// differently from `linux_proc_stat_times`, W5-2's two silently-skipped
/// `RJdkProcess` checks come straight back; a shared
/// `linux_stat_line_times` is what makes that unfalsifiable, and this is
/// what checks it stayed shared.
///
/// The third row is the arm the deleted boolean `foreign_pid_is_alive`
/// carried: a pid no process holds is not alive, and `-1` is the only value
/// `ProcessHandleImpl.isAlive()` reads that way. `i64::MAX` is far outside
/// `pid_max`, so `/proc/<pid>/stat` is `ENOENT` for it on any host.
#[test]
#[cfg(target_os = "linux")]
fn one_stat_read_reports_the_same_start_time_as_the_separate_probe() {
    let me = std::process::id() as i64;
    let (alive, start) = linux_liveness_and_start_time(me);
    assert!(alive, "this process is running");
    assert_eq!(
        start,
        os_process_start_time(me),
        "the merged probe and `start_time_or_any`'s source must be ONE \
         number; `Info.info(pid, startTime)` compares them with a bare `!=`"
    );
    assert_eq!(
        foreign_start_time_or_dead(me),
        start_time_or_any(me),
        "and so must what `isAlive0` hands back"
    );

    assert_eq!(
        linux_liveness_and_start_time(i64::MAX),
        (false, None),
        "no process holds that pid, and an absent /proc/<pid>/stat is how \
         this platform says so"
    );
    assert_eq!(
        foreign_start_time_or_dead(i64::MAX),
        -1,
        "-1 is the only value `ProcessHandleImpl.isAlive()` reads as dead"
    );
}

/// The two ids are different numbers, and a `ProcessHandleImpl` native
/// handed the pid must still find the child.
#[test]
#[cfg(unix)]
fn a_real_pid_resolves_to_its_table_handle() {
    let child = Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn /bin/sleep");
    let (handle, pid) = install_child_for_test(child);
    assert_ne!(
        handle, pid,
        "handle is a counter and pid is the OS's; a test where they \
         coincide would pass for the wrong reason"
    );

    assert_eq!(handle_for_pid(tt(), pid), Some(handle), "by pid");

    // `isAlive0` answers a START TIME, not a boolean: >= 0 is alive and -1
    // is the only way to say otherwise.
    let alive =
        native_proc_handle_is_alive0(&mut MockNativeContext::new(), &[Value::Long(pid)])
            .unwrap();
    let Some(Value::Long(start)) = alive else {
        panic!("isAlive0 must answer a long, got {alive:?}");
    };
    assert!(start >= 0, "a running child is alive, got {start}");

    destroy_handle(tt(), handle, true);
    let code = wait_for_handle(tt(), handle);
    assert!(code != 0, "killed child should not report success: {code}");

    let dead = native_proc_handle_is_alive0(&mut MockNativeContext::new(), &[Value::Long(pid)])
        .unwrap();
    assert_eq!(dead, Some(Value::Long(-1)), "exited, by pid");
}

/// `argc`/`envc` are passed for a reason: an empty argument is an
/// argument.
///
/// The block is `count` NUL-terminated strings laid end to end, so the
/// split yields a trailing empty piece that is NOT an argument, and an
/// empty argument in the middle that IS one. Dropping every empty piece —
/// what the decoder did before — gets both wrong in the same direction.
#[test]
fn a_nul_block_keeps_empty_arguments_and_drops_the_trailer() {
    // `sh -c 'printf [%s] "$1" "$2"' sh "" z` — argv[2] is deliberately "".
    let block = b"sh\0\0z\0";
    assert_eq!(
        split_nul_block(block, 3),
        vec!["sh".to_string(), String::new(), "z".to_string()],
    );
    // Asking for fewer than the block holds takes a prefix, never a scan
    // for a terminator that is not there.
    assert_eq!(split_nul_block(block, 1), vec!["sh".to_string()]);
    assert!(split_nul_block(b"", 0).is_empty());
}

/// The Windows environment block is one NUL-separated String, not the
/// `String[]` the old `create` registration decoded.
///
/// The block is NUL-*terminated*, so the last split piece is empty and must
/// not become a variable; `=C:=...` entries are Windows' per-drive
/// current-directory pseudo-variables and are dropped rather than forwarded
/// as a variable literally named `=C:`. An empty VALUE is legal and is kept.
#[test]
#[cfg(windows)]
fn an_environment_block_splits_on_nul_and_drops_the_drive_pseudo_vars() {
    assert_eq!(
        parse_env_block("PATH=C:\\bin\0HOME=C:\\me\0"),
        vec![
            ("PATH".to_string(), "C:\\bin".to_string()),
            ("HOME".to_string(), "C:\\me".to_string()),
        ]
    );
    assert_eq!(
        parse_env_block("=C:=C:\\work\0FOO=bar\0"),
        vec![("FOO".to_string(), "bar".to_string())],
        "a per-drive cwd pseudo-variable is not an environment variable"
    );
    assert_eq!(
        parse_env_block("EMPTY=\0"),
        vec![("EMPTY".to_string(), String::new())],
        "an empty value is legal and must survive"
    );
    assert!(parse_env_block("").is_empty());
    assert!(
        parse_env_block("NOEQUALS\0").is_empty(),
        "an entry with no '=' is not a variable"
    );
}

/// `toCString` output is NUL-terminated; the terminator is not part of the
/// string, and neither is anything the JDK left in the tail.
#[test]
fn a_c_string_stops_at_its_terminator() {
    assert_eq!(decode_c_string(b"/bin/sh\0"), "/bin/sh");
    assert_eq!(decode_c_string(b"/bin/sh\0junk"), "/bin/sh");
    assert_eq!(decode_c_string(b"/bin/sh"), "/bin/sh", "no terminator");
    assert_eq!(decode_c_string(b"\0"), "");
}

/// The three `fds[i]` input cases, and why the middle one is unambiguous.
///
/// `Redirect.INHERIT` puts the slot's own index in the slot, and fd-table
/// ids 0/1/2 are permanently the VM's own standard streams with the
/// counter starting at 3 — so `fds[1] == 1` can only ever mean "stdout",
/// whichever way you read it.
///
/// Windows reaches the same decoder through `ProcessImpl.create`'s
/// `long[] stdHandles` rather than `forkAndExec`'s `int[] fds` — the values
/// are `FdTable` ids on both platforms — so the test covers both arms.
#[test]
#[cfg(any(target_os = "linux", windows))]
fn a_requested_fd_maps_to_pipe_inherit_or_an_existing_descriptor() {
    assert!(matches!(
        redirect_from_requested_fd(0, -1),
        StdioRedirect::Pipe
    ));
    assert!(matches!(
        redirect_from_requested_fd(1, 1),
        StdioRedirect::Inherit
    ));
    assert!(matches!(
        redirect_from_requested_fd(2, 2),
        StdioRedirect::Inherit
    ));
    // A file redirect: the JDK opened it and handed us its table id.
    assert!(matches!(
        redirect_from_requested_fd(1, 7),
        StdioRedirect::ExistingFd(7)
    ));
    // The slot index is compared against ITS OWN slot, so id 0 in the
    // stdout slot is a descriptor, not an inherit.
    assert!(matches!(
        redirect_from_requested_fd(1, 0),
        StdioRedirect::ExistingFd(0)
    ));
}

/// `destroy` must still reach a child that a `waitFor` is blocked on.
///
/// The real JDK's `ProcessImpl` puts a reaper thread into
/// `waitForProcessExit0` for every child it spawns, so this is the normal
/// state rather than a race. Before the table held the child behind its own
/// mutex, the waiter took it out and `destroy` silently did nothing.
#[test]
#[cfg(unix)]
fn destroy_reaches_a_child_a_waiter_is_blocked_on() {
    let child = Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn /bin/sleep");
    let (handle, _pid) = install_child_for_test(child);

    let waiter = std::thread::spawn(move || wait_for_handle(tt(), handle));
    // Give the waiter time to actually be inside `Child::wait()`. If it is
    // not yet, `destroy_handle` takes the fast path and the test still
    // asserts the thing that matters — that the child dies.
    std::thread::sleep(Duration::from_millis(200));

    assert!(destroy_handle(tt(), handle, true), "destroy must report success");
    let code = waiter.join().expect("waiter thread");
    assert_eq!(code, 128 + 9, "SIGKILL is reported as 128+signum");
}

/// A pid we never spawned resolves to nothing, and the bridge says so
/// rather than reaching into the table with it.
///
/// The literal -2 is written out on purpose: it is the value
/// `ProcessHandleImpl.completion` compares against, so a test that spelled
/// it `PROCESS_NOT_A_CHILD` on both sides would pass no matter what the
/// constant was changed to.
#[test]
fn an_unspawned_pid_resolves_to_no_handle() {
    let foreign = 0x7fff_0000i64; // far above any pid this host will mint
    assert_eq!(handle_for_pid(tt(), foreign), None);
    let mut ctx = MockNativeContext::new();
    let rc =
        native_proc_handle_wait_for_process_exit0(&mut ctx, &[Value::Long(foreign)]).unwrap();
    // Windows (wave 100, lane L1): HotSpot's Windows native opens any pid
    // and answers -1 for one it cannot open; it never answers NOT_A_CHILD.
    #[cfg(windows)]
    assert_eq!(rc, Some(Value::Int(-1)), "no such process: -1, as HotSpot's Windows native");
    #[cfg(not(windows))]
    assert_eq!(rc, Some(Value::Int(-2)), "NOT_A_CHILD, not an exit status");
    assert_eq!(
        native_proc_handle_destroy_process0(&mut ctx, &[Value::Long(foreign), Value::Int(1)])
            .unwrap(),
        Some(Value::Int(0)),
        "and must not signal it"
    );
}

/// The VM's own pid must read as alive: `ProcessHandleImpl.<clinit>` seeds
/// `current` with `isAlive0(getCurrentPid0())`, and -1 there would make the
/// VM report its own process as nonexistent.
#[test]
fn the_current_process_is_alive_by_its_real_pid() {
    let mut ctx = MockNativeContext::new();
    let self_pid = std::process::id() as i64;
    let answer = native_proc_handle_is_alive0(&mut ctx, &[Value::Long(self_pid)]).unwrap();
    let Some(Value::Long(start)) = answer else {
        panic!("isAlive0 must answer a long, got {answer:?}");
    };
    assert!(start >= 0, "this process exists, got {start}");
}

/// The three answers `ProcessHandleImpl$Info.info(pid, startTime)` compares
/// must be ONE number.
///
/// That method is real JDK bytecode this VM cannot influence:
///
/// ```text
/// info.info0(pid);
/// if (startTime != info.startTime) { info.command = null; ... }
/// ```
///
/// — a bare `!=`, with none of the `STARTTIME_ANY` wildcarding that
/// `equals()` and `isAlive()` apply. So `info0`'s stamp, `isAlive0`'s
/// return (which is what the handle's `startTime` field is built from) and
/// `current_process_start_time` (which builds THE `current()` handle) have
/// to agree exactly, or every field `info0` just wrote is discarded by its
/// own caller and `ProcessHandle.current().info()` reports an empty record
/// while nothing throws.
///
/// This asserts the agreement, not a particular value: a host that cannot
/// read a start time at all legitimately answers `STARTTIME_ANY` (0) from
/// all three, and that is still a match.
#[test]
fn info0_stamps_the_start_time_isalive0_and_current_both_report() {
    let mut ctx = MockNativeContext::new();
    let pid = std::process::id() as i64;

    let info = MockNativeContext::alloc_object(&mut ctx, 0);
    native_proc_handle_info0(&mut ctx, &[Value::Object(Some(info)), Value::Long(pid)])
        .expect("info0 must not throw");
    let stamped = ctx.get_field_by_name(info, "startTime");

    let answer = native_proc_handle_is_alive0(&mut ctx, &[Value::Long(pid)]).unwrap();
    let Some(alive) = answer else {
        panic!("isAlive0 must answer a long, got {answer:?}");
    };

    assert_eq!(
        stamped, alive,
        "info0's Info.startTime and isAlive0's return are the two sides of \
         `startTime != info.startTime` in ProcessHandleImpl$Info.info; a \
         difference silently empties ProcessHandle.current().info()"
    );
    assert_eq!(
        stamped,
        Value::Long(current_process_start_time()),
        "and current_process_start_time builds the handle that supplies the \
         left-hand side of that same comparison"
    );
}

/// `info0` must report a command for this very process.
///
/// `RJdkProcess.java:135` guards its check with
/// `if (info.command().isPresent())`, so an empty `command` does not fail
/// the vector — it makes one `check(...)` silently not run, and HotSpot 25
/// runs it. `command` is the one `Info` field both platform arms can
/// source: `/proc/<pid>/cmdline` on Linux, `QueryFullProcessImageNameW` on
/// Windows.
#[test]
#[cfg(any(target_os = "linux", windows))]
fn info0_reports_a_command_for_this_process() {
    let mut ctx = MockNativeContext::new();
    let info = MockNativeContext::alloc_object(&mut ctx, 0);
    native_proc_handle_info0(
        &mut ctx,
        &[
            Value::Object(Some(info)),
            Value::Long(std::process::id() as i64),
        ],
    )
    .expect("info0 must not throw");
    assert!(
        matches!(
            ctx.get_field_by_name(info, "command"),
            Value::Object(Some(_))
        ),
        "Info.command must be a String, not the null the constructor leaves \
         behind — an absent one takes RJdkProcess.java:136 with it"
    );
}

/// Interpreter round i5 wave 101, lane L1: Linux `Info.command` is the
/// resolved `/proc/<pid>/exe` (HotSpot's `os_getCmdlineAndUserInfo`), not
/// `argv[0]`; the arguments and the command line are the record's words.
#[test]
#[cfg(target_os = "linux")]
fn w101_l1_linux_info_command_is_the_resolved_executable() {
    use std::io::Read;
    let mut child = Command::new("/bin/sh")
        .args(["-c", "printf ready; sleep 30; true"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn /bin/sh");
    let mut ready = [0; 5];
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut ready)
        .unwrap();
    assert_eq!(&ready, b"ready");
    let pid = child.id() as i64;
    let read = os_process_cmdline(pid).expect("/proc cmdline");
    let _ = child.kill();
    let _ = child.wait();
    let exe = std::fs::canonicalize("/bin/sh")
        .expect("canonical /bin/sh")
        .to_string_lossy()
        .into_owned();
    assert_eq!(read.command.as_deref(), Some(exe.as_str()));
    assert_eq!(
        read.arguments,
        Some(vec![
            "-c".to_string(),
            "printf ready; sleep 30; true".to_string()
        ])
    );
    assert_eq!(
        read.command_line.as_deref(),
        Some("/bin/sh -c printf ready; sleep 30; true")
    );
}

/// `destroy0` must refuse a pid whose start time disagrees with the
/// caller's — that is the whole guard against acting on a recycled pid —
/// and must trust a caller that never learned one.
#[test]
#[cfg(target_os = "linux")]
fn destroy_refuses_a_stale_start_time_and_trusts_an_absent_one() {
    let child = Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn /bin/sleep");
    let pid = child.id() as i64;
    // Deliberately NOT installed in the process table: this is the foreign
    // path, and a child in the table is destroyed through its `Child`.
    let mut child = child;

    let real = os_process_start_time(pid).expect("/proc start time");
    assert!(start_time_matches(pid, real), "its own start time");
    assert!(
        !start_time_matches(pid, real - 1),
        "a start time from some earlier holder of this pid"
    );
    assert!(
        start_time_matches(pid, PROCESS_STARTTIME_ANY),
        "a caller that never learned one is trusted, as in HotSpot"
    );
    // An exited process has no start time to disagree with, so the check
    // cannot be what stops a signal — `kill` failing is.
    assert!(
        start_time_matches(pid + 4_000_000, real),
        "unknown is not disagreement"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// A receiver shaped AND named like one of the VM's own process objects.
///
/// The class name is load-bearing, not decoration: every concrete
/// `java.lang.Process` native now asks `is_vm_process` before trusting the
/// `PROC_FIELD_*` slots, so an unnamed mock takes the foreign-receiver path
/// and a test written for the VM path would quietly measure the other one.
fn mock_process(ctx: &mut MockNativeContext, handle: i64, pid: i64) -> ObjectRef {
    let proc_ref = ctx.alloc_object_with_class(PROC_FIELD_COUNT, SYNTHETIC_PROCESS_CLASS);
    ctx.set_field(proc_ref, PROC_FIELD_EXIT, Value::Int(EXIT_NOT_YET));
    ctx.set_field(proc_ref, PROC_FIELD_PID, Value::Long(pid));
    ctx.set_field(proc_ref, PROC_FIELD_HANDLE, Value::Long(handle));
    proc_ref
}

/// A spawn must hand back a LIVE, READABLE stdout pipe.
///
/// This is the property `Runtime.exec` did not have: it ran the child to
/// completion with `Command::output()` and stored the bytes as a Java
/// String on an object nothing downstream could read, so
/// `Process.getInputStream()` fell through to fd -1 and every read
/// returned EOF. Tomcat's `CGIServlet` copies exactly that stream into
/// the HTTP response, which is why `TestSecurity2019.testCVE_2019_0232`
/// saw `200 OK` with an empty body.
///
/// Asserted through the `FdTable`, the same route the pipe-stream natives
/// take, rather than through the Java stream objects: the point is that
/// the fd recorded on the Process is the child's real stdout.
#[test]
#[cfg(not(target_os = "windows"))]
fn spawn_and_wrap_exposes_a_live_stdout_pipe() {
    let _guard = SPAWN_TEST_LOCK.lock();
    let mut ctx = MockNativeContext::new();
    let result = spawn_and_wrap(
        &mut ctx,
        "/bin/echo",
        &["hello-from-the-child".to_string()],
        None,
        None,
        false,
        false,
    )
    .expect("spawn /bin/echo")
    .expect("spawn returns a Process");
    let proc_ref = match result {
        Value::Object(Some(o)) => o,
        other => panic!("expected a Process object, got {other:?}"),
    };

    let handle = match ctx.get_field(proc_ref, PROC_FIELD_HANDLE) {
        Value::Long(h) => h,
        other => panic!("PROC_FIELD_HANDLE must be a Long, got {other:?}"),
    };
    assert_ne!(handle, 0, "a spawned Process must carry a live handle");

    let stdout_fd = match ctx.get_field(proc_ref, PROC_FIELD_STDOUT_FD) {
        Value::Int(fd) => fd,
        other => panic!("PROC_FIELD_STDOUT_FD must be an Int, got {other:?}"),
    };
    assert!(
        stdout_fd >= 0,
        "a piped stdout must have a real fd-table id, got {stdout_fd}"
    );

    let mut buf = [0u8; 64];
    let n = ctx
        .fd_table()
        .read_bytes(stdout_fd as FdId, &mut buf)
        .expect("read the child's stdout");
    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(
        text.contains("hello-from-the-child"),
        "the child's stdout must be readable through the recorded fd, got {text:?}"
    );

    assert_eq!(wait_for_handle(tt(), handle), 0);
}

/// The spawn policy gate runs BEFORE the fork, on every spawn route.
///
/// `SecurityManager.checkExec` used to be applied by `Runtime.exec` and by
/// `native-builtins`' own (shadowed) `ProcessBuilder.start`, but NOT by the
/// `ProcessBuilder.start` that actually wins at runtime, so a deny-all
/// policy refused one entry point and let the other fork the same child.
/// The gate now lives in `spawn_and_wrap`, which all of them funnel
/// through.
///
/// Both arms are in one test on purpose: the hook is a process-global
/// `OnceLock`, so a second test installing a different one would silently
/// keep the first.
#[test]
#[cfg(not(target_os = "windows"))]
fn spawn_policy_hook_is_consulted_and_can_refuse_the_fork() {
    let _guard = SPAWN_TEST_LOCK.lock();
    use std::sync::atomic::AtomicBool;

    static DENY: AtomicBool = AtomicBool::new(true);
    static SAW: Mutex<Option<String>> = Mutex::new(None);

    fn hook(_ctx: &mut dyn NativeContext, program: &str) -> Result<(), MethodCallFailed> {
        *SAW.lock() = Some(program.to_string());
        if DENY.load(Ordering::Relaxed) {
            Err(RuntimeError::SecurityException {
                message: format!("test policy denies {program}"),
            }
            .into())
        } else {
            Ok(())
        }
    }
    set_spawn_policy_hook(hook);

    // --- deny arm: the error propagates verbatim and nothing is spawned.
    let mut ctx = MockNativeContext::new();
    let marker =
        std::env::temp_dir().join(format!("cratonvm-spawn-policy-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let denied = spawn_and_wrap(
        &mut ctx,
        "/usr/bin/touch",
        &[marker.to_string_lossy().into_owned()],
        None,
        None,
        false,
        false,
    );
    match denied {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::SecurityException { .. },
        ))) => {}
        other => panic!("a refused spawn must surface the SecurityException, got {other:?}"),
    }
    assert_eq!(
        SAW.lock().as_deref(),
        Some("/usr/bin/touch"),
        "the gate must see command[0]"
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !marker.exists(),
        "a refused spawn must not fork: {} exists",
        marker.display()
    );

    // --- allow arm: the same call goes through once the gate says yes.
    DENY.store(false, Ordering::Relaxed);
    let allowed = spawn_and_wrap(
        &mut ctx,
        "/bin/echo",
        &["ok".to_string()],
        None,
        None,
        false,
        false,
    )
    .expect("an allowed spawn must proceed")
    .expect("spawn returns a Process");
    if let Value::Object(Some(proc_ref)) = allowed {
        if let Value::Long(handle) = ctx.get_field(proc_ref, PROC_FIELD_HANDLE) {
            assert_eq!(wait_for_handle(tt(), handle), 0);
        }
    }
    let _ = std::fs::remove_file(&marker);
}

/// `isAlive0` is keyed by PID and answers with a START TIME.
///
/// Both halves were wrong at once. It took `args[0]` -- the JDK's pid --
/// and looked it up as a `NEXT_HANDLE` id, two unrelated number spaces, so
/// it found nothing and reported "still running" for every pid forever.
/// And it returned 1/0 as though `ProcessHandleImpl.isAlive()` read a
/// boolean; that method reads the value as a start time and treats ANY
/// value >= 0 as alive, so the 0 it returned for an exited process meant
/// alive as well. The two errors could not cancel out: the answer was
/// "alive" either way.
///
/// -1 is the only encoding of "not alive"; 0 is "alive, start time
/// unknown".
#[test]
#[cfg(not(target_os = "windows"))]
fn is_alive0_is_keyed_by_pid_and_reports_a_start_time() {
    let child = Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let (handle, pid) = install_child_for_test(child);
    assert_ne!(handle, pid, "the test is meaningless if the two coincide");
    let mut ctx = MockNativeContext::new();

    let alive = native_proc_handle_is_alive0(&mut ctx, &[Value::Long(pid)])
        .unwrap()
        .unwrap();
    let Value::Long(start) = alive else {
        panic!("isAlive0 must answer a long, got {alive:?}");
    };
    assert!(start >= 0, "a running child must be alive, got {start}");

    // On a host with `/proc` the answer is a real epoch-millisecond start
    // time, and the value is what makes a pid safe to act on — so it has to
    // be STABLE. A start time that changed between calls would pass
    // `destroy0`'s staleness check only by accident. Not asserted as a
    // literal: it is different on every run, and different again on a host
    // without `/proc`, where the honest answer is the `STARTTIME_ANY` 0.
    let again = native_proc_handle_is_alive0(&mut ctx, &[Value::Long(pid)])
        .unwrap()
        .unwrap();
    assert_eq!(again, alive, "the start time must not move under us");
    if cfg!(target_os = "linux") {
        assert!(
            start > 0,
            "on Linux the start time comes from /proc/<pid>/stat, got {start}"
        );
        assert!(
            start_time_matches(pid, start),
            "the value isAlive0 reports must be the one destroy0 accepts"
        );
        assert!(
            !start_time_matches(pid, start + 1),
            "and a value it did NOT report must be refused, or the check is \
             decoration"
        );
    }

    // A pid that is neither ours nor anyone's must be -1. (Deliberately
    // NOT asserted for the internal handle id: on Linux a small integer is
    // a perfectly plausible live pid -- `/proc/1` is init -- so that
    // assertion would be testing the host, not this code.)
    let nobody = native_proc_handle_is_alive0(&mut ctx, &[Value::Long(pid + 4_000_000)])
        .unwrap()
        .unwrap();
    assert_eq!(nobody, Value::Long(-1), "an unused pid is not alive");

    assert!(destroy_handle(tt(), handle, true));
    let _ = wait_for_handle(tt(), handle);

    let dead = native_proc_handle_is_alive0(&mut ctx, &[Value::Long(pid)])
        .unwrap()
        .unwrap();
    assert_eq!(
        dead,
        Value::Long(-1),
        "a reaped child must report -1, the only value ProcessHandleImpl \
         .isAlive() reads as not-alive"
    );
}

/// `destroyProcess0` and `waitForProcessExit0` are keyed by pid too.
#[test]
#[cfg(not(target_os = "windows"))]
fn destroy_and_wait_natives_resolve_the_pid_not_the_handle() {
    let child = Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let (handle, pid) = install_child_for_test(child);
    let mut ctx = MockNativeContext::new();

    // A pid we never spawned cannot be waited for, and the way to say so
    // is `NOT_A_CHILD` (-2), not -1. This assertion read -1 when it was
    // written; -1 is a value `ProcessHandleImpl.completion` hands the
    // caller as the process's exit status, so it completed
    // `ProcessHandle.of(pid).onExit()` immediately for a live process.
    // See `probes/ForeignHandleProbe.java`.
    let unknown = native_proc_handle_wait_for_process_exit0(
        &mut ctx,
        &[Value::Long(pid + 4_000_000), Value::Int(0)],
    )
    .unwrap()
    .unwrap();
    assert_eq!(unknown, Value::Int(-2));

    let killed =
        native_proc_handle_destroy_process0(&mut ctx, &[Value::Long(pid), Value::Int(1)])
            .unwrap()
            .unwrap();
    assert_eq!(
        killed,
        Value::Int(1),
        "destroyProcess0 must find the child by pid"
    );

    let code =
        native_proc_handle_wait_for_process_exit0(&mut ctx, &[Value::Long(pid), Value::Int(0)])
            .unwrap()
            .unwrap();
    assert!(
        matches!(code, Value::Int(c) if c != -1 && c != -2),
        "waitForProcessExit0 must find the child by pid, got {code:?}"
    );

    let _ = wait_for_handle(tt(), handle);
}

/// Interpreter round i2 wave 70, lane L1 (`CRATONVM_PROCESS_EXIT_SIGNAL`):
/// an exit reaper publishes the child's exit through the signal, so a
/// waiter that read the generation before the exit wakes without waiting
/// out its slice, and a second `ensure` for the same handle starts no
/// second reaper.
#[test]
fn the_exit_reaper_publishes_the_exit_through_the_signal() {
    #[cfg(target_os = "windows")]
    let child = Command::new("cmd")
        .args(["/c", "exit", "3"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cmd /c exit 3");
    #[cfg(not(target_os = "windows"))]
    let child = Command::new("/bin/sh")
        .args(["-c", "exit 3"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn /bin/sh -c 'exit 3'");
    let handle = 42424343i64;
    let pid = child.id() as i64;
    tt().children.lock().insert(handle, Arc::new(Mutex::new(child)));
    tt().exits.lock().insert(
        handle,
        ExitCache {
            pid,
            exit_code: None,
        },
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = process_exit_generation(tt());
    assert!(ensure_process_exit_reaper(tt(), handle));
    assert!(
        ensure_process_exit_reaper(tt(), handle),
        "a second ensure is a no-op"
    );
    let code = loop {
        if let Some(code) = tt().exits.lock().get(&handle).and_then(|c| c.exit_code) {
            break code;
        }
        assert!(
            Instant::now() < deadline,
            "the reaper never published the exit"
        );
        wait_process_exit_signal(tt(), seen, Duration::from_millis(500));
        seen = process_exit_generation(tt());
    };
    assert_eq!(code, 3);
    assert_eq!(try_exit_handle(tt(), handle), Some(3));
    // A generation that already moved returns at once, whatever the slice.
    let before = Instant::now();
    wait_process_exit_signal(
        tt(),
        process_exit_generation(tt()).wrapping_sub(1),
        Duration::from_secs(30),
    );
    assert!(before.elapsed() < Duration::from_secs(5));
}

/// Interpreter round i5 wave 102, lane L1 (`CRATONVM_PROCESS_PIDFD_WAIT`):
/// a pidfd wait wakes at the child's exit and answers its code, reaped
/// (no zombie: a second `try_exit_handle` answers from the cache and
/// `waitid` finds nothing left), and a killed child answers 128 + signal.
/// Skipped where `pidfd_open` is refused (a pre-5.3 kernel, seccomp).
#[cfg(target_os = "linux")]
#[test]
fn w102_l1_a_pidfd_wait_answers_the_exit_and_reaps() {
    for (script, expected, handle) in [
        ("exit 7", 7, 42424410i64),
        ("kill -9 $$", 128 + 9, 42424411i64),
    ] {
        let child = Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sh");
        let pid = child.id() as i64;
        tt().children.lock().insert(handle, Arc::new(Mutex::new(child)));
        tt().exits.lock().insert(
            handle,
            ExitCache {
                pid,
                exit_code: None,
            },
        );
        let Some(wait) = LinuxPidfdWait::open(tt(), handle) else {
            eprintln!("pidfd_open refused here: skipped");
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        let code = loop {
            if let Some(code) = wait.wait(handle, Duration::from_millis(500)) {
                break code;
            }
            assert!(Instant::now() < deadline, "the pidfd never reported the exit");
        };
        assert_eq!(code, expected, "{script}");
        assert_eq!(try_exit_handle(tt(), handle), Some(expected));
        // Reaped by the waiter: nothing is left for `waitid`.
        // SAFETY: an all-zero `siginfo_t` is a valid out-parameter.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a live local.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG,
            )
        };
        assert_eq!(r, -1, "the child was left a zombie");
        drop(wait);
    }
}

/// Interpreter round i7 wave 136, lane L1b (`CRATONVM_PROCESS_PIDFD_REAPER`):
/// the pidfd reaper waits with the child's mutex free (another thread's
/// `try_lock` succeeds while it waits), answers the exit code, caches it
/// and leaves no zombie. Skipped where `pidfd_open` is refused.
#[cfg(target_os = "linux")]
#[test]
fn i136_l1b_the_pidfd_reaper_waits_without_the_childs_mutex() {
    let handle = 42424420i64;
    let child = Command::new("/bin/sh")
        .args(["-c", "sleep 1; exit 5"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn /bin/sh");
    let pid = child.id() as i64;
    let entry = Arc::new(Mutex::new(child));
    tt().children.lock().insert(handle, Arc::clone(&entry));
    tt().exits.lock().insert(
        handle,
        ExitCache {
            pid,
            exit_code: None,
        },
    );
    if LinuxPidfdWait::open(tt(), handle).is_none() {
        eprintln!("pidfd_open refused here: skipped");
        return;
    }
    let waiter = std::thread::spawn(move || wait_for_handle_pidfd(tt(), handle));
    std::thread::sleep(Duration::from_millis(200));
    assert!(entry.try_lock().is_some(), "the reaper must not hold the child's mutex");
    assert_eq!(try_exit_handle(tt(), handle), None, "still running");
    assert_eq!(waiter.join().expect("the reaper"), Some(5));
    assert_eq!(try_exit_handle(tt(), handle), Some(5), "cached");
    // SAFETY: an all-zero `siginfo_t` is a valid out-parameter.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live local.
    let r = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG,
        )
    };
    assert_eq!(r, -1, "the child was left a zombie");    }

/// End-to-end spawn + waitFor round-trip — no NativeContext needed.
#[test]
fn spawn_and_wait_returns_exit_code() {
    // Use the platform-appropriate "echo" command; `cmd /c echo` on
    // Windows, `/bin/echo` on Linux.  We want a known-good exit.
    #[cfg(target_os = "windows")]
    let mut child = Command::new("cmd")
        .args(["/c", "echo", "hello"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cmd /c echo");
    #[cfg(not(target_os = "windows"))]
    let mut child = Command::new("/bin/echo")
        .arg("hello")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn /bin/echo");

    let handle = 42424242i64;
    let pid = child.id() as i64;
    tt().children.lock().insert(handle, Arc::new(Mutex::new(child)));
    tt().exits.lock().insert(
        handle,
        ExitCache {
            pid,
            exit_code: None,
        },
    );

    let code = wait_for_handle(tt(), handle);
    assert_eq!(code, 0, "echo should exit 0");
    // Second call must be idempotent and return the cached code.
    let code_again = wait_for_handle(tt(), handle);
    assert_eq!(code_again, 0);
}

/// `destroy_handle` signals the kill to the OS.
#[test]
fn destroy_handle_kills_live_child() {
    // Spawn a command that sleeps; we'll kill it.
    #[cfg(target_os = "windows")]
    let child = Command::new("cmd")
        .args(["/c", "ping", "-n", "30", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    #[cfg(not(target_os = "windows"))]
    let child = Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    let Ok(child) = child else {
        // If we can't spawn (e.g. PATH quirks on CI), skip this test.
        eprintln!("skip destroy_handle_kills_live_child — spawn failed");
        return;
    };
    let handle = 42424243i64;
    let pid = child.id() as i64;
    tt().children.lock().insert(handle, Arc::new(Mutex::new(child)));
    tt().exits.lock().insert(
        handle,
        ExitCache {
            pid,
            exit_code: None,
        },
    );

    let killed = destroy_handle(tt(), handle, true);
    assert!(killed, "destroy_handle should succeed for live child");

    // After kill, waitFor returns some exit code (non-zero on
    // Unix; Windows returns 1).
    let _code = wait_for_handle(tt(), handle);
}

#[test]
fn process_wait_for_enters_gc_blocked_region() {
    #[cfg(target_os = "windows")]
    let child = Command::new("cmd")
        .args(["/c", "echo", "x"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    #[cfg(not(target_os = "windows"))]
    let child = Command::new("/bin/echo")
        .arg("x")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");

    let (handle, pid) = install_child_for_test(child);
    let mut ctx = MockNativeContext::new();
    let proc_ref = mock_process(&mut ctx, handle, pid);

    let result = native_process_wait_for(&mut ctx, &[Value::Object(Some(proc_ref))])
        .unwrap()
        .unwrap();
    assert_eq!(result, Value::Int(0));
    assert_eq!(ctx.get_field(proc_ref, PROC_FIELD_EXIT), Value::Int(0));
    // Interpreter round i3 wave 80, lane L6: a child that has already
    // exited answers before any region (the JDK body's `hasExited` fast
    // path), so the count is one region or none, always balanced.
    let (begin, end) = ctx.blocking_region_counts();
    assert!(begin <= 1, "at most one region, got {begin}");
    assert_eq!(begin, end, "blocked-region enter/leave must balance");
}

/// Interpreter round i3 wave 80, lane L6
/// (`i70-L1-synthetic-process-waitfor-ignores-interrupt-20261103.md`):
/// an interrupted waiter on a RUNNING child gets `InterruptedException`
/// (no message) from both stand-ins, with its interrupt status cleared, as
/// from the JDK's `ConditionObject.await` / `awaitNanos`.
#[cfg(not(target_os = "windows"))]
#[test]
fn i80_process_wait_for_throws_when_interrupted_on_a_running_child() {
    let child = Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let (handle, pid) = install_child_for_test(child);
    let mut ctx = MockNativeContext::new();
    let proc_ref = mock_process(&mut ctx, handle, pid);

    let is_ie = |ctx: &MockNativeContext, failed: &MethodCallFailed| match failed {
        MethodCallFailed::ExceptionThrown(exc) => {
            ctx.class_name_of_id(ctx.class_id_of_object(*exc))
                .as_deref()
                == Some("java/lang/InterruptedException")
        }
        _ => false,
    };

    ctx.set_interrupted(true);
    let started = Instant::now();
    let untimed = native_process_wait_for(&mut ctx, &[Value::Object(Some(proc_ref))])
        .expect_err("an interrupted waitFor() must throw");
    assert!(
        is_ie(&ctx, &untimed),
        "waitFor() throws InterruptedException"
    );
    assert!(
        !ctx.is_interrupted(false),
        "the throw consumes the interrupt"
    );

    ctx.set_interrupted(true);
    // Wave 102: a null unit is the JDK bodies' NPE now; a scripted one.
    let unit = mock_millis_unit(&mut ctx, 60_000);
    let timed = native_process_wait_for_timeout(
        &mut ctx,
        &[
            Value::Object(Some(proc_ref)),
            Value::Long(60_000),
            Value::Object(Some(unit)),
        ],
    )
    .expect_err("an interrupted waitFor(60 s) must throw");
    assert!(
        is_ie(&ctx, &timed),
        "waitFor(long, TimeUnit) throws InterruptedException"
    );
    assert!(
        !ctx.is_interrupted(false),
        "the throw consumes the interrupt"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "neither wait may outlast the interrupt"
    );
    let (begin, end) = ctx.blocking_region_counts();
    assert_eq!(begin, end, "blocked-region enter/leave must balance");

    let _ = destroy_handle(tt(), handle, true);
    let _ = wait_for_handle(tt(), handle);
}

/// Interpreter round i4 wave 96, lane L1
/// (`interpreter-i96-L1-windows-processimpl-wait-ignores-interrupt-FIXED-20261128.md`): the
/// Windows `ProcessImpl.waitForInterruptibly` / `waitForTimeoutInterruptibly`
/// natives return at once for an interrupted waiter on a RUNNING child
/// (the Java body then throws), leaving the status for that body to read,
/// and return when the child exits, not at a poll.
#[cfg(windows)]
#[test]
fn i96_windows_processimpl_waits_see_the_interrupt_and_the_exit() {
    let child = Command::new("ping")
        .args(["-n", "30", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ping");
    let (handle, _pid) = install_child_for_test(child);
    let mut ctx = MockNativeContext::new();

    ctx.set_interrupted(true);
    let started = Instant::now();
    native_process_impl_wait_for_interruptibly(&mut ctx, &[Value::Long(handle)])
        .expect("the native never throws");
    native_process_impl_wait_for_timeout(
        &mut ctx,
        &[Value::Long(handle), Value::Long(60_000)],
    )
    .expect("the native never throws");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "an interrupted waiter must not wait for the child"
    );
    assert!(
        ctx.is_interrupted(false),
        "the natives leave the status for the Java body's Thread.interrupted()"
    );
    assert!(try_exit_handle(tt(), handle).is_none(), "the child still runs");

    ctx.set_interrupted(false);
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let _ = destroy_handle(tt(), handle, true);
    });
    let started = Instant::now();
    native_process_impl_wait_for_timeout(
        &mut ctx,
        &[Value::Long(handle), Value::Long(60_000)],
    )
    .expect("the native never throws");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the timed wait returns when the child exits"
    );
    assert!(try_exit_handle(tt(), handle).is_some(), "the child has exited");
    native_process_impl_wait_for_interruptibly(&mut ctx, &[Value::Long(handle)])
        .expect("the native never throws");
    let (begin, end) = ctx.blocking_region_counts();
    assert_eq!(begin, end, "blocked-region enter/leave must balance");
    let _ = killer.join();
}

/// Interpreter round i3 wave 80, lane L6: the `onExit()` waiter carries the
/// name HotSpot's reaper takes for its wait.
#[test]
fn i80_exit_waiter_thread_is_named_like_the_jdk_reaper() {
    assert_eq!(exit_waiter_thread_name(4242), "process reaper (pid 4242)");
    assert_eq!(exit_waiter_thread_name(-1), "process reaper");
}

/// Interpreter round i3 wave 80, lane L6: `exitValue()` on a running child
/// raises the platform's JDK 25 `ProcessImpl` text.
#[test]
fn i80_exit_value_message_is_the_platform_jdks() {
    let want = if cfg!(windows) {
        "process has not exited"
    } else {
        "process hasn't exited"
    };
    assert_eq!(exit_value_not_exited_message(), want);
}

/// Interpreter round i3 wave 80, lane L6: a pre-interrupted `waitFor()` on
/// a child that has already exited answers its exit code on UNIX (the JDK
/// body's `while (!hasExited)` never awaits) and keeps the status set.
#[cfg(not(target_os = "windows"))]
#[test]
fn i80_process_wait_for_on_an_exited_child_ignores_the_interrupt() {
    let child = Command::new("/bin/echo")
        .arg("x")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let (handle, pid) = install_child_for_test(child);
    let _ = wait_for_handle(tt(), handle);
    let mut ctx = MockNativeContext::new();
    let proc_ref = mock_process(&mut ctx, handle, pid);
    ctx.set_interrupted(true);
    let code = native_process_wait_for(&mut ctx, &[Value::Object(Some(proc_ref))])
        .unwrap()
        .unwrap();
    assert_eq!(code, Value::Int(0));
    assert!(
        ctx.is_interrupted(false),
        "no wait, so the status stays set"
    );
}

#[test]
fn process_handle_wait_for_exit_enters_gc_blocked_region() {
    #[cfg(target_os = "windows")]
    let child = Command::new("cmd")
        .args(["/c", "echo", "x"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    #[cfg(not(target_os = "windows"))]
    let child = Command::new("/bin/echo")
        .arg("x")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");

    // The JDK's `ProcessHandleImpl.waitForProcessExit0` is handed a PID.
    // This test used to pass the internal handle, which is what the native
    // used to (wrongly) expect -- so it asserted the bug rather than the
    // contract. See `handle_for_pid`.
    let (_handle, pid) = install_child_for_test(child);
    let mut ctx = MockNativeContext::new();
    let result =
        native_proc_handle_wait_for_process_exit0(&mut ctx, &[Value::Long(pid), Value::Int(0)])
            .unwrap()
            .unwrap();
    assert_eq!(result, Value::Int(0));
    assert_eq!(ctx.blocking_region_counts(), (1, 1));
}

#[cfg(not(target_os = "windows"))]
#[test]
fn process_wait_for_timeout_enters_gc_blocked_region_between_polls() {
    let child = Command::new("/bin/sleep")
        .arg("1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");

    let (handle, pid) = install_child_for_test(child);
    let mut ctx = MockNativeContext::new();
    let proc_ref = mock_process(&mut ctx, handle, pid);

    // Wave 102: a null unit is the JDK bodies' NPE now; a scripted one.
    let unit = mock_millis_unit(&mut ctx, 20);
    let result = native_process_wait_for_timeout(
        &mut ctx,
        &[
            Value::Object(Some(proc_ref)),
            Value::Long(20),
            Value::Object(Some(unit)),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(result, Value::Int(0));
    let (begin, end) = ctx.blocking_region_counts();
    assert!(begin >= 1, "timed wait should enter a blocked region");
    assert_eq!(begin, end, "blocked-region enter/leave must balance");

    let _ = destroy_handle(tt(), handle, true);
    let _ = wait_for_handle(tt(), handle);
}

/// The foreign-receiver timed wait must ALSO nap inside a blocked region.
///
/// `waitFor(long, TimeUnit)` on an application subclass polls the
/// subclass's own `exitValue()`, so it can wait for the full timeout with no
/// subprocess handle in sight. The thread is in `NativeRunning` throughout,
/// which the STW census waits for, so a nap outside a blocked region is a
/// GC pause of the caller's choosing. The first cut of the receiver guard
/// had exactly that bug.
#[test]
fn foreign_receiver_timed_wait_also_enters_a_blocked_region() {
    let mut ctx = MockNativeContext::new();
    // No class name => not one of the VM's process objects, which is the
    // whole point: this is the application-subclass path.
    let foreign = ctx.alloc_object(2);
    assert!(
        !is_vm_process(&mut ctx, foreign),
        "an unnamed mock object must not be mistaken for a VM Process"
    );

    // Wave 102: a null unit is the JDK bodies' NPE now; a scripted one.
    let unit = mock_millis_unit(&mut ctx, 20);
    let result = native_process_wait_for_timeout(
        &mut ctx,
        &[
            Value::Object(Some(foreign)),
            Value::Long(20),
            Value::Object(Some(unit)),
        ],
    )
    .unwrap()
    .unwrap();

    // The mock's `invoke_virtual` has no `exitValue` to run, so the poll
    // never sees an exit and the wait times out — which is the specified
    // answer for a process that has not exited, and the answer the
    // pre-guard code got wrong by reading slot bytes off a stranger.
    assert_eq!(
        result,
        Value::Int(0),
        "a never-exiting process waits out its timeout"
    );
    let (begin, end) = ctx.blocking_region_counts();
    assert!(
        begin >= 1,
        "the foreign-receiver poll must nap inside a blocked region, not spin outside one"
    );
    assert_eq!(begin, end, "blocked-region enter/leave must balance");
}

/// `pid_for_handle` returns the captured pid even after reap.
#[test]
fn pid_for_handle_is_stable_after_wait() {
    #[cfg(target_os = "windows")]
    let child = Command::new("cmd")
        .args(["/c", "echo", "x"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    #[cfg(not(target_os = "windows"))]
    let child = Command::new("/bin/echo")
        .arg("x")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");

    let handle = 42424244i64;
    let pid = child.id() as i64;
    tt().children.lock().insert(handle, Arc::new(Mutex::new(child)));
    tt().exits.lock().insert(
        handle,
        ExitCache {
            pid,
            exit_code: None,
        },
    );
    assert_eq!(pid_for_handle(tt(), handle), pid);
    let _ = wait_for_handle(tt(), handle);
    // pid should still be retrievable post-wait.
    assert_eq!(pid_for_handle(tt(), handle), pid);
}

/// V1: under CWD confinement the spawn gate rejects a bare PATH-resolved
/// program name and an explicit path that escapes the sandbox, while the
/// default (unconfined) profile accepts anything. NUL is always rejected.
#[test]
fn validate_spawn_program_confinement_gate() {
    // Default profile: anything spawns (JDK-faithful single-tenant).
    crate::set_path_confine_to_cwd(false);
    assert!(validate_spawn_program("sh").is_ok());
    assert!(validate_spawn_program("/bin/sh").is_ok());
    assert!(validate_spawn_program(r"C:\Windows\System32\cmd.exe").is_ok());
    // NUL is rejected even with confinement off (host C-string truncation).
    assert!(validate_spawn_program("sh\0-c").is_err());

    // Confined profile: fail closed.
    crate::set_path_confine_to_cwd(true);
    // Bare command name -> PATH-resolved host binary -> rejected.
    assert!(
        validate_spawn_program("sh").is_err(),
        "bare program name must be rejected under confinement"
    );
    assert!(
        validate_spawn_program("cmd").is_err(),
        "bare program name must be rejected under confinement"
    );
    // An explicit absolute path outside the sandbox is rejected by the
    // containment check inside `validate_path`.
    #[cfg(unix)]
    assert!(
        validate_spawn_program("/bin/sh").is_err(),
        "explicit out-of-sandbox path must be rejected under confinement"
    );
    #[cfg(windows)]
    assert!(
        validate_spawn_program(r"C:\Windows\System32\cmd.exe").is_err(),
        "explicit out-of-sandbox path must be rejected under confinement"
    );

    // Restore the global default so we don't leak state to other tests.
    crate::set_path_confine_to_cwd(false);
}

#[test]
fn tokenize_keeps_quoted_paths_intact() {
    // A quoted program path with spaces must stay a single argv[0].
    let argv = tokenize_command_line(r#""C:\Program Files\Java\bin\java.exe" -version"#);
    assert_eq!(
        argv,
        vec![
            r"C:\Program Files\Java\bin\java.exe".to_string(),
            "-version".to_string()
        ]
    );
}

#[test]
fn tokenize_plain_and_quoted_args() {
    assert_eq!(
        tokenize_command_line("prog -cp \"a b\" Main"),
        vec![
            "prog".to_string(),
            "-cp".to_string(),
            "a b".to_string(),
            "Main".to_string()
        ]
    );
    // Embedded "" inside a quoted section -> literal quote.
    assert_eq!(
        tokenize_command_line(r#"prog "say ""hi""""#),
        vec!["prog".to_string(), r#"say "hi""#.to_string()]
    );
    // Empty / whitespace-only input yields no tokens.
    assert!(tokenize_command_line("   ").is_empty());
    // An explicit empty quoted argument is preserved.
    assert_eq!(
        tokenize_command_line(r#"prog """#),
        vec!["prog".to_string(), String::new()]
    );
}
