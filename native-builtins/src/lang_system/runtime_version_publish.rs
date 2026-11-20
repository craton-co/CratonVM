#[allow(unused_imports)]
use super::*;

/// Publish `version` as the singleton and return whichever instance won — a
/// concurrent first call may have published its own, and identity is the whole
/// point of memoising.
///
/// The row lock is held across the global-root lookup and insert (as the old
/// process-wide lock was) so two first calls cannot both publish; neither
/// allocates on the heap nor runs Java.
pub fn runtime_version_publish(ctx: &mut dyn NativeContext, version: ObjectRef) -> ObjectRef {
    let vm = ctx.vm_identity();
    RUNTIME_VERSION_ROOT.with(vm, |slot| {
        if let Some(winner) = slot.and_then(|handle| ctx.resolve_global_root(handle)) {
            return winner;
        }
        let handle = ctx.add_global_root(version);
        // Handle 0 means "no global-root table" (mock contexts): leave the slot
        // empty so the next call rebuilds rather than caching a dead handle.
        if handle != 0 {
            *slot = Some(handle);
        }
        version
    })
}

/// `Runtime.Version.feature()` вЂ” major Java specification version (e.g. 25).
pub fn native_runtime_version_feature(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // `Runtime.Version.parse("8")` produces a fully initialized real JDK
    // Version object (not the lightweight metadata fallback below). Its
    // `version` list is authoritative: returning the host VM feature for it
    // makes `JarFile.baseVersion()` incorrectly report 25, disabling every
    // multi-release lookup in Spring Boot's NestedJarFile.
    if let Ok(this) = obj_arg(args, 0) {
        if let Value::Object(Some(parts)) = ctx.get_field_by_name(this, "version") {
            let parts_pin = ctx.pin_native_root(parts);
            let parts = ctx.read_native_pin(parts_pin, parts);
            let first = ctx.invoke_virtual(parts, "get", "(I)Ljava/lang/Object;", &[Value::Int(0)]);
            ctx.unpin_native_roots(parts_pin);
            if let Ok(Some(Value::Object(Some(first)))) = first {
                let first_pin = ctx.pin_native_root(first);
                let first = ctx.read_native_pin(first_pin, first);
                let value = ctx.invoke_virtual(first, "intValue", "()I", &[]);
                ctx.unpin_native_roots(first_pin);
                if let Ok(Some(Value::Int(value))) = value {
                    return Ok(Some(Value::Int(value)));
                }
            }
        }
    }
    Ok(Some(Value::Int(runtime_feature_version(ctx))))
}

/// The host VM's feature (major) version, from system properties alone.
///
/// This is `Runtime.Version.feature()`'s fallback, factored out so callers that
/// only want the number don't have to build a whole `Runtime$Version` object
/// to ask for it.
pub fn runtime_feature_version(ctx: &dyn NativeContext) -> i32 {
    ctx.get_system_property("java.specification.version")
        .and_then(|s| s.trim().parse::<i32>().ok())
        .or_else(|| {
            ctx.get_system_property("java.version").and_then(|s| {
                let t = s.trim();
                if let Some(rest) = t.strip_prefix("1.") {
                    rest.split('.').next()?.parse().ok()
                } else {
                    t.split('.').next()?.parse().ok()
                }
            })
        })
        .unwrap_or(25)
}

/// `Runtime.Version.build()` - optional build number.
///
/// This override is forced for EVERY `Runtime$Version` receiver, including the
/// fully parsed ones `Runtime.Version.parse(String)` produces, so it must read
/// the real `build` field rather than assume the receiver is a lightweight
/// `Runtime.version()` object. Returning a blanket `Optional.empty()` made a
/// parsed version disagree with itself: `compareTo`/`equalsIgnoreOptional`
/// read the `build` FIELD on the receiver but the `build()` ACCESSOR on the
/// argument, so `v.compareTo(v)` returned 1 for any version with a build
/// number. `Optional.empty()` remains the fallback for a receiver whose field
/// is genuinely absent (synthetic-JDK mode, where the class has no real
/// layout to populate).
pub fn native_runtime_version_build(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Ok(this) = obj_arg(args, 0) {
        if let build @ Value::Object(Some(_)) = ctx.get_field_by_name(this, "build") {
            return Ok(Some(build));
        }
    }
    ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[])
}

pub fn native_runtime_exit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let code = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => match args.first() {
            Some(Value::Int(v)) => *v,
            _ => 0,
        },
    };

    // Mirror native_system_exit: env-gated caller-chain dump and soft-return.
    if crate::nbflags().dbg_exit {
        let trace = ctx.capture_stack_trace();
        let mut rendered = String::new();
        for (i, entry) in trace.iter().take(20).enumerate() {
            use std::fmt::Write as _;
            let _ = write!(
                rendered,
                "\n  #{i} {cls}.{m} (bci={bci})",
                cls = entry.class_name,
                m = entry.method_name,
                bci = entry.byte_code_index,
            );
        }
        tracing::warn!(
            target: "cratonvm::system_exit",
            "[CRATONVM_DBG_EXIT] Runtime.exit({code}) caller chain:{rendered}"
        );
    }

    if crate::nbflags().soft_exit {
        tracing::warn!(
            target: "cratonvm::system_exit",
            "[cratonvm] Runtime.exit({code}) soft-returned (CRATONVM_SOFT_EXIT=1)"
        );
        return Ok(None);
    }

    // B6: Surface Runtime.exit calls so silent shutdowns are visible.
    claim_exit_or_block_forever(ctx);
    // Interpreter round i4 wave 84: HotSpot prints nothing on exit, in either
    // mode, so the line is the exit census's alone (as `native_system_exit`).
    if exit_census_on() || None::<std::ffi::OsString>.is_some() {
        eprintln!("[cratonvm] Runtime.exit({code}) called - process terminating");    }
    // W7-92: see `native_system_exit` for why the hooks run before the sweep.
    run_shutdown_hooks(ctx, "Runtime.exit");
    sweep_declared_slot_maps_before_exit(&*ctx, "Runtime.exit");
    invoke_pre_exit_hook_from(ctx, code);
    // See `native_system_exit` — same reason, same position, same `code`.
    flush_console_streams(&*ctx);
    cratonvm_native_api::process_exit::exit_process(code);
}

/// `java.lang.Shutdown.halt0(int)` — the bottom of `Runtime.halt(int)`.
///
/// Terminates with the requested status and, unlike the `exit` path, does NOT
/// run Java shutdown hooks: `Runtime.halt` is specified as forcible
/// termination, and the JDK runs hooks from `Shutdown.exit`, which halt
/// bypasses. The VM-internal pre-exit hook (staged-archive cleanup, JFR
/// dump-on-exit) still fires — it is not a Java shutdown hook, and its own
/// comment already claims to cover `Runtime.halt`.
///
/// W7-92 made the `exit` paths run hooks and deliberately left this one alone.
/// MEASURED on HotSpot 25.0.3+9: `ShutdownProbe halt` produced no hook output
/// and rc=5, and `HookContract haltinhook` — `Runtime.halt(9)` called from
/// INSIDE a running hook — terminated immediately at rc=9 with the remaining
/// hooks unrun. The absence of a `run_shutdown_hooks` call below is the
/// behaviour, not an oversight; a later sweep tidying the asymmetry away would
/// be a regression.
pub fn native_shutdown_halt0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let code = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    // Same opt-in escape hatch the two `exit` natives honour: turn the
    // termination into a return so whatever the caller was hiding behind it
    // becomes visible. Off by default.
    if crate::nbflags().soft_exit {
        tracing::warn!(
            target: "cratonvm::system_exit",
            "[cratonvm] Runtime.halt({code}) soft-returned (CRATONVM_SOFT_EXIT=1)"
        );
        return Ok(None);
    }

    // Plain ASCII dash on purpose: the neighbouring exit messages carry a
    // mojibake em-dash from an old encoding mishap and print as garbage.
    //
    // SBR5 sysprops: under `--real-jdk` this native is also the bottom of the JDK's own
    // `System.exit` -> `Runtime.exit` -> `Shutdown.exit` -> `Shutdown.halt`, which is
    // retired to bytecode. HotSpot prints nothing on exit, so the line stays
    // `--compatible`'s alone (where the exit natives print their own).
    // Interpreter round i4 wave 84: `--compatible` printed it too, a byte
    // HotSpot never writes (`L6W84DeadlockDetection --compatible` DIFF(1)).
    if exit_census_on() || None::<std::ffi::OsString>.is_some() {
        eprintln!("[cratonvm] Runtime.halt({code}) called - process terminating");    }
    sweep_declared_slot_maps_before_exit(&*ctx, "Runtime.halt");
    invoke_pre_exit_hook_from(ctx, code);
    // NO `run_shutdown_hooks` here, and that is the whole point of `halt`
    // (MEASURED, `HookProbe halt`: `HOOK-OUT h-1 MUST-NOT-APPEAR` does not
    // appear, rc 5). The FLUSH is a different question and HotSpot answers it
    // the other way: `HookProbe haltnoflush` writes an unterminated,
    // unflushed `System.out.print` and then halts, and the bytes are still
    // delivered. So flush, run nothing.
    flush_console_streams(&*ctx);
    cratonvm_native_api::process_exit::exit_process(code);
}

// ---------------------------------------------------------------------------
// Runtime.exec вЂ” spawn subprocesses via std::process::Command
// Process synthetic: 3-field (exit_code=0 Int, stdout=1 String, stderr=2 String)
// ---------------------------------------------------------------------------

/// Consult the installed `java.lang.SecurityManager`, if any, before
/// spawning a host process. Mirrors HotSpot's behaviour: every
/// `ProcessBuilder.start` and `Runtime.exec*` overload must call
/// `SecurityManager.checkExec(command[0])` before the spawn syscall
/// (fork/exec/CreateProcess) is issued. If the SM throws
/// `SecurityException`, the error is propagated to the Java caller and
/// the spawn MUST NOT happen.
///
/// `command_first` is the program path (`command[0]`) as it will be
/// handed to `std::process::Command::new`. An empty string is rejected
/// up-front so a misuse on the SM side (treating `""` as "allow
/// nothing") can't be bypassed by passing an empty argv.
///
/// With no SecurityManager installed this is a no-op вЂ” matching JDK
/// behaviour where `Runtime.exec` is unrestricted until `System.setSecurityManager`
/// is called.
///
/// Audit TODO (Panama): host-call sites that go through `jdk.internal.foreign`
/// / `java.lang.foreign.Linker` can invoke `execve`/`CreateProcessW`
/// without ever transiting `ProcessBuilder.start` or `Runtime.exec`.
/// That bypass is not addressed here вЂ” gating it requires intercepting
/// every Panama downcall, tracked as a separate task. See
/// `native-builtins::panama` for the FFI entry points.
/// Install `check_exec_or_throw` as `native-io`'s pre-spawn policy gate.
///
/// `native-io` owns every real spawn (`ProcessBuilder.start`, `Runtime.exec`,
/// `ProcessImpl.create`, `forkAndExec` all funnel into `spawn_and_wrap`) but
/// cannot reach the SecurityManager, which lives in this crate. This hands it
/// the function pointer. Idempotent, so calling it from more than one
/// registration path is safe.
///
/// Must be called from a path that runs in EVERY mode: the gate is a security
/// control, and a mode that skips the install silently spawns ungated.
pub fn install_spawn_policy_hook() {
    cratonvm_native_io::process::set_spawn_policy_hook(check_exec_or_throw);
}

pub fn check_exec_or_throw(
    ctx: &mut dyn NativeContext,
    command_first: &str,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // Pass ctx so the singleton read re-fetches the CURRENT (post-GC) address
    // via the var-handle-root registry (raw static copies are never remapped).
    if crate::security_manager::get_security_manager(&*ctx).is_none() {
        return Ok(());
    }

    // Allocate a Java String for command[0] and call sm.checkExec(String).
    // The synthetic SecurityManager.checkExec native (security_manager.rs)
    // routes through checkPermission в†’ policy_allows_full_generic; a
    // denial surfaces as RuntimeError::SecurityException, which we
    // propagate verbatim so the Java caller observes a SecurityException
    // and the spawn does NOT happen.
    //
    // `invoke_virtual` takes (receiver, method, descriptor, args) where
    // `args` lists ONLY the explicit method parameters вЂ” the receiver is
    // not duplicated in the args slice (see other call sites such as
    // `AccessController.doPrivileged` in security_manager.rs).
    let cmd_obj = ctx.create_string(command_first);
    // gc-common w16-e: the String's allocation is a GC point, so the manager
    // is read AFTER it (it used to be the receiver read before it).
    let Some(sm_ref) = crate::security_manager::get_security_manager(&*ctx) else {
        return Ok(());
    };
    let args = [Value::Object(Some(cmd_obj))];
    ctx.invoke_virtual(sm_ref, "checkExec", "(Ljava/lang/String;)V", &args)?;
    Ok(())
}

/// Read a String[] from an object reference into a Vec<String>.
///
/// A null element is dropped. Callers that must distinguish "absent" from
/// "null" (`Runtime.exec`'s cmdarray, where HotSpot throws
/// `NullPointerException`) use `checked_cmdarray` instead.
pub fn read_string_array(ctx: &mut dyn NativeContext, arr_val: &Value) -> Vec<String> {
    let arr = match arr_val {
        Value::Object(Some(a)) => *a,
        _ => return Vec::new(),
    };
    let len = ctx.array_length(arr);
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
            result.push(ctx.read_string(s).unwrap_or_default());
        }
    }
    result
}

/// Validate and read a `Runtime.exec` cmdarray the way `ProcessBuilder.start`
/// does: `NullPointerException` for a null array or any null element, then
/// `ArrayIndexOutOfBoundsException` for an empty one (that is `cmdarray[0]`
/// failing, which is why the reported index is 0).
///
/// `read_string_array` DROPS a null element, so `exec(["/bin/echo", null])`
/// used to run a different command line than the caller wrote instead of
/// throwing. Verified against HotSpot 25 in probes/ProcSurfaceProbe.java
/// (T10/T11).
pub fn checked_cmdarray(
    ctx: &mut dyn NativeContext,
    arr_val: &Value,
) -> Result<Vec<String>, MethodCallFailed> {
    let arr = match arr_val {
        Value::Object(Some(a)) => *a,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Runtime.exec: null command array".to_string()),
            }
            .into())
        }
    };
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        match ctx.get_array_element(arr, i) {
            Value::Object(Some(s)) => out.push(ctx.read_string(s).unwrap_or_default()),
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("Runtime.exec: null element in command array".to_string()),
                }
                .into())
            }
        }
    }
    if out.is_empty() {
        return Err(RuntimeError::aioobe(0, 0).into());
    }
    Ok(out)
}

/// Execute a command and hand back a LIVE `Process`.
///
/// Delegates to `native-io`'s `spawn_and_wrap`, the SAME spawn the
/// `ProcessBuilder.start` native uses. That keeps the `std::process::Child`
/// alive in the process table, registers its three pipes in the `FdTable`, and
/// wraps them in a `cratonvm/synthetic/Process` whose layout every
/// `java.lang.Process` native in `native-io` already reads.
///
/// It used to run `Command::output()` here instead, storing the child's whole
/// stdout and stderr as two Java Strings on a 3-slot object allocated under
/// `java/lang/Process`. Three defects fell out of that, and the delegation
/// fixes all three:
///
///   * **The object had nowhere to put those slots.** In real-JDK mode
///     `java.lang.Process` is a REAL loaded class with six fields of its own,
///     so `try_alloc_concurrent_synthetic("java/lang/Process", 3)?` produced a
///     real-layout six-slot object and slots 0..2 aliased `java.lang.Process`'s
///     own reader/writer caches.
///   * **Nothing downstream spoke that layout.** `getInputStream()` and its
///     siblings are answered by `native-io::process`, which reads an
///     fd-carrying layout offset PAST those six real fields. It found nothing,
///     fell through to a pipe stream on fd -1, and every read returned EOF, so
///     a `Runtime.exec` child's output was unreachable. That is what emptied
///     Tomcat's CGI response body (`TestSecurity2019.testCVE_2019_0232`:
///     `rc == 200`, body `null`).
///   * **`exec` blocked until the child exited.** `Runtime.exec` must return
///     immediately with a running child. Blocking meant `isAlive()` was never
///     true and `exitValue()` never threw `IllegalThreadStateException` (the
///     exact signal `CGIServlet` polls on), and a child that reads its stdin
///     could never be fed, because the caller only receives the pipe once
///     `exec` has returned.
///
/// `redirect_error_stream` is false: no `Runtime.exec` overload asks for a
/// merged stream, so stdout and stderr stay separate pipes.
pub fn runtime_spawn_process(
    ctx: &mut dyn NativeContext,
    cmd: &[String],
    env: Option<&[String]>,
    work_dir: Option<&str>,
) -> MethodCallResult {
    if cmd.is_empty() {
        return Err(RuntimeError::aioobe(0, 0).into());
    }
    let program = cmd[0].clone();

    // SECURITY: `SecurityManager.checkExec(command[0])` is NOT called here.
    // `spawn_and_wrap` runs it as its first act, via the policy hook installed
    // by `install_spawn_policy_hook`, so that the gate covers every spawn
    // route rather than only this one. Calling it here as well would consult
    // the SecurityManager twice per `exec` -- observable to any policy that
    // counts or logs checks, and not what HotSpot does.

    // HotSpot: a null `envp` inherits this process's environment unchanged; a
    // non-null one REPLACES it wholesale. `clear_env` carries that
    // distinction, which is why it is derived from the presence of `env` and
    // not from the pair count: an empty-but-present `envp` must give the child
    // an empty environment, not an inherited one.
    let env_pairs: Option<Vec<(String, String)>> = env.map(|vars| {
        vars.iter()
            .filter_map(|var| {
                var.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect()
    });
    let clear_env = env_pairs.is_some();

    cratonvm_native_io::process::spawn_and_wrap(
        ctx,
        &program,
        &cmd[1..],
        work_dir,
        env_pairs.as_deref(),
        clear_env,
        false,
    )
}

/// Read the `File` working-directory argument of the three-arg `exec`
/// overloads.
///
/// Reads `path` BY NAME first: in real-JDK mode `java.io.File`'s slot 0 is not
/// necessarily its `path` field, and the previous slot-0-only read then
/// produced `None` -- a working directory the caller asked for, silently
/// dropped. Same order `native-io::process::file_path_of` uses.
pub fn exec_dir_path(ctx: &mut dyn NativeContext, arg: Option<&Value>) -> Option<String> {
    let file_obj = match arg {
        Some(Value::Object(Some(f))) => *f,
        _ => return None,
    };
    match ctx.get_field_by_name(file_obj, "path") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => match ctx.get_field(file_obj, 0) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
    }
}

/// Runtime.exec(String) вЂ” parse command line split by whitespace.
pub fn native_runtime_exec_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = Runtime instance, args[1] = command string
    // NULL is an NPE and EMPTY is an IllegalArgumentException -- two different
    // doors, and this VM had them swapped and missing respectively.
    //
    // MEASURED in COMPATIBLE mode against HotSpot 25.0.3+9
    // (`probes/Phase1Sweep.java`, the P1-I lane). Strict mode already agreed on
    // both rows, because it drops these `SyntheticStub`s and runs the JDK's own
    // `exec` -- so this is a defect the strict work exposed in the DEFAULT mode:
    //
    //   Runtime.exec("")     HotSpot IllegalArgumentException  CratonVM ArrayIndexOutOfBounds
    //   Runtime.exec(null)   HotSpot NullPointerException      CratonVM IllegalArgumentException
    //
    // The JDK's `exec(String, String[], File)` opens with
    // `if (command.isEmpty()) throw new IllegalArgumentException("Empty
    // command")`, so a null receiver NPEs on `isEmpty()` one instruction before
    // the emptiness test can run. That ordering is the whole of the behaviour.
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
    if parts.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Empty command".to_string(),        }
        .into());
    }
    runtime_spawn_process(ctx, &parts, None, None)}

/// Runtime.exec(String[])
pub fn native_runtime_exec_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arg_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let cmd = checked_cmdarray(ctx, &arg_val)?;
    runtime_spawn_process(ctx, &cmd, None, None)
}

/// Runtime.exec(String, String[])
pub fn native_runtime_exec_string_env(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // NULL is an NPE and EMPTY is an IllegalArgumentException -- two different
    // doors, and this VM had them swapped and missing respectively.
    //
    // MEASURED in COMPATIBLE mode against HotSpot 25.0.3+9
    // (`probes/Phase1Sweep.java`, the P1-I lane). Strict mode already agreed on
    // both rows, because it drops these `SyntheticStub`s and runs the JDK's own
    // `exec` -- so this is a defect the strict work exposed in the DEFAULT mode:
    //
    //   Runtime.exec("")     HotSpot IllegalArgumentException  CratonVM ArrayIndexOutOfBounds
    //   Runtime.exec(null)   HotSpot NullPointerException      CratonVM IllegalArgumentException
    //
    // The JDK's `exec(String, String[], File)` opens with
    // `if (command.isEmpty()) throw new IllegalArgumentException("Empty
    // command")`, so a null receiver NPEs on `isEmpty()` one instruction before
    // the emptiness test can run. That ordering is the whole of the behaviour.
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
    if parts.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Empty command".to_string(),        }
        .into());
    }
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) {
        None
    } else {
        Some(read_string_array(ctx, &env_val))
    };
    runtime_spawn_process(ctx, &parts, env.as_deref(), None)
}

/// Runtime.exec(String[], String[])
pub fn native_runtime_exec_array_env(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arg_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let cmd = checked_cmdarray(ctx, &arg_val)?;
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) {
        None
    } else {
        Some(read_string_array(ctx, &env_val))
    };
    runtime_spawn_process(ctx, &cmd, env.as_deref(), None)
}

/// Runtime.exec(String, String[], File)
pub fn native_runtime_exec_string_env_dir(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // NULL is an NPE and EMPTY is an IllegalArgumentException -- two different
    // doors, and this VM had them swapped and missing respectively.
    //
    // MEASURED in COMPATIBLE mode against HotSpot 25.0.3+9
    // (`probes/Phase1Sweep.java`, the P1-I lane). Strict mode already agreed on
    // both rows, because it drops these `SyntheticStub`s and runs the JDK's own
    // `exec` -- so this is a defect the strict work exposed in the DEFAULT mode:
    //
    //   Runtime.exec("")     HotSpot IllegalArgumentException  CratonVM ArrayIndexOutOfBounds
    //   Runtime.exec(null)   HotSpot NullPointerException      CratonVM IllegalArgumentException
    //
    // The JDK's `exec(String, String[], File)` opens with
    // `if (command.isEmpty()) throw new IllegalArgumentException("Empty
    // command")`, so a null receiver NPEs on `isEmpty()` one instruction before
    // the emptiness test can run. That ordering is the whole of the behaviour.
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
    if parts.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Empty command".to_string(),        }
        .into());
    }
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) {
        None
    } else {
        Some(read_string_array(ctx, &env_val))
    };
    let dir = exec_dir_path(ctx, args.get(3));
    runtime_spawn_process(ctx, &parts, env.as_deref(), dir.as_deref())
}

/// Runtime.exec(String[], String[], File)
pub fn native_runtime_exec_array_env_dir(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arg_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let cmd = checked_cmdarray(ctx, &arg_val)?;
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) {
        None
    } else {
        Some(read_string_array(ctx, &env_val))
    };
    let dir = exec_dir_path(ctx, args.get(3));
    runtime_spawn_process(ctx, &cmd, env.as_deref(), dir.as_deref())
}

// ---------------------------------------------------------------------------
// System.getenv
// ---------------------------------------------------------------------------

pub fn native_system_getenv(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let key_ref = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        // `System.getenv(null)` reaches `ProcessEnvironment.getenv`, which
        // encodes the name before it looks anything up, so the JDK's answer is
        // that method's own helpful NPE rather than a null. Answering null made
        // a caller's bug look like an unset variable, which is the same failure
        // `System.getProperty(null)` was fixed for. The text is a site-specific
        // literal: both halves are fixed by `ProcessEnvironment`'s source.
        // MEASURED by `SystemRuntimeObjectSweep.java`.
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"String.getBytes(java.nio.charset.Charset)\" because \"str\" is null"
                        .to_string(),
                ),
            }
            .into())
        }
    };
    let key_str = ctx.read_string(key_ref).unwrap_or_default();
    match cratonvm_types::flags::runtime_var(&key_str) {
        Ok(val) => {
            let str_obj = ctx.create_string(&val);
            Ok(Some(Value::Object(Some(str_obj))))
        }
        Err(_) => Ok(Some(Value::Object(None))),
    }
}


/// One cell per VM, not per process. `reset_system_singletons` was correct
/// only while VMs were created strictly in sequence — a `Vm::new` on one test
/// thread wiped the cell another live VM was using, and the loser then read
/// back a `Map`/`Properties` object allocated in the other VM's heap. Keyed by
/// `vm_identity` and torn down from `release_vm_native_state`; see
/// `cratonvm_native_api::vm_scoped`.
pub static SYSTEM_ENV: VmScoped<Option<ObjectRef>> = VmScoped::new();
pub static SYSTEM_PROPS: VmScoped<Option<ObjectRef>> = VmScoped::new();

/// The cached no-arg `System.getenv()` Map singleton, if already built.
pub fn system_env_singleton(vm: usize) -> Option<ObjectRef> {
    SYSTEM_ENV.peek(vm, |cell| *cell).flatten()
}

/// Publish `obj` as the `System.getenv()` singleton (double-checked, like
/// [`set_system_props_singleton`]); returns the canonical singleton.
pub fn set_system_env_singleton(vm: usize, obj: ObjectRef) -> ObjectRef {
    SYSTEM_ENV.with(vm, |cell| match *cell {
        Some(existing) => existing,
        None => {
            *cell = Some(obj);
            obj
        }
    })
}

/// Return the OpenJDK-shaped read-only wrapper used by `System.getenv()`.
///
/// The backing object is the real-layout `java/util/HashMap` built below.
/// HotSpot exposes the no-arg environment as a
/// `java.util.Collections$UnmodifiableMap` whose private field `m` points at
/// that backing map. System Rules reflects on that field by name, so the
/// CratonVM unmodifiable-map stand-in deliberately keeps the backing in slot 0,
/// matching the JDK's `m` field slot.
///
/// # Why this asks `java.util.Collections` first (P1-B, 2026-08-12)
///
/// This used to go straight to `try_ensure_synthetic_class(
/// "cratonvm/internal/UnmodifiableMap", 2)`. That stand-in is a compatibility
/// fabrication, so under `--real-jdk` it is refused — correctly. What was wrong
/// was the *caller*: `System.getenv` is registered from
/// `register_essential_natives_with_shims`, so it survives strict mode, runs,
/// asks for a fabricated receiver, and dies as `NoClassDefFoundError:
/// cratonvm/internal/UnmodifiableMap` at the application's call site. Spring
/// takes that in `AbstractEnvironment.<init>`, before bean one.
///
/// The native already holds a REAL `java/util/HashMap`, so the real
/// `java.util.Collections.unmodifiableMap(Map)` can wrap it and there is no
/// need to fabricate anything. `vm_init.rs::ensure_bootstrap_compat_class`
/// claims these stand-ins "exist for the synthetic collection shims, which
/// strict mode does not register" — that premise is FALSE for
/// `UnmodifiableMap`, and this site was the counter-example.
///
/// Per mode, what the `ctx.invoke` below reaches:
///
/// * `--real-jdk`: `native-collections`' `Collections.unmodifiableMap`
///   registration is `NativeKind::SyntheticStub` and is dropped at
///   registration, so the **real `java.base` bytecode** runs and the result is a
///   genuine `java.util.Collections$UnmodifiableMap` — HotSpot's own answer.
/// * `--fast-boot` (default): that same registration wins and
///   `alloc_unmod_wrapper` allocates `cratonvm/internal/UnmodifiableMap` with
///   the backing at slot 0 — bit-for-bit what this function used to build
///   itself, so compatible mode is unchanged.
/// * `--synthetic-jdk`: `phases_early::register_collections_extras_natives`
///   binds the same triple to `native_return_first_arg` and runs later, so the
///   raw `HashMap` comes back. That is the mutable-map degradation below, and it
///   is a pre-existing property of that mode's identity binding
///   (`native-collections/src/lib.rs::wrap_unmodifiable`'s "vacuous-green trap"
///   note documents the same shape for `unmodifiableSet`), not something this
///   change introduces.
///
/// # Boot ordering — `allow_java_call`
///
/// Running Java bytecode from a native is only safe once the class library can
/// actually answer. The caller says whether that holds:
///
/// * The REAL-LAYOUT path passes `true`. It has already resolved every
///   `java/util/HashMap` and `java/util/HashMap$Node` field index off the real
///   classes, so `java.util.HashMap` is loaded and initialized by then, and
///   `java.util.Collections.<clinit>` — three `EMPTY_LIST`/`EMPTY_MAP`/
///   `EMPTY_SET` allocations — cannot need more than that, cannot do I/O, and
///   cannot re-enter `System.getenv()`.
/// * The LEGACY 3-field fallback passes `false`. That arm exists precisely
///   *because* the real `HashMap` layout was not resolvable, i.e. the class
///   library is not usable yet; and a real `Collections$UnmodifiableMap`
///   delegating to a 3-field synthetic map would be worse than the stand-in
///   whose native shims read slot 0.
///
/// # Why it no longer returns `Result`
///
/// If every wrapper is unavailable the answer is the **raw `HashMap`**: a
/// mutable map is far less wrong than an unloadable class, and it keeps
/// `System.getenv()` answering instead of killing the caller. The refusal is
/// still *recorded* — `ClassManager::try_ensure_synthetic_class` records the
/// `CompatibilityClassRequested` violation before returning `Err`, so the
/// `--real-jdk-report` census still sees it; only the throw is dropped.
pub fn wrap_system_env_map(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
    allow_java_call: bool,
) -> ObjectRef {
    let pin = ctx.pin_native_root(map);

    // 1. The real `java.util.Collections.unmodifiableMap(Map)`.
    if allow_java_call
        && ctx
            .ensure_class_initialized("java/util/Collections")
            .is_ok()
    {
        let backing = ctx.read_native_pin(pin, map);
        // A failure here is deliberately swallowed rather than propagated: it
        // means the real wrapper is unavailable, which is what steps 2 and 3
        // are for. Same precedent as
        // `jca::provider_chain::wrap_unmodifiable`, which discards a failed
        // `Collections.unmodifiableSet` and returns the plain set.
        if let Ok(Some(Value::Object(Some(view)))) = ctx.invoke(
            "java/util/Collections",
            "unmodifiableMap",
            "(Ljava/util/Map;)Ljava/util/Map;",
            &[Value::Object(Some(backing))],
        ) {
            ctx.unpin_native_roots(pin);
            return view;
        }
    }

    // 2. The compatibility stand-in. Refused under `--real-jdk`, which is the
    //    whole point of the mode; `try_` rather than the infallible spelling so
    //    the refusal is a value and not a fabrication.
    let stand_in = ctx.try_ensure_synthetic_class("cratonvm/internal/UnmodifiableMap", 2);
    if let Ok(wrapper_class) = stand_in {
        let map = ctx.read_native_pin(pin, map);
        let wrapper = ctx.alloc_object(wrapper_class, 2);
        let map = ctx.read_native_pin(pin, map);
        ctx.set_field(wrapper, 0, Value::Object(Some(map)));
        ctx.unpin_native_roots(pin);
        return wrapper;
    }

    // 3. Degrade to the backing map itself. Mutable where HotSpot's is not —
    //    say so in the log rather than letting it pass silently — but a real
    //    `java.util.HashMap` that every caller can read.
    tracing::warn!(
        "System.getenv(): neither java.util.Collections.unmodifiableMap nor the \
         cratonvm/internal/UnmodifiableMap stand-in was available; returning the \
         backing HashMap, which is MUTABLE unlike HotSpot's"
    );
    let map = ctx.read_native_pin(pin, map);
    ctx.unpin_native_roots(pin);
    map
}

/// The cached `System.getProperties()` `Properties` singleton, if already built.
pub fn system_props_singleton(vm_identity: usize) -> Option<ObjectRef> {
    SYSTEM_PROPS.peek(vm_identity, |cell| *cell).flatten()
}

/// Publish `obj` as the `System.getProperties()` singleton, unless another
/// thread already won the race вЂ” in which case the existing one is returned and
/// `obj` is discarded (it becomes unreachable and is collected). Returns the
/// canonical singleton so all callers converge on one identity.
pub fn set_system_props_singleton(vm_identity: usize, obj: ObjectRef) -> ObjectRef {
    SYSTEM_PROPS.with(vm_identity, |cell| match *cell {
        Some(existing) => existing,
        None => {
            *cell = Some(obj);
            obj
        }
    })
}

/// Replace the cached `System.getProperties()` singleton. Used by
/// `System.setProperties(Properties)`, which HotSpot implements as a global
/// swap of `System.props`, not as a mutation of the previous Properties object.
/// Returns the previous singleton so callers can drop any system-props marker.
pub fn replace_system_props_singleton(
    vm_identity: usize,
    obj: Option<ObjectRef>,
) -> Option<ObjectRef> {
    SYSTEM_PROPS.with(vm_identity, |cell| std::mem::replace(cell, obj))
}

/// GC root scan for the `System.getenv()` / `System.getProperties()` singletons
/// (companion to [`gc_update_system_singleton_refs`]). Mirrors
/// `classloader::gc_scan_loader_singleton_roots`.
pub fn gc_scan_system_singleton_roots(vm_identity: usize, out: &mut Vec<ObjectRef>) {
    out.extend(system_env_singleton(vm_identity));
    out.extend(system_props_singleton(vm_identity));
}

/// Post-GC remap for the system singletons (companion to
/// [`gc_scan_system_singleton_roots`]). Repoints the cached `ObjectRef`s to
/// their relocated addresses after a moving collection.
pub fn gc_update_system_singleton_refs(
    vm_identity: usize,
    pointer_map: &cratonvm_types::PointerMap,
) {
    if pointer_map.is_empty() {
        return;
    }
    let remap = |slot: &mut Option<ObjectRef>| {
        if let Some(obj_ref) = slot.as_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    };
    SYSTEM_ENV.with(vm_identity, remap);
    SYSTEM_PROPS.with(vm_identity, remap);
}

/// Per-VM teardown for the cached system singletons. Called from
/// `release_vm_native_state`.
///
/// This replaces the old `reset_system_singletons()`, which `Vm::new` called
/// to clear a *previous* VM's leftovers. That only worked while VMs were
/// created in sequence: with two live at once it wiped a running VM's cell.
/// A fresh `vm_identity` starts with no row, so there is nothing to reset at
/// construction time any more.
pub fn forget_vm_system_singletons(vm_identity: usize) {
    SYSTEM_ENV.forget(vm_identity);
    SYSTEM_PROPS.forget(vm_identity);
    // Same reason, different table: a `vm_identity` is reused, and a second VM
    // inheriting the first's loaded-library rows would refuse a load the first
    // VM made — an `UnsatisfiedLinkError` naming a loader that no longer
    // exists. See `LOADED_LIBRARIES`.
    LOADED_LIBRARIES.forget(vm_identity);
    // The `System.initPhase1` property snapshot. A reused `vm_identity` must
    // start with no row rather than inherit a previous VM's boot-time values
    // — see `SAVED_SYSTEM_PROPERTIES`.
    SAVED_SYSTEM_PROPERTIES.forget(vm_identity);
    // SBR5 sysprops: the merged-property baseline is per VM too.
    crate::sysprops_merge::forget_vm(vm_identity);
    // gc-common w16-e: the shutdown-hook registry and the `Runtime.version()`
    // singleton handle. Both hold slot numbers in this VM's global-root
    // table, which dies with it.
    SHUTDOWN_STATE.forget(vm_identity);
    RUNTIME_VERSION_ROOT.forget(vm_identity);
    // gci i1/wr: the logging pre-warm arming, per VM since this round.
    LOGGING_PREWARM_ARMED.forget(vm_identity);
}

/// Rust-side twin of `jdk.internal.misc.VM.savedProps`: the system
/// properties as they stood at `System.initPhase1`, captured once per VM so
/// native callers can answer the way `jdk.internal.util.StaticProperty`
/// does — from the boot-time snapshot, not the live store a later
/// `System.setProperty` writes into. See [`saved_system_property`] and
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub static SAVED_SYSTEM_PROPERTIES: VmScoped<std::collections::HashMap<String, String>> =
    VmScoped::new();

/// Take the boot-time snapshot [`SAVED_SYSTEM_PROPERTIES`] serves. Called
/// from `native_system_init_phase1`'s Step 4 — the shared body every
/// registration branch (`--real-jdk`, `--fast-boot`, plain `Compatible`)
/// runs unconditionally — which is what makes this different from
/// `publish_vm_saved_props`: that one is skipped in plain `Compatible`
/// mode (no need to build a live `java.util.HashMap` there, since real
/// bytecode never reads `VM.savedProps` in that mode), but a *native*
/// caller in exactly that mode is why this Rust-side memo exists.
///
/// Whole-map replace, not merge: a second boot of this process (a fresh
/// `vm_identity` reusing the row, as the test harnesses do) must not see a
/// stale key from a previous VM's snapshot survive under a new one.
pub fn capture_saved_system_properties(ctx: &mut dyn NativeContext) {
    let entries = ctx.list_system_properties();
    if entries.is_empty() {
        return;
    }
    let vm = ctx.vm_identity();
    SAVED_SYSTEM_PROPERTIES.with(vm, |map| {
        map.clear();
        map.extend(entries);
    });
}

/// Read `key` out of the real `jdk/internal/misc/VM.savedProps` map, if that
/// static field is already a populated `Map`.
///
/// **Why this has to be checked at all**, and checked FIRST: under
/// `--fast-boot`, `System.initPhase1()`'s own real bytecode runs rather than
/// [`native_system_init_phase1`] -- `register_inner`'s
/// `drop_real_layout_synthetic && class_name == "java/lang/System" &&
/// matches!(method_name, "initPhase1" | "initPhase2" | "initPhase3")` arm
/// deliberately leaves those three uninter­cepted (see
/// `native_vm_save_properties`'s doc comment) so the real bootstrap sequence
/// runs, backed by narrower natives. That means
/// [`capture_saved_system_properties`] -- reached only from inside
/// `native_system_init_phase1` -- never runs under `--fast-boot`, and its
/// Rust-side memo stays empty for the whole life of that VM. But the real
/// bytecode's own call to `VM.saveProperties(props)` IS intercepted (that is
/// what `native_vm_save_properties` is for), so `VM.savedProps` is a real,
/// correctly-populated `Map` by the time any application code runs. MEASURED
/// 2026-09-21: without this, `saved_system_property` fell through to the
/// live store under `--fast-boot` and the `java.io.tmpdir` fix below did
/// nothing in the one mode the defect was found in.
pub fn read_saved_props_map(ctx: &mut dyn NativeContext, key: &str) -> Option<String> {
    let cid = ctx.class_id_by_name("jdk/internal/misc/VM")?;
    let idx = ctx.static_field_index_by_name(cid, "savedProps")?;
    let Value::Object(Some(map)) = ctx.get_static_field(cid, idx) else {
        return None;
    };
    let k = ctx.create_string(key);
    match ctx.invoke_virtual(
        map,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(k))],
    ) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

/// Read `key` the way `jdk.internal.util.StaticProperty` does: a boot-time
/// snapshot, immune to a LATER `System.setProperty` call the way
/// `ctx.get_system_property` (the live store) is not. Two sources feed it,
/// tried in order, because which one is populated depends on the mode:
///
/// 1. [`read_saved_props_map`] — the real `VM.savedProps` `Map`, populated by
///    real `initPhase1` bytecode under `--fast-boot`.
/// 2. [`SAVED_SYSTEM_PROPERTIES`] — the Rust-side twin
///    [`capture_saved_system_properties`] took, populated by *this* native
///    running instead of real bytecode in plain `Compatible` mode.
///
/// Falls back to the live store when neither source has `key` — taken before
/// `System.initPhase1` ran, or a `vm_identity` this process never booted
/// through it at all (the inline unit-test harnesses that build a bare
/// `SharedVm` and never call `initPhase1`).
///
/// This is the fix for `java.io.tmpdir` being read live in compatible mode:
/// `jdk_temp_dir`'s callers used to read `ctx.get_system_property("java.io.tmpdir")`
/// directly, which is the live store and moves on every `System.setProperty`
/// call — a runtime write HotSpot's own `File$TempDirectory` never sees,
/// because it reads `StaticProperty.javaIoTmpDir()`, itself sourced from
/// `VM.savedProps`. Companion to `NativeContext::captured_line_separator`,
/// which solves the identical live-vs-snapshot split for `line.separator` by
/// reading back a Java static field real bytecode always sets; `java.io.tmpdir`
/// (and the rest of `StaticProperty`) has no field of its own to piggyback
/// on, so this reads `VM.savedProps` directly, with the Rust memo as a
/// second source for the one mode that never populates it.
pub fn saved_system_property(ctx: &mut dyn NativeContext, key: &str) -> Option<String> {
    if let Some(v) = read_saved_props_map(ctx, key) {
        return Some(v);
    }
    if let Some(v) = SAVED_SYSTEM_PROPERTIES
        .peek(ctx.vm_identity(), |m| m.get(key).cloned())
        .flatten()
    {
        return Some(v);
    }
    ctx.get_system_property(key)
}

/// The boot-time system properties as the Rust-side snapshot has them
/// (`--compatible`, whose native `initPhase1` took it); empty under
/// `--real-jdk`, where the JDK's own `initPhase1` ran and `VM.savedProps` is the
/// record.
pub fn boot_property_entries(ctx: &mut dyn NativeContext) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = SAVED_SYSTEM_PROPERTIES
        .peek(ctx.vm_identity(), |m| {
            m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        })
        .unwrap_or_default();
    if entries.is_empty() {
        // `--compatible` runs the JDK's own `System.initPhase1` too (the
        // native one never ran, so no Rust-side snapshot): `VM.savedProps` is
        // the record.
        entries = saved_props_map_entries(ctx);
    }
    entries.sort();
    entries
}

/// The `String -> String` entries of the real `jdk.internal.misc.VM.savedProps`
/// map; empty when it is not a populated `Map`. Every object read across a Java
/// call is pinned.
pub fn saved_props_map_entries(ctx: &mut dyn NativeContext) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(vm_cid) = ctx.class_id_by_name("jdk/internal/misc/VM") else {
        return out;
    };
    let Some(idx) = ctx.static_field_index_by_name(vm_cid, "savedProps") else {
        return out;
    };
    let Value::Object(Some(map)) = ctx.get_static_field(vm_cid, idx) else {
        return out;
    };
    let map_pin = ctx.pin_native_root(map);
    let keys = match ctx.invoke_virtual(map, "keySet", "()Ljava/util/Set;", &[]) {
        Ok(Some(Value::Object(Some(set)))) => {
            ctx.invoke_virtual(set, "toArray", "()[Ljava/lang/Object;", &[])
        }
        _ => {
            ctx.unpin_native_roots(map_pin);
            return out;
        }
    };
    let Ok(Some(Value::Object(Some(keys)))) = keys else {
        ctx.unpin_native_roots(map_pin);
        return out;
    };
    let keys_pin = ctx.pin_native_root(keys);
    let len = ctx.array_length(keys);
    for i in 0..len {
        let keys_now = ctx.read_native_pin(keys_pin, keys);
        let Value::Object(Some(key)) = ctx.get_array_element(keys_now, i) else {
            continue;
        };
        let Some(key_text) = ctx.read_string(key) else {
            continue;
        };
        let map_now = ctx.read_native_pin(map_pin, map);
        if let Ok(Some(Value::Object(Some(value)))) = ctx.invoke_virtual(
            map_now,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(key))],
        ) {
            if let Some(value_text) = ctx.read_string(value) {
                out.push((key_text, value_text));
            }
        }
    }
    ctx.unpin_native_roots(map_pin);
    out
}

/// A fresh `java.util.Properties` holding the boot-time system properties, for
/// `System.setProperties(null)` (SBR3 `refmisc`): HotSpot's
/// `System.setProperties(null)` installs `createProperties(SystemProps
/// .initProperties())`, a new object with the properties the VM booted with,
/// not the previous object and not an empty set. Only where the JDK bodies own
/// `System.props` (`--real-jdk`, or the `--compatible` `jdk-concurrent-hash-map`
/// row: `sysprops_merge::jdk_bodies_own_system_props`): the object
/// is built from the real `VM.savedProps` (the map `System.initPhase1` saved);
/// `None` anywhere else or when it cannot be built, and the caller then keeps
/// its previous behaviour. Every reference is re-read through a pin.
pub fn fresh_boot_properties(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    if !crate::sysprops_merge::jdk_bodies_own_system_props(&*ctx) {
        return None;
    }
    let vm_cid = ctx.class_id_by_name("jdk/internal/misc/VM")?;
    let idx = ctx.static_field_index_by_name(vm_cid, "savedProps")?;
    let Value::Object(Some(saved)) = ctx.get_static_field(vm_cid, idx) else {
        return None;
    };
    let saved_pin = ctx.pin_native_root(saved);
    let props = match ctx.new_object_initialized("java/util/Properties", "()V", &[]) {
        Ok(Some(Value::Object(Some(p)))) => p,
        _ => {
            ctx.unpin_native_roots(saved_pin);
            return None;
        }
    };
    let props_pin = ctx.pin_native_root(props);
    let saved = ctx.read_native_pin(saved_pin, saved);
    let props_now = ctx.read_native_pin(props_pin, props);
    let put = ctx.invoke_virtual(
        props_now,
        "putAll",
        "(Ljava/util/Map;)V",
        &[Value::Object(Some(saved))],
    );
    let out = ctx.read_native_pin(props_pin, props);
    ctx.unpin_native_roots(saved_pin);
    put.ok().map(|_| out)
}

/// Record that the BOOT loader holds `name`, so a later app-loader
/// `System.loadLibrary(name)` gets the JDK's "already loaded in another
/// classloader" error instead of a success.
///
/// **THIS HAS NO CALLER IN THE TREE. It is a written-down hand-off, not a live
/// path — do not read its presence as the feature being on.** The one call site
/// it is for is the `jdk/internal/loader/BootLoader.loadLibrary` registration in
/// `native-builtins/src/lib.rs`, which is a deliberate `|_ctx, _args| Ok(None)`
/// no-op (that short-circuit is what stops real `NativeLibraries` bytecode from
/// blocking on the JDK's native-library lock during Linux boot-class `<clinit>`,
/// so it must stay a no-op *for the load*; only the bookkeeping is missing).
/// The patch is one line inside that closure — `BootLoader.loadLibrary(String)`
/// is static, so `args[0]` is the name:
///
/// ```ignore
/// |ctx, args| {
///     if let Some(Value::Object(Some(o))) = args.first() {
///         let name = ctx.read_string(*o).unwrap_or_default();
///         crate::lang_system::record_boot_loader_library(ctx, &name);
///     }
///     Ok(None)
/// }
/// ```
///
/// UNARMED ON PURPOSE, and the reason is measurable rather than cautious:
/// arming it makes `System.loadLibrary("net")` throw for any program that has
/// already reached a `java.net` boot class, which is exactly HotSpot's answer
/// and exactly what `RJdkJni.libraryLoading` depends on NOT happening — its
/// `zip` probe falls through to a `net` probe that must succeed
/// (`RJdkJni.java:189-202`), and `run.sh` compares `CK` lines. Whether CratonVM
/// reaches `BootLoader.loadLibrary("net")` before that line cannot be settled
/// from source; it is one A/B on a built binary in both modes with the HotSpot
/// oracle beside it. Nothing here has been built or run.
///
/// Recording is inert under `Compatible` regardless: nothing reads
/// `LOADED_LIBRARIES` unless a `LoaderScoping::On` registration is in force,
/// and only the strict arm installs one.
pub fn record_boot_loader_library(ctx: &mut dyn NativeContext, name: &str) {
    if name.is_empty() {
        return;
    }
    // Loader id 0 is the bootstrap loader — `NativeContext::loader_id_of_class`'s
    // own encoding, and the loader `BootLoader.loadLibrary` loads on behalf of
    // by definition. Any existing owner is left alone: this native returns void
    // and reports nothing, so there is no shape in which to raise the conflict.
    let _existing_owner = claim_library(ctx.vm_identity(), 0, name);
}

pub fn native_system_getenv_all(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ClassId;

    // Identity: return the cached singleton so `System.getenv() ==
    // System.getenv()` holds (SC-env-classreading RC-A). The process
    // environment is immutable for a running JVM, so the cached snapshot stays
    // correct. Only the real-layout path below caches; the legacy 3-field
    // fallback still returns the OpenJDK-shaped unmodifiable wrapper but is left
    // uncached so a later call retries once the real `java/util/HashMap` layout
    // is resolvable.
    if let Some(cached) = system_env_singleton(ctx.vm_identity()) {
        return Ok(Some(Value::Object(Some(cached))));
    }

    // Build a HashMap with all environment variables.
    //
    // S111r7: previously this routine allocated `java/util/HashMap` with only
    // 3 fields and stored `(buckets, size, capacity)` at slot indices 0/1/2,
    // matching the synthetic layout used by `native-collections::native_map_*`.
    // That works while every Map operation routes through a registered native,
    // but `SystemEnvironmentPropertySource.containsKey` (Spring core) reaches
    // the bytecode interpreter for `HashMap.containsKey -> getNode`, where
    // `getfield #105 // table:[Ljava/util/HashMap$Node;` resolves to the real
    // HashMap layout's slot for `table` вЂ” slot 2 in JDK 25 (AbstractMap
    // inherits `keySet` (0) and `values` (1); HashMap declares `table` next).
    // Reading slot 2 returned `Int(16)` (our synthetic CAPACITY value), then
    // `arraylength` on `Int(16)` produced
    //   `internal error: expected object reference, got int(16)`.
    //
    // Fix: resolve the real HashMap / HashMap$Node field slot indices via
    // `resolve_field_index`, allocate enough field slots to cover the real
    // layout, and populate the object so the interpreter's bytecode getfield
    // sees correct values. Falls back to the legacy synthetic-3-field layout
    // if the real class wasn't loaded (e.g. running before bootstrap completes
    // or against a synthetic stub).
    let hashmap_class_id = ctx
        .ensure_class_initialized("java/util/HashMap")
        .unwrap_or(ClassId::new(0));
    // Best-effort: also load the Node class so its real field layout is known.
    let _ = ctx.ensure_class_initialized("java/util/HashMap$Node");

    // Real-layout slot resolution. Each `Some(idx)` means we know the
    // bytecode interpreter will read that field at `idx`; if every required
    // field is resolvable AND the resolved indices are mutually consistent
    // (no aliasing), we use the real layout; otherwise fall back.
    let f_table = ctx.resolve_field_index("java/util/HashMap", "table");
    let f_size = ctx.resolve_field_index("java/util/HashMap", "size");
    let f_threshold = ctx.resolve_field_index("java/util/HashMap", "threshold");
    let f_loadfactor = ctx.resolve_field_index("java/util/HashMap", "loadFactor");
    let f_entryset = ctx.resolve_field_index("java/util/HashMap", "entrySet");
    let n_hash = ctx.resolve_field_index("java/util/HashMap$Node", "hash");
    let n_key = ctx.resolve_field_index("java/util/HashMap$Node", "key");
    let n_value = ctx.resolve_field_index("java/util/HashMap$Node", "value");
    let n_next = ctx.resolve_field_index("java/util/HashMap$Node", "next");

    let cap = 16usize;
    // Everything from here to the end of the function allocates repeatedly —
    // two strings and a node per environment variable — while holding the
    // bucket array, the map and each freshly built node. Those are all young
    // objects, so their addresses are only valid until the next allocation:
    // hold them in the scope and re-read at every store.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let buckets_obj = scope.new_ref_array(ClassId::new(0), cap);
    let buckets_h = scope.root(buckets_obj);

    // JDK HashMap.hash: (h = key.hashCode()) ^ (h >>> 16). For Strings,
    // hashCode = sum of 31*h + ch. Then bucket index is (n-1) & hash for
    // power-of-two capacity (16 here).
    fn jdk_string_hash(s: &str) -> i32 {
        let mut h: i32 = 0;
        // String.hashCode is per-char (UTF-16 code unit). For ASCII env vars
        // this is identical to per-byte; for non-ASCII fall back to chars.
        for ch in s.chars() {
            h = h.wrapping_mul(31).wrapping_add(ch as i32);
        }
        h ^ ((h as u32 >> 16) as i32)
    }

    if let (
        Some(f_table),
        Some(f_size),
        Some(f_threshold),
        Some(f_loadfactor),
        Some(f_entryset),
        Some(n_hash),
        Some(n_key),
        Some(n_value),
        Some(n_next),
    ) = (
        f_table,
        f_size,
        f_threshold,
        f_loadfactor,
        f_entryset,
        n_hash,
        n_key,
        n_value,
        n_next,
    ) {
        // Allocate enough slots to cover the real layout.
        let map_n_fields = [f_table, f_size, f_threshold, f_loadfactor, f_entryset]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
            + 1;
        let node_n_fields = [n_hash, n_key, n_value, n_next]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
            + 1;
        let map_obj = scope.alloc_object(hashmap_class_id, map_n_fields);
        let map_h = scope.root(map_obj);
        let map = scope.get(&map_h);
        let buckets = scope.get(&buckets_h);
        scope.set_field(map, f_table, Value::Object(Some(buckets)));
        scope.set_field(map, f_size, Value::Int(0));
        // threshold = (int)(capacity * 0.75) for the default load factor.
        scope.set_field(map, f_threshold, Value::Int((cap as i32 * 3) / 4));
        scope.set_field(map, f_loadfactor, Value::Float(0.75));
        scope.set_field(map, f_entryset, Value::Object(None));

        // Fallible since 2026-08-10 (Real-JDK wave 2, step 3). This arm is the
        // REAL-layout path — every field index above came from the real
        // `java.util.HashMap`/`HashMap$Node` — so `HashMap$Node` is already
        // loaded here and the ask resolves to the real class rather than
        // fabricating. The refusal only fires on an image where it is not.
        let node_class_id = crate::util_concurrent_ext::refused_class(
            &mut *scope,
            "java/util/HashMap$Node",
            node_n_fields,
        )?;

        for (key, value) in std::env::vars() {
            let key_str = scope.create_string(&key);
            let key_h = scope.root(key_str);
            let val_str = scope.create_string(&value);
            let val_h = scope.root(val_str);
            let hash = jdk_string_hash(&key);
            // (n-1) & hash, since cap=16 is power of two.
            let idx = ((cap as u32 - 1) & hash as u32) as usize;
            let node = scope.alloc_object(node_class_id, node_n_fields);
            // Nothing below allocates, so one read of each address is enough —
            // but it has to come AFTER the last allocation, not before it.
            let key_obj = scope.get(&key_h);
            let val_obj = scope.get(&val_h);
            let buckets = scope.get(&buckets_h);
            let map = scope.get(&map_h);
            scope.set_field(node, n_hash, Value::Int(hash));
            scope.set_field(node, n_key, Value::Object(Some(key_obj)));
            scope.set_field(node, n_value, Value::Object(Some(val_obj)));
            let existing = scope.get_array_element(buckets, idx);
            scope.set_field(node, n_next, existing);
            scope.set_array_element(buckets, idx, Value::Object(Some(node)));

            let old_size = match scope.get_field(map, f_size) {
                Value::Int(s) => s,
                _ => 0,
            };
            scope.set_field(map, f_size, Value::Int(old_size + 1));
        }

        // Cache the OpenJDK-shaped process-wide singleton (double-checked
        // publish). The wrapper's field 0 is the private `m` backing field that
        // libraries such as System Rules reach via reflection.
        //
        // `true`: this is the real-layout arm, so every `java/util/HashMap`
        // field index above came off the real class and running
        // `java.util.Collections` bytecode here is safe. See
        // `wrap_system_env_map`'s boot-ordering note.
        let map = scope.get(&map_h);
        let env = wrap_system_env_map(&mut *scope, map, true);
        let env = set_system_env_singleton(scope.vm_identity(), env);
        return Ok(Some(Value::Object(Some(env))));
    }

    // Legacy fallback: synthetic 3-field layout for environments where
    // the real HashMap class hierarchy isn't fully resolvable.
    // A failed real-class initialization yields ClassId(0), whose Object layout
    // has zero slots. This fallback writes map and node fields, so both need
    // named synthetic layouts even when the real classes cannot initialize.
    //
    // Fallible since 2026-08-10 (Real-JDK wave 2, step 3): a 3-field synthetic
    // `java/util/HashMap` IS the compatibility substitution — the comment above
    // says as much — so under `--real-jdk` it is refused and `System.getenv()`
    // raises `NoClassDefFoundError` naming the class instead of handing back a
    // map whose layout `java.base`'s own `HashMap` bytecode cannot read.
    let fallback_map_class_id =
        crate::util_concurrent_ext::refused_class(&mut *scope, "java/util/HashMap", 3)?;
    let fallback_node_class_id =
        crate::util_concurrent_ext::refused_class(&mut *scope, "java/util/HashMap$Node", 4)?;
    let map_obj = scope.alloc_object(fallback_map_class_id, 3); // MAP_NUM_FIELDS = 3
    let map_h = scope.root(map_obj);
    let map = scope.get(&map_h);
    let buckets = scope.get(&buckets_h);
    scope.set_field(map, 0, Value::Object(Some(buckets))); // MAP_FIELD_BUCKETS
    scope.set_field(map, 1, Value::Int(0)); // MAP_FIELD_SIZE
    scope.set_field(map, 2, Value::Int(cap as i32)); // MAP_FIELD_CAPACITY

    for (key, value) in std::env::vars() {
        let key_str = scope.create_string(&key);
        let key_h = scope.root(key_str);
        let val_str = scope.create_string(&value);
        let val_h = scope.root(val_str);
        let hash = jdk_string_hash(&key);
        let idx = ((cap as u32 - 1) & hash as u32) as usize;
        let node = scope.alloc_object(fallback_node_class_id, 4); // hash, key, value, next
        let key_obj = scope.get(&key_h);
        let val_obj = scope.get(&val_h);
        let buckets = scope.get(&buckets_h);
        let map = scope.get(&map_h);
        scope.set_field(node, 0, Value::Int(hash));
        scope.set_field(node, 1, Value::Object(Some(key_obj)));
        scope.set_field(node, 2, Value::Object(Some(val_obj)));

        let existing = scope.get_array_element(buckets, idx);
        scope.set_field(node, 3, existing); // next = existing bucket head
        scope.set_array_element(buckets, idx, Value::Object(Some(node)));

        let old_size = match scope.get_field(map, 1) {
            Value::Int(s) => s,
            _ => 0,
        };
        scope.set_field(map, 1, Value::Int(old_size + 1));
    }

    // `false`: this arm was reached BECAUSE the real `java/util/HashMap` layout
    // was not resolvable, so the class library cannot be asked to run
    // `java.util.Collections.unmodifiableMap` — and a real wrapper delegating to
    // a 3-field synthetic map would be worse than the stand-in whose shims read
    // slot 0. Uncached, as before, so a later call retries the real path.
    let map = scope.get(&map_h);
    let env = wrap_system_env_map(&mut *scope, map, false);
    Ok(Some(Value::Object(Some(env))))
}

pub fn native_pb_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
    Ok(None)
}

pub fn native_pb_command(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, 0)))}

// `native_pb_start` lived here: a `ProcessBuilder.start` that ran
// `check_exec_or_throw` and then handed back a one-slot dummy Process which had
// spawned nothing. Its own comment called it "the simplified
// ProcessBuilder.start stub that never actually spawns", kept as
// defense-in-depth for the SecurityManager gate. That gate now lives in
// `native-io`'s `spawn_and_wrap`, where every spawn route meets it, so the stub
// had nothing left to defend -- and its dummy Process was the same
// allocate-under-a-real-JDK-class-name truncation as the rest of this cluster.

/// The component `ClassId` of a `StackTraceElement[]`. `Object` only when the
/// class is not loaded (or loaded under two loaders), the pre-i6 behaviour.
pub fn stack_trace_element_component_id(
    ctx: &mut dyn NativeContext,
) -> cratonvm_types::ClassId {
    crate::lang_class::reflection_component_id(ctx, "java/lang/StackTraceElement")
}

/// The `ClassId` of `StackTraceElement[]` itself, the component of
/// `Thread.dumpThreads`' `StackTraceElement[][]` (i7-L2). Loads the array
/// class (and so `StackTraceElement`) on a miss, so it can run Java code and
/// collect: call it only while no raw reference is live. `Object` when the
/// class cannot be had, the pre-i7 behaviour.
pub fn stack_trace_element_array_class_id(
    ctx: &mut dyn NativeContext,
) -> cratonvm_types::ClassId {
    const NAME: &str = "[Ljava/lang/StackTraceElement;";
    if let Some(id) = ctx.class_id_by_name(NAME) {
        return id;
    }
    if let Ok(Some(Value::Object(Some(mirror)))) = ctx.load_class(NAME) {
        if let Some(id) = ctx.class_id_from_mirror(mirror) {
            return id;
        }
    }
    ctx.class_id_by_name(NAME)
        .unwrap_or_else(|| cratonvm_types::ClassId::new(0))
}

/// Materialize a `java/lang/StackTraceElement[]` from a captured frame trace
/// (innermost frame first, as `getStackTrace()` expects index 0 = current
/// call). Shared by `Thread.getStackTrace0()` and `Thread.dumpThreads()`.
pub fn build_stack_trace_element_array(
    ctx: &mut dyn NativeContext,
    trace: &[cratonvm_native_api::StackTraceEntry],
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    // One element, three strings and possibly a mirror are allocated per frame,
    // so neither the array nor the element can be carried as a raw address
    // across the body of the loop.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    // `StackTraceElement[]`, not `Object[]`: `Thread.asyncGetStackTrace` casts
    // `getStackTrace0()`'s result to `[Ljava/lang/StackTraceElement;`.
    let component = stack_trace_element_component_id(&mut *scope);
    let arr_obj = scope.new_ref_array(component, trace.len());
    let arr_h = scope.root(arr_obj);
    // The captured trace is outermost-first (frame[0] = bottom of stack);
    // getStackTrace()/getAllStackTraces() want index 0 = the innermost (current)
    // call, so materialize reversed вЂ” matching HotSpot ordering.
    for (i, e) in trace.iter().rev().enumerate() {
        let ste =
            crate::try_alloc_concurrent_synthetic(&mut *scope, "java/lang/StackTraceElement", 4)?;
        let ste_h = scope.root(ste);
        let cls_dotted = match scope.class_id_by_name(&e.class_name) {
            Some(cid) => {
                crate::lang_class::dotted_class_name(scope.vm_identity(), cid, &e.class_name)
            }
            // A hidden class keeps its `/0x…` suffix (wave 35).
            None => std::sync::Arc::from(crate::lang_class::dotted_binary_name(&e.class_name)),
        };
        crate::lang_misc::fill_stack_trace_element(
            &mut *scope,
            ste,
            &e.class_name,
            &cls_dotted,
            &e.method_name,
            e.source_file.as_deref(),
            e.line_number,
        );
        let arr = scope.get(&arr_h);
        let ste = scope.get(&ste_h);
        scope.set_array_element(arr, i, Value::Object(Some(ste)));
    }
    Ok(scope.get(&arr_h))
}

/// `Thread.getStackTrace0()` вЂ” the live stack of the receiver thread (or, for
/// another thread, its last-published blocking-deposit snapshot). Returns a
/// `StackTraceElement[]`. Previously stubbed to an empty array.
pub fn native_thread_get_stack_trace(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let component = stack_trace_element_component_id(ctx);
            let arr = ctx.new_ref_array(component, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let trace = ctx.thread_stack_trace(this);
    // `Thread.getStackTrace()` leaves hidden frames out, as HotSpot's
    // `asyncGetStackTrace` does (`--real-jdk`, interpreter round i1 wave 35;
    // `NativeContext::hidden_stack_entries`). `dumpThreads`
    // (`getAllStackTraces`) keeps them, as HotSpot's does: measured,
    // `L4W35OtherThreadHidden`.
    let hidden = ctx.hidden_stack_entries(&trace);
    let mut trace: Vec<_> = if hidden.iter().any(|h| *h) {
        trace
            .into_iter()
            .zip(hidden)
            .filter_map(|(entry, hidden)| (!hidden).then_some(entry))
            .collect()
    } else {
        trace
    };
    // Interpreter round i4 wave 92, lane L2: under `-XX:+ShowHiddenFrames`
    // the listing above keeps every hidden frame that exists, and HotSpot
    // lists also the ones this VM never pushed (a lambda proxy's SAM frame,
    // a served `MethodHandle` call's `LambdaForm` frames): the walkers'
    // stand-ins, spliced INNERMOST first as a walk reads them. Before the
    // depth cap, which counts them as HotSpot does.
    if ctx.show_hidden_frames_flag() && trace.len() >= 2 {
        let innermost_first: Vec<_> = trace.into_iter().rev().collect();
        let (mut spliced, _) =
            crate::lang_stackwalker::add_hidden_standin_frames(ctx, innermost_first, |_| true);
        spliced.reverse();
        trace = spliced;
    }
    // HotSpot's `GetStackTraceHandshakeClosure` lists at most
    // `MaxJavaStackTraceDepth` frames (default 1024, `0` = all), the innermost
    // ones, counted after the hidden-frame skip; this listed every frame
    // (interpreter round i4 wave 84, lane L2). `trace` is OUTERMOST-first.
    // `dumpThreads` (`getAllStackTraces`) dumps the whole stack, as HotSpot's
    // `VM_ThreadDump` with `max_depth == -1` does, and is not capped.
    let max_depth = ctx.max_java_stack_trace_depth();
    if max_depth != 0 && trace.len() > max_depth {
        trace.drain(..trace.len() - max_depth);
    }
    let arr = build_stack_trace_element_array(ctx, &trace);
    Ok(Some(Value::Object(Some(arr?))))
}

// ---------------------------------------------------------------------------
// jdk.internal.vm.ThreadSnapshot.create(Thread) -- JDK 25 native (gci i12/td)
// ---------------------------------------------------------------------------

/// `Thread.State.TERMINATED`'s ordinal.
pub const THREAD_STATE_TERMINATED: i32 = 5;

/// The JVMTI thread-status bits HotSpot stores in `ThreadSnapshot.threadStatus`
/// for a `Thread.State` ordinal: the bits `jdk.internal.misc.VM.toThreadState`
/// decodes back to that same state (`ALIVE` 0x1, `TERMINATED` 0x2, `RUNNABLE`
/// 0x4, `WAITING_INDEFINITELY` 0x10, `WAITING_WITH_TIMEOUT` 0x20,
/// `BLOCKED_ON_MONITOR_ENTER` 0x400). gci i12/td.
pub fn thread_snapshot_status(state_ordinal: i32) -> i32 {
    const ALIVE: i32 = 0x0001;
    const TERMINATED: i32 = 0x0002;
    const RUNNABLE: i32 = 0x0004;
    const WAITING_INDEFINITELY: i32 = 0x0010;
    const WAITING_WITH_TIMEOUT: i32 = 0x0020;
    const BLOCKED_ON_MONITOR_ENTER: i32 = 0x0400;
    match state_ordinal {
        0 => 0,
        1 => ALIVE | RUNNABLE,
        2 => ALIVE | BLOCKED_ON_MONITOR_ENTER,
        3 => ALIVE | WAITING_INDEFINITELY,
        4 => ALIVE | WAITING_WITH_TIMEOUT,
        _ => TERMINATED,
    }
}

/// `ThreadSnapshot$BlockerLockType` ordinals: `PARK_BLOCKER`,
/// `WAITING_TO_LOCK`, `WAITING_ON`.
pub const BLOCKER_PARK: i32 = 0;
pub const BLOCKER_WAITING_TO_LOCK: i32 = 1;
pub const BLOCKER_WAITING_ON: i32 = 2;

/// Which blocker HotSpot's `GetThreadSnapshotHandshakeClosure` records for a
/// thread in `state_ordinal` (gci i12/td): the monitor it is entering when
/// BLOCKED; its `parkBlocker` when parked (WAITING / TIMED_WAITING with a park
/// blocker set: `LockSupport.park` sets it before parking and clears it
/// after); else the monitor it waits on (`Object.wait`). `None` otherwise
/// (running, sleeping, a park with no blocker).
pub fn thread_snapshot_blocker(
    state_ordinal: i32,
    park_blocker: bool,
    monitor: bool,
) -> Option<i32> {
    match state_ordinal {
        2 if monitor => Some(BLOCKER_WAITING_TO_LOCK),
        3 | 4 if park_blocker => Some(BLOCKER_PARK),
        3 | 4 if monitor => Some(BLOCKER_WAITING_ON),
        _ => None,
    }
}

/// `static native ThreadSnapshot create(Thread)` (`jdk.internal.vm.ThreadSnapshot`,
/// JDK 25; HotSpot `JVM_CreateThreadSnapshot` / `ThreadSnapshotFactory`). The
/// JDK 25 `ThreadDumper` -- behind `jcmd Thread.dump_to_file` and
/// `HotSpotDiagnosticMXBean.dumpThreads` -- takes one per thread of the
/// thread containers; with no registration both threw
/// `UnsatisfiedLinkError` (gci i12/td, page
/// `gci-i11vt-virtual-thread-continuation-roots-review-findings` item 13).
///
/// Fills the fields "filled by VM": `name`, `threadStatus`
/// ([`thread_snapshot_status`] of `Thread.getState()`, the same answer every
/// other reader gets, virtual threads included), `carrierThread` (a mounted
/// virtual thread's), `stackTrace`, `locks` (`ThreadLock[]`, the monitors a
/// platform thread holds with the depth of the frame that locked each) and
/// the blocker ([`thread_snapshot_blocker`]). A platform thread's stack and
/// monitors come from the VM's `ThreadInfo` snapshot (`thread_jmx_snapshot`,
/// the frames a HotSpot thread dump lists); a virtual thread's stack from
/// its own `getStackTrace()`. `null` for a terminated thread, as HotSpot.
pub fn native_thread_snapshot_create(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let thread = match args.first() {
        Some(Value::Object(Some(t))) => *t,
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    // Every step below that runs Java or allocates can move the objects held
    // across it; each is rooted here and re-read through its handle.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let thread_h = scope.root(thread);
    let state = match scope.invoke_virtual(thread, "getState", "()Ljava/lang/Thread$State;", &[])? {
        Some(Value::Object(Some(state))) => state,
        _ => return Ok(Some(Value::Object(None))),
    };
    let ordinal = match scope.invoke_virtual(state, "ordinal", "()I", &[])? {
        Some(Value::Int(ordinal)) => ordinal,
        _ => return Ok(Some(Value::Object(None))),
    };
    if ordinal == THREAD_STATE_TERMINATED {
        return Ok(Some(Value::Object(None)));
    }
    let thread = scope.get(&thread_h);
    let is_virtual = matches!(
        scope.invoke_virtual(thread, "isVirtual", "()Z", &[])?,
        Some(Value::Int(v)) if v != 0
    );

    // A platform thread's frames, monitors and contended / waited monitor.
    let thread = scope.get(&thread_h);
    let jmx = if is_virtual { None } else { scope.thread_jmx_snapshot(thread) };
    let mut trace: Option<Vec<cratonvm_native_api::StackTraceEntry>> = None;
    let mut monitor_h: Option<cratonvm_native_api::NativeHandle> = None;
    let mut owned: Vec<(cratonvm_native_api::NativeHandle, i32)> = Vec::new();
    if let Some(snapshot) = jmx {
        monitor_h = snapshot.lock.map(|o| scope.root(o));
        let attributed = snapshot.locked_monitor_depths.len() == snapshot.locked_monitors.len();
        for (i, &object) in snapshot.locked_monitors.iter().enumerate() {
            // A monitor no interpreted frame claims (`-1`, or no attribution)
            // goes to the innermost frame, as the `ThreadInfo` door puts it.
            let depth = if attributed {
                snapshot.locked_monitor_depths[i].max(0)
            } else {
                0
            };
            owned.push((scope.root(object), depth));
        }
        trace = Some(snapshot.stack_trace);
    }

    let thread = scope.get(&thread_h);
    let name_h = match crate::lang_misc::thread_field(&*scope, thread, "name") {
        Value::Object(Some(name)) => Some(scope.root(name)),
        _ => None,
    };
    let thread = scope.get(&thread_h);
    let park_h = match crate::lang_misc::thread_field(&*scope, thread, "parkBlocker") {
        Value::Object(Some(blocker)) => Some(scope.root(blocker)),
        _ => None,
    };
    let carrier_h = if is_virtual {
        let thread = scope.get(&thread_h);
        match scope.get_field_by_name(thread, "carrierThread") {
            Value::Object(Some(carrier)) => Some(scope.root(carrier)),
            _ => None,
        }
    } else {
        None
    };

    let stack = match trace {
        Some(trace) => build_stack_trace_element_array(&mut *scope, &trace)?,
        None => {
            let thread = scope.get(&thread_h);
            match scope.invoke_virtual(thread, "getStackTrace", "()[Ljava/lang/StackTraceElement;", &[])? {
                Some(Value::Object(Some(stack))) => stack,
                _ => {
                    let component = stack_trace_element_component_id(&mut *scope);
                    scope.new_ref_array(component, 0)
                }
            }
        }
    };
    let stack_h = scope.root(stack);

    // `ThreadLock[]`, typeOrdinal 0 (`LOCKED`): this VM eliminates no lock.
    let mut lock_hs: Vec<cratonvm_native_api::NativeHandle> = Vec::with_capacity(owned.len());
    for (object_h, depth) in &owned {
        let lock = match scope.new_object("jdk/internal/vm/ThreadSnapshot$ThreadLock")? {
            Some(Value::Object(Some(lock))) => lock,
            _ => break,
        };
        let object = scope.get(object_h);
        scope.set_field_by_name(lock, "depth", Value::Int(*depth));
        scope.set_field_by_name(lock, "typeOrdinal", Value::Int(0));
        scope.set_field_by_name(lock, "obj", Value::Object(Some(object)));
        lock_hs.push(scope.root(lock));
    }
    let locks_h = match lock_hs.first() {
        Some(first) => {
            let first = scope.get(first);
            let component = scope.class_id_of_object(first);
            let locks = scope.new_ref_array(component, lock_hs.len());
            for (i, h) in lock_hs.iter().enumerate() {
                let lock = scope.get(h);
                scope.set_array_element(locks, i, Value::Object(Some(lock)));
            }
            Some(scope.root(locks))
        }
        None => None,
    };

    let snapshot = match scope.new_object("jdk/internal/vm/ThreadSnapshot")? {
        Some(Value::Object(Some(snapshot))) => snapshot,
        _ => return Ok(Some(Value::Object(None))),
    };
    // No allocation from here on: the handles' addresses stay current.
    let name = name_h.as_ref().map(|h| scope.get(h));
    let carrier = carrier_h.as_ref().map(|h| scope.get(h));
    let stack = scope.get(&stack_h);
    let locks = locks_h.as_ref().map(|h| scope.get(h));
    let park = park_h.as_ref().map(|h| scope.get(h));
    let monitor = monitor_h.as_ref().map(|h| scope.get(h));
    let blocker = thread_snapshot_blocker(ordinal, park.is_some(), monitor.is_some());
    let blocker_object = match blocker {
        Some(BLOCKER_PARK) => park,
        Some(_) => monitor,
        None => None,
    };
    scope.set_field_by_name(snapshot, "name", Value::Object(name));
    scope.set_field_by_name(snapshot, "threadStatus", Value::Int(thread_snapshot_status(ordinal)));
    scope.set_field_by_name(snapshot, "carrierThread", Value::Object(carrier));
    scope.set_field_by_name(snapshot, "stackTrace", Value::Object(Some(stack)));
    scope.set_field_by_name(snapshot, "locks", Value::Object(locks));
    scope.set_field_by_name(snapshot, "blockerTypeOrdinal", Value::Int(blocker.unwrap_or(0)));
    scope.set_field_by_name(snapshot, "blockerObject", Value::Object(blocker_object));
    Ok(Some(Value::Object(Some(snapshot))))
}

// ---------------------------------------------------------------------------
// System.mapLibraryName(String) вЂ” JDK 25 native
// ---------------------------------------------------------------------------

/// Maps a library name to a platform-specific filename.
/// e.g. "foo" в†’ "foo.dll" (Windows), "libfoo.so" (Linux), "libfoo.dylib" (macOS).
pub fn native_system_map_library_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let name = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        // `System.mapLibraryName(null)`: the JNI body throws a NullPointerException
        // with no message. It answered null, a silent value where the JDK raises
        // (`SbrW6System`'s `mapLibraryName null` row; both modes, toward HotSpot).
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let mapped = if cfg!(windows) {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    };
    let result = ctx.create_string(&mapped);
    Ok(Some(Value::Object(Some(result))))
}

// ---------------------------------------------------------------------------
// Thread.sleepNanos0(long) вЂ” JDK 25 native (replaces sleep(long) internally)
// ---------------------------------------------------------------------------

pub fn native_thread_sleep_nanos(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Interpreter round i5 wave 104, lane L6 (`L6W104SleepNanosNegative`):
    // the long as `sleep0` / `Unsafe.park` decode it (a category-2 local may
    // arrive tag-lost as a `Double`), and HotSpot 25's `JVM_SleepNanos`
    // refuses a negative time BEFORE the interrupt check:
    // `IllegalArgumentException("nanosecond timeout value out of range")`.
    // A negative time returned at once here (no JDK caller passes one;
    // reflection can).
    let nanos = match args.first() {
        Some(Value::Long(n)) => *n,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => i64::from(*i),
        _ => 0,
    };
    if nanos < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "nanosecond timeout value out of range".to_string(),
            }
            .into(),
        );
    }
    // Wave 24 (L7): HotSpot's `JVM_SleepNanos` checks the interrupt first,
    // for every duration: a pre-interrupted `Thread.sleep(0)` (JDK 25:
    // `sleep(0)` -> `sleepNanos(0)` -> here) throws "sleep interrupted" and
    // clears the flag. The check used to sit inside `nanos > 0`.
    if ctx.is_interrupted(true) {
        return Err(sleep_interrupted(ctx, false));
    }
    if nanos > 0 {
        // Keycloak Gap 9 localization (CRATONVM_DBG_SLEEP_TRACE): JDK25
        // Thread.sleep(millis) routes through Thread.sleepNanos -> here, so the
        // worker's poll-loop sleeps land in THIS native (not the millis one).
        if crate::nbflags().dbg_sleep_trace {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            static PRINTED: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if n % 16 == 0 && PRINTED.fetch_add(1, Ordering::Relaxed) < 80 {
                let st = ctx.capture_stack_trace();
                let frames: Vec<String> = st
                    .iter()
                    .take(12)
                    .map(|e| format!("{}.{}:{}", e.class_name, e.method_name, e.line_number))
                    .collect();
                eprintln!(
                    "[SLEEP-NANOS-TRACE #{n} nanos={nanos}] {}",
                    frames.join(" <- ")
                );
            }
        }
        let requested_millis = ((nanos as u128) + 999_999) / 1_000_000;
        let effective_millis =
            crate::async_handoff_sleep_millis(requested_millis.min(i64::MAX as u128) as i64);
        let effective_nanos = (effective_millis as u128)
            .saturating_mul(1_000_000)
            .max(nanos as u128)
            .min(u64::MAX as u128) as u64;
        let duration = std::time::Duration::from_nanos(effective_nanos);
        // Interpreter round i3 wave 76, lane L6 (page
        // `i75-L6-proposal-one-sleep-body-20261108`): the one sleep body,
        // `sleep(J)`'s too; this one does not pump the scheduled-task registry
        // (as before; whether it should is the page's open HotSpot question).
        return sleep_body(
            ctx,
            duration,
            nanos,
            // Wave 104 (lane L6): as `sleep(J)`, HotSpot's `blockingOperation`.
            "LockSupport.park",
            false,
        );
    }
    Ok(None)
}

/// The one body of a `Thread.sleep` that blocks (interpreter round i3 wave 76,
/// lane L6; page `i75-L6-proposal-one-sleep-body-20261108`): what
/// `native_thread_sleep` (`sleep(J)`, `--compatible`) and
/// `native_thread_sleep_nanos` (`sleepNanos0`, `--real-jdk`, and the target of
/// `sleep0` and `sleep(JI)`) each had a copy of, so the two modes reach
/// HotSpot's one `JVM_SleepNanos` through the same code. A pure refactor: the
/// callers keep their argument checks, their pre-sleep interrupt check, their
/// trace sampler and their duration (`duration`, after
/// `async_handoff_sleep_millis`); `recorded_nanos` is what each recorded as
/// the requested time of `jdk.ThreadSleep` (`sleep(J)`: the millis in nanos;
/// `sleepNanos0`: the nanos asked for); `pinned_reason` is each one's
/// `jdk.VirtualThreadPinned` reason; `pump` is on for `sleep(J)` only.
///
/// NEW-15.4: a non-pinned virtual thread yields its continuation (or, without
/// a timed yield, releases its carrier permit for the sleep); a pinned one
/// keeps the carrier and emits `jdk.VirtualThreadPinned` per JEP 491.
///
/// WP4.5 (`pump`): while this VM has `ScheduledExecutorService` tasks in the
/// scheduled-task registry, the sleep waits in 10 ms slices and runs them
/// between slices, outside the blocked region (a task runs Java, which a
/// GC-blocked thread must not). Otherwise ONE region for the whole sleep
/// (interpreter round i1 wave 49, lane L1: leaving it every slice made a
/// sleeping thread read RUNNABLE between two slices, and JDWP answered
/// RUNNING where HotSpot says SLEEPING: `L1W47JdiBlockedBeforeAttach`), and
/// the region says it is a sleep (interpreter round i2 wave 63, lane L1,
/// `CRATONVM_PUBLISHED_WAIT_KIND`; `TIMED_WAITING` either way, as the sleep is
/// bounded). Wave 24 (L7): counted down from the start instead of a deadline
/// `Instant` (`Thread.sleep(Long.MAX_VALUE)` overflowed it), with the
/// interrupt polled before every wait. Interpreter round i3 wave 75, lane L6
/// (`CRATONVM_SLEEP_NO_POLL`): each wait is one wait of the remainder that an
/// interrupt ends at once (of the pump's slice while pumping); off, a 10 ms
/// `std::thread::sleep` slice inside the blocked-wait trampoline (JIT round 18
/// wave 10, `CRATONVM_XT_HELPER_UNPARKED_READ`).
///
/// After the sleep the flag is read and cleared, and an interrupt seen during
/// or after it throws HotSpot's `InterruptedException("sleep interrupted")`
/// (wave 24: the unit variant threw it without a message).
pub fn sleep_body(
    ctx: &mut dyn NativeContext,
    duration: std::time::Duration,
    recorded_nanos: i64,
    pinned_reason: &'static str,
    pump: bool,
) -> MethodCallResult {
    let is_virtual = ctx.is_current_virtual();
    let pinned = is_virtual && ctx.vt_pin_count() > 0;
    if pinned {
        ctx.emit_virtual_thread_pinned_jfr(pinned_reason);
    }
    let release = is_virtual && !pinned;
    if release && ctx.vt_sleep_for(duration) {
        return Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::ContinuationYield {
                wake_after_nanos: duration.as_nanos().min(u64::MAX as u128) as u64,
            },
        ));
    }
    if release {
        ctx.vt_release_carrier();
    }
    let sleep_start = std::time::Instant::now();
    let slice = std::time::Duration::from_millis(10);
    let vm = pump.then(|| ctx.vm_identity());
    let mut interrupted = false;
    let mut in_region = false;
    loop {
        let pumping = vm.is_some_and(|vm| crate::scheduled_pump::registry().has_tasks_for(vm));
        if pumping {
            if in_region {
                ctx.end_blocking_region();
                in_region = false;
            }
            crate::scheduled_pump::registry().pump(ctx);
        }
        let remaining = duration.saturating_sub(sleep_start.elapsed());
        if remaining.is_zero() {
            break;
        }
        if ctx.is_interrupted(false) {
            interrupted = true;
            break;
        }
        if !in_region {
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::Sleeping);
            in_region = true;
        }
        let this_slice = remaining.min(slice);
        if !ctx.sleep_wait_unpolled(if pumping { this_slice } else { remaining }) {
            ctx.run_blocked_wait(&mut || std::thread::sleep(this_slice));
        }
    }
    if in_region {
        ctx.end_blocking_region();
    }
    let actual_dur = sleep_start.elapsed();
    if release {
        ctx.vt_acquire_carrier();
    }
    ctx.record_thread_sleep(recorded_nanos, actual_dur.as_nanos() as u64);
    let interrupted_after_sleep = ctx.is_interrupted(true);
    if interrupted || interrupted_after_sleep {
        return Err(sleep_interrupted(ctx, true));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// T19.N2 вЂ” Thread.sleep0(J)V вЂ” JDK 21+ internal sleep native.
// ---------------------------------------------------------------------------
//
// In JDK 21+ the public `Thread.sleep(long millis)` validates the argument
// and then delegates to this private `sleep0(J)V` for the actual sleep +
// interrupt check. `millis` is guaranteed >= 0 by the public caller, but
// HotSpot's native still performs a defensive negative-millis check and
// throws `IllegalArgumentException`, so we mirror that contract.
//
// Implementation notes:
//   * Bounds-check `millis >= 0` BEFORE casting to `u64` (signedв†’unsigned
//     cast of a negative value is a correctness bug вЂ” -1 would become
//     `u64::MAX`).
//   * When `millis == 0`, HotSpot still checks the interrupt status and
//     throws `InterruptedException` if set; no actual blocking happens.
//   * The sleep is `sleep_body`'s (interpreter round i3 wave 76): one wait of
//     the remainder that an interrupt ends at once (`CRATONVM_SLEEP_NO_POLL`,
//     default on), or 10 ms slices polling the interrupt with it off.
//   * The interrupt flag is CLEARED when we throw `InterruptedException`
//     per JDK spec.
pub fn native_thread_sleep0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // WP4.5 -- see `native_thread_sleep`: long args from the operand-stack
    // get re-decoded as Doubles by `CompactValue::to_value()`.
    let raw_nanos: i64 = match args.first() {
        Some(Value::Long(ns)) => *ns,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => *i as i64,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: "sleep0: missing long nanos arg".to_string(),
                }
                .into(),
            );
        }
    };
    if raw_nanos < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "timeout value is negative".to_string(),
            }
            .into(),
        );
    }
    // JDK 21's `Thread.sleep(long millis)` converts to NANOSECONDS and calls
    // the private `sleep0(long nanos)` (JDK 25 renamed it `sleepNanos0`, which
    // is `native_thread_sleep_nanos`). This native used to read the argument as
    // MILLISECONDS, so under `--real-jdk` on a JDK 21 boot image (where the
    // bytecode `Thread.sleep` wins over the `sleep(J)V` native) `sleep(50)`
    // asked for 50,000,000 ms and never returned
    // (`SBR4-SATB-found-while-probing-HELD-20261005.md` item 1). One sleep
    // body, so the interrupt check, the 10 ms slices, the blocked-state
    // region and the `InterruptedException("sleep interrupted")` message are
    // `sleepNanos0`'s, as HotSpot's `JVM_SleepNanos` is the one body of both.
    native_thread_sleep_nanos(ctx, &[Value::Long(raw_nanos)])
}

// ---------------------------------------------------------------------------
// T14 вЂ” System bootstrap chain: initPhase1/2/3
// ---------------------------------------------------------------------------

/// `System.initPhase1()V` вЂ” JDK bootstrap phase 1.
///
/// In the real JDK, this method:
/// 1. Sets up the system properties map (`System.props`)
/// 2. Initializes stdout, stderr, stdin streams
/// 3. Sets `System.lineSeparator`
///
/// Our implementation delegates to NativeContext methods that are already
/// backed by real VM state (system_properties, system_streams). We set
/// the System class's static fields so that subsequent Java code can read
/// `System.out`, `System.err`, `System.in`, and `System.lineSeparator`
/// directly from the static fields.
pub fn native_system_init_phase1(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Step 1: Ensure System class is initialized so static fields exist
    ctx.ensure_class_initialized("java/lang/System")?;

    // Step 3, before the streams: set System.lineSeparator from the
    // line.separator property. JIT round 20, lane walkcompat: the JDK's own
    // `initPhase1` captures it right after the properties, before any stream
    // is built, so a stream step that throws (the launcher then falls back to
    // the synthetic streams) no longer leaves the static null -- which the
    // JDK `System.lineSeparator()` body `--compatible` now runs under the
    // `jdk-print-writer` row (`retired_shadow::is_print_writer_line_separator_row`)
    // would hand to every `println`.
    let line_sep = ctx
        .get_system_property("line.separator")
        .unwrap_or_else(|| if cfg!(windows) { "\r\n" } else { "\n" }.to_string());
    let line_sep_obj = ctx.create_string(&line_sep);
    ctx.set_static_field_by_name(
        "java/lang/System",
        "lineSeparator",
        Value::Object(Some(line_sep_obj)),
    );

    // Step 2: Set System.out and System.err from VM-managed streams.
    // The interpreter already intercepts getstatic on System.out/err,
    // but we also write the static fields so direct heap reads see them.
    // `resolve_field_index` is instance-only; `out`/`err`/`in`/`lineSeparator`
    // are all statics, so we use the static-by-name path (the same one
    // `setIn0`/`setOut0`/`setErr0` use elsewhere in this crate).
    // Helper: build a UTF-8 Charset stub matching the layout used by
    // `Charset.forName` / `Charset.defaultCharset` natives (slot 0 = name String).
    // Keycloak's Picocli.getErrWriter goes `new PrintWriter(System.err)` в†’
    // `PrintWriter(OutputStream, boolean)` which reads `((PrintStream)err).charset()`,
    // a plain getfield on the `charset` field. If that field is null, the
    // downstream `new OutputStreamWriter(stream, charset)` throws NPE("charset")
    // and Quarkus silently exits during command-line parsing. We must therefore
    // stamp a non-null Charset on System.out/err at bootstrap time.
    //
    // charset-NPE fix (2026-05-21): `set_field_by_name` resolves the
    // `charset` slot by walking the receiver's class hierarchy. If the
    // System.out/err object was allocated against the 1-field synthetic
    // `java/io/PrintStream` stub (created before the real class was
    // loaded), that walk finds no `charset` field and silently drops
    // the write вЂ” leaving the field null. `ensure_system_streams` now
    // force-loads the real PrintStream class first, but we also make
    // this helper defensive: it ensures the real `java/io/PrintStream`
    // class is loaded so the `charset` field is resolvable, and it
    // verifies the write actually landed.
    /// The charset name a system stream should carry: its `*.encoding`
    /// property when the launcher or the platform set one, UTF-8 otherwise.
    ///
    /// Reading the PROPERTY rather than calling
    /// `cratonvm_native_api::os_encoding` directly is what makes
    /// `-Dstdout.encoding=…` on the command line work, exactly as it does on
    /// HotSpot: the property table has already absorbed the `-D` overrides by
    /// the time `initPhase1` runs.
    fn std_stream_encoding(ctx: &dyn NativeContext, key: &str) -> String {
        ctx.get_system_property(key)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "UTF-8".to_string())
    }

    fn install_charset(ctx: &mut dyn NativeContext, stream: ObjectRef, encoding: &str) {
        // gc-common w16-e: everything below until the stamp is a GC point
        // (two class initialisations, `Charset.forName` in Java, the stub's
        // allocations), and `stream` used to be stamped through the address
        // it had before them. Rooted here, re-read at the stamp.
        let stream_pin = ctx.pin_native_root(stream);
        install_charset_rooted(ctx, stream_pin, stream, encoding);
        ctx.unpin_native_roots(stream_pin);
    }

    fn install_charset_rooted(
        ctx: &mut dyn NativeContext,
        stream_pin: usize,
        stream: ObjectRef,
        encoding: &str,
    ) {
        // Ensure the real PrintStream class is loaded so `charset` is a
        // resolvable field name. (No-op in pure synthetic-jdk mode.)
        let _ = ctx.ensure_class_initialized("java/io/PrintStream");
        let cs_class = match ctx.ensure_class_initialized("java/nio/charset/Charset") {
            Ok(cid) => cid,
            Err(_) => return,
        };
        // Prefer the REAL charset. `java/nio/charset/Charset` is ABSTRACT, so a
        // hand-allocated instance of it has no `newEncoder()` body and every
        // caller that encodes through this stream dies with
        // `AbstractMethodError` rather than the NPE this helper exists to
        // prevent -- a worse error, one frame further from the cause. See
        // `bug-printstream-charset-answers-the-abstract-base-20260825-FIXED-20260901.md`
        // (retired 2026-09-01 with both of its residuals closed; the stamp
        // below is one of them).
        //
        // This runs during `initPhase1`, which is why the hand-allocated stub
        // was reached for in the first place, so the real call is attempted
        // defensively and the stub stays as the fallback: bootstrap ordering
        // decides which one lands, and neither outcome is worse than today's.
        // `PrintStream.charset()` repairs a stub survivor on first read.
        // `encoding` is the STREAM's encoding, not a constant: HotSpot gives
        // `System.out` the console's charset (`ANSI_X3.4-1968` under `LANG=C`,
        // `Cp1251` on a 1251 Windows host) and only `file.encoding` is pinned
        // to UTF-8. Stamping UTF-8 unconditionally is what made
        // `System.out.print("Ж")` write UTF-8 bytes where HotSpot writes `?`
        // — `printstream_encode` and `install_real_stream_fields` below both
        // read THIS object, so the stamp decides the bytes as well as the
        // answer to `charset()`.
        let resolve = |ctx: &mut dyn NativeContext, name: &str| -> Option<ObjectRef> {
            let want = ctx.create_string(name);
            match ctx.invoke(
                "java/nio/charset/Charset",
                "forName",
                "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
                &[Value::Object(Some(want))],
            ) {
                Ok(Some(Value::Object(Some(real))))
                    if !matches!(
                        ctx.class_name_of_id(ctx.class_id_of_object(real))
                            .as_deref(),
                        Some("java/nio/charset/Charset")
                    ) =>
                {
                    Some(real)
                }
                _ => None,
            }
        };
        let cs_obj = match resolve(ctx, encoding) {
            Some(real) => real,
            // The host named an encoding this image has no charset for (a
            // synthetic-JDK build asked for `cp866`, say). A stand-in that
            // CLAIMS that encoding while everything downstream writes UTF-8
            // would be worse than either honest answer, so degrade the whole
            // stream to UTF-8 rather than the name alone.
            None => match resolve(ctx, "UTF-8") {
                Some(utf8) => utf8,
                None => {
                    // The stub is rooted across its name's allocation.
                    let cs_fields = ctx.class_num_total_fields(cs_class).max(1);
                    let stub = ctx.alloc_object(cs_class, cs_fields);
                    let stub_pin = ctx.pin_native_root(stub);
                    let name = ctx.create_string(encoding);
                    let stub = ctx.read_native_pin(stub_pin, stub);
                    ctx.unpin_native_roots(stub_pin);
                    ctx.set_field(stub, 0, Value::Object(Some(name)));
                    stub
                }
            },
        };
        let stream = ctx.read_native_pin(stream_pin, stream);
        // Use field-by-name so we hit the real-JDK `charset` slot (its
        // declared index differs from any synthetic ordering).
        ctx.set_field_by_name(stream, "charset", Value::Object(Some(cs_obj)));
        // Verify the write landed. If `charset` did not resolve (e.g. the
        // receiver is still a fieldless synthetic stub), the bare
        // `set_field_by_name` above was a no-op and `PrintStream.charset()`
        // would return null в†’ NPE("charset") on the first `new
        // PrintWriter(System.err)`. Fall back to resolving the slot
        // index explicitly against `java/io/PrintStream` and, as a last
        // resort, scan the object's reference slots is unsafe (could
        // clobber `out`), so we only retry the explicit-index path.
        if let Some(idx) = ctx.resolve_field_index("java/io/PrintStream", "charset") {
            if idx < ctx.object_num_fields(stream) {
                ctx.set_field(stream, idx, Value::Object(Some(cs_obj)));
            }
        }
    }

    /// Give `System.out`/`System.err` the three stream fields the REAL
    /// `PrintStream` bytecode needs.
    ///
    /// The synthetic `PrintStream` this VM mints for the system streams has
    /// only `charset` and `autoFlush` populated. MEASURED against HotSpot with
    /// `--add-opens java.base/java.io=ALL-UNNAMED` and reflection on the live
    /// object:
    ///
    /// ```text
    ///               HotSpot                  this VM (before)
    ///   out         BufferedOutputStream     NULL
    ///   charOut     OutputStreamWriter       NULL
    ///   textOut     BufferedWriter           NULL
    ///   charset     sun.nio.cs.UTF_8         sun.nio.cs.UTF_8
    /// ```
    ///
    /// Nothing notices while this VM's own `PrintStream` natives answer: they
    /// write to the host stream directly and never read these fields. The
    /// moment the real bytecode runs, `PrintStream.writeln` calls
    /// `ensureOpen()`, finds `out == null`, throws `IOException`, and catches
    /// it into `trouble = true`:
    ///
    /// ```text
    ///   CRATONVM_ENFORCE_NATIVE_SHADOW=java/io/PrintStream
    ///     System.out.println("A")        prints NOTHING, checkError() == true
    ///     new PrintStream(new FileOutputStream(FileDescriptor.out), true)
    ///                                    prints normally
    /// ```
    ///
    /// **Every write is discarded and the VM exits 0.** That took out twelve of
    /// this lane's thirteen probes at once when the shadow dial was armed over
    /// `java/io/` — each reported `lines=0` and a clean exit, which reads as a
    /// probe that passed. The dial is a supported mode and the retirement
    /// instrument for this whole campaign, so a family that cannot survive
    /// being armed cannot be adjudicated at all.
    ///
    /// Built from the real constructors rather than by hand, so the objects are
    /// whatever the running JDK's own classes are. Every step is defensive: if
    /// anything here is not ready this early in `initPhase1`, the repair is
    /// skipped and the stream is exactly as it was before.
    fn install_real_stream_fields(ctx: &mut dyn NativeContext, stream: ObjectRef, fd_name: &str) {
        // Already wired (a real-JDK-constructed stream, or a second call).
        if matches!(ctx.get_field_by_name(stream, "out"), Value::Object(Some(_))) {
            return;
        }
        // Each `new_object` can move the heap, so everything live across one is
        // pinned and re-read (the native stale-local family). gc-common w16-e:
        // `stream` is pinned BEFORE `FileDescriptor`'s initialisation, whose
        // first run is a `<clinit>` (a GC point); its `charset` used to be read
        // through the pre-initialisation address, and that address pinned.
        let stream_pin = ctx.pin_native_root(stream);
        // `FileDescriptor.out` / `.err` — the same descriptor a hand-written
        // `new FileOutputStream(FileDescriptor.out)` uses, which is the form
        // measured to work under the armed dial.
        let fd = match ctx.ensure_class_initialized("java/io/FileDescriptor") {
            Ok(cid) => match ctx.static_field_index_by_name(cid, fd_name) {
                Some(idx) => match ctx.get_static_field(cid, idx) {
                    Value::Object(Some(fd)) => Some(fd),
                    _ => None,
                },
                None => None,
            },
            Err(_) => None,
        };
        let stream = ctx.read_native_pin(stream_pin, stream);
        let charset = match ctx.get_field_by_name(stream, "charset") {
            Value::Object(Some(cs)) => Some(cs),
            _ => None,
        };
        let (Some(fd), Some(charset)) = (fd, charset) else {
            // zt t7/cl3: the silent skip, said under the print-loss switch.
            if false {
                eprintln!(
                    "[cratonvm] print-state: System.{fd_name} left unwired \
                     (FileDescriptor.{fd_name}={} charset={})",
                    fd.is_some(),
                    charset.is_some()
                );
            }
            ctx.unpin_native_roots(stream_pin);
            return;
        };
        let fd_pin = ctx.pin_native_root(fd);
        let cs_pin = ctx.pin_native_root(charset);

        let built = (|| -> Option<()> {
            let fos = match ctx.new_object("java/io/FileOutputStream") {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => return None,
            };
            let fos_pin = ctx.pin_native_root(fos);
            let fd_now = ctx.read_native_pin(fd_pin, fd);
            let fos_now = ctx.read_native_pin(fos_pin, fos);
            ctx.invoke(
                "java/io/FileOutputStream",
                "<init>",
                "(Ljava/io/FileDescriptor;)V",
                &[Value::Object(Some(fos_now)), Value::Object(Some(fd_now))],
            )
            .ok()?;

            let osw = match ctx.new_object("java/io/OutputStreamWriter") {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => return None,
            };
            let osw_pin = ctx.pin_native_root(osw);
            let fos_now = ctx.read_native_pin(fos_pin, fos);
            let cs_now = ctx.read_native_pin(cs_pin, charset);
            let osw_now = ctx.read_native_pin(osw_pin, osw);
            ctx.invoke(
                "java/io/OutputStreamWriter",
                "<init>",
                "(Ljava/io/OutputStream;Ljava/nio/charset/Charset;)V",
                &[
                    Value::Object(Some(osw_now)),
                    Value::Object(Some(fos_now)),
                    Value::Object(Some(cs_now)),
                ],
            )
            .ok()?;

            let bw = match ctx.new_object("java/io/BufferedWriter") {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => return None,
            };
            let bw_pin = ctx.pin_native_root(bw);
            let osw_now = ctx.read_native_pin(osw_pin, osw);
            let bw_now = ctx.read_native_pin(bw_pin, bw);
            ctx.invoke(
                "java/io/BufferedWriter",
                "<init>",
                "(Ljava/io/Writer;)V",
                &[Value::Object(Some(bw_now)), Value::Object(Some(osw_now))],
            )
            .ok()?;

            // `FilterOutputStream.closeLock` is `private final Object closeLock
            // = new Object()` -- an INSTANCE INITIALISER, and this stream was
            // allocated rather than constructed, so no initialiser ever ran.
            // The other three fields above had to be installed by hand for the
            // same reason; this one was missed because it has no wiring to
            // forget, only an initialiser that did not happen. MEASURED
            // 2026-09-12 by reflection against HotSpot 25: `java.lang.Object`
            // there, `null` here, on `System.out` and `System.err`, in both
            // modes. It is the last live item on the blocked list W7-22 §3
            // wrote for retiring `java/io/PrintStream`.
            let lock = match ctx.new_object("java/lang/Object") {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => return None,
            };
            let lock_pin = ctx.pin_native_root(lock);

            // Publish only once all FOUR exist: a half-wired stream (an `out`
            // with no `textOut`) would turn today's silent discard into an NPE
            // inside `writeln`, which is worse than either endpoint.
            let stream_now = ctx.read_native_pin(stream_pin, stream);
            let fos_now = ctx.read_native_pin(fos_pin, fos);
            let osw_now = ctx.read_native_pin(osw_pin, osw);
            let bw_now = ctx.read_native_pin(bw_pin, bw);
            let lock_now = ctx.read_native_pin(lock_pin, lock);
            ctx.set_field_by_name(stream_now, "out", Value::Object(Some(fos_now)));
            ctx.set_field_by_name(stream_now, "charOut", Value::Object(Some(osw_now)));
            ctx.set_field_by_name(stream_now, "textOut", Value::Object(Some(bw_now)));
            ctx.set_field_by_name(stream_now, "closeLock", Value::Object(Some(lock_now)));
            Some(())
        })();
        // zt t7/cl3: a step that failed leaves the stream unwired, and every
        // later write through the JDK's `PrintStream` bytecode is discarded.
        if built.is_none() && false {
            eprintln!(
                "[cratonvm] print-state: System.{fd_name} left unwired \
                 (a FileOutputStream / OutputStreamWriter / BufferedWriter step failed)"
            );
        }

        ctx.unpin_native_roots(stream_pin);
    }

    /// Stamp one system stream and publish it as `System.<name>`.
    ///
    /// gc-common w16-e: both steps run Java and allocate, and the stream used
    /// to be passed to the second step, and published into the static, through
    /// the address `get_system_stream` returned before either. Rooted across
    /// both and re-read after each.
    fn install_system_stream(
        ctx: &mut dyn NativeContext,
        stream: ObjectRef,
        encoding: &str,
        name: &str,
    ) {
        let stream_pin = ctx.pin_native_root(stream);
        install_charset(ctx, stream, encoding);
        let stream = ctx.read_native_pin(stream_pin, stream);
        install_real_stream_fields(ctx, stream, name);
        let stream = ctx.read_native_pin(stream_pin, stream);
        ctx.unpin_native_roots(stream_pin);
        ctx.set_static_field_by_name("java/lang/System", name, Value::Object(Some(stream)));
    }

    if let Some(out_stream) = ctx.get_system_stream("out") {
        let enc = std_stream_encoding(ctx, "stdout.encoding");
        install_system_stream(ctx, out_stream, &enc, "out");
    }
    if let Some(err_stream) = ctx.get_system_stream("err") {
        let enc = std_stream_encoding(ctx, "stderr.encoding");
        install_system_stream(ctx, err_stream, &enc, "err");
    }
    // S110 вЂ” System.in: wire up to OS stdin (fd id 0 in our FileDescriptorTable,
    // which pre-registers it). The synthetic `Scanner.<init>(InputStream)`
    // native in `native-io/src/lib.rs` reads field 0 of the stream object;
    // when it sees `Value::Int(fd)` it pulls bytes via `fd_table().read_byte`.
    // Allocating a FileInputStream-shaped object with field 0 = Int(0) is
    // therefore enough to make `new Scanner(System.in)` consume real stdin.
    //
    // Without this install, `System.in` was null, so `new Scanner(System.in)`
    // got the empty-string fallback in the native and `nextLine()`/`nextInt()`
    // immediately threw `NoSuchElementException: no more elements`.
    {
        // gc-common w16-e: the stdin object is held in a handle scope. It used
        // to be written through the address it had before the
        // `FileDescriptor` initialisation (a `<clinit>` on first touch) and
        // the descriptor's allocation, and then cached and published through
        // that address too.
        // HotSpot's stdin descriptor reads `handle == -1` on Unix
        // (`FileDescriptor.getHandle(0)`), the stream's handle on Windows
        // (`CRATONVM_FD_FIELDS_PER_PLATFORM`, JIT r23 w1 L6; it was 0, which
        // also left the descriptor indistinguishable from an unpopulated one).
        let stdin_handle = cratonvm_native_io::nio_native::standard_stream_handle(
            0,
            cratonvm_native_io::nio_native::fd_fields_per_platform_enabled(),
        );
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        // Reuse a pinned stdin object if `GETSTATIC System.in` / `ensure_system_stdin_object`
        // materialised it before `initPhase1` (Surefire fork bootstrap).
        let in_h = if let Some(o) = scope.get_system_stream("in") {
            scope.root(o)
        } else {
            let fis_class_id = scope.ensure_class_initialized("java/io/FileInputStream")?;
            let num_fields = scope.class_num_total_fields(fis_class_id).max(2);
            // Both classes are initialised before either object exists.
            let fd_class_id = scope.ensure_class_initialized("java/io/FileDescriptor")?;
            let fd_fields = scope.class_num_total_fields(fd_class_id).max(2);
            let new_in = scope.alloc_object(fis_class_id, num_fields);
            let new_in_h = scope.root(new_in);
            let fd_obj = scope.alloc_object(fd_class_id, fd_fields);
            scope.set_field_by_name(fd_obj, "fd", Value::Int(0));
            scope.set_field_by_name(fd_obj, "handle", Value::Long(stdin_handle));
            let new_in = scope.get(&new_in_h);
            scope.set_field_by_name(new_in, "fd", Value::Object(Some(fd_obj)));
            if !matches!(
                scope.get_field_by_name(new_in, "fd"),
                Value::Object(Some(_))
            ) {
                // Legacy synthetic fallback: encode stdin as `fd + 1` so the
                // reference-slot coercion path cannot collapse fd 0 to null.
                scope.set_field(new_in, 1, Value::Int(1));
            }
            scope.cache_system_stdin(new_in);
            new_in_h
        };
        let in_obj = scope.get(&in_h);
        let fd_obj = match scope.get_field_by_name(in_obj, "fd") {
            Value::Object(Some(fd_obj)) => fd_obj,
            _ => {
                let fd_class_id = scope.ensure_class_initialized("java/io/FileDescriptor")?;
                let fd_fields = scope.class_num_total_fields(fd_class_id).max(2);
                let fd_obj = scope.alloc_object(fd_class_id, fd_fields);
                let in_obj = scope.get(&in_h);
                scope.set_field_by_name(in_obj, "fd", Value::Object(Some(fd_obj)));
                fd_obj
            }
        };
        scope.set_field_by_name(fd_obj, "fd", Value::Int(0));
        scope.set_field_by_name(fd_obj, "handle", Value::Long(stdin_handle));
        let in_obj = scope.get(&in_h);
        scope.cache_system_stdin(in_obj);
        scope.set_static_field_by_name("java/lang/System", "in", Value::Object(Some(in_obj)));
    }

    // Step 3 (`System.lineSeparator`) runs first, right after step 1: see
    // there.

    // Step 4: snapshot the system properties the way the real `initPhase1`'s
    // `VM.saveProperties` does, for native callers that need the boot-time
    // answer rather than the live store -- see `saved_system_property`.
    capture_saved_system_properties(ctx);

    Ok(None)
}

/// `System.initPhase2(ZZ)I` вЂ” JDK bootstrap phase 2 (module system).
///
/// In the real JDK, this initializes the module system graph. Our VM
/// handles modules synthetically (all classes are in the unnamed module),
/// so we return 0 (JNI_OK) to indicate success.
///
/// Parameters: (boolean printToStderr, boolean printStackTrace)
/// Returns: int (0 = success, non-zero = failure)
pub fn native_system_init_phase2(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Module system is handled synthetically вЂ” report success
    Ok(Some(Value::Int(0)))
}

/// `System.initPhase3()V` вЂ” JDK bootstrap phase 3 (class loader hierarchy).
///
/// In the real JDK, this sets up the platform and application class loaders.
/// Our VM uses a flat class loading model, so this is a no-op.
pub fn native_system_init_phase3(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// T14 вЂ” jdk/internal/misc/VM natives
// ---------------------------------------------------------------------------

/// `VM.getSavedProperty(String)String` вЂ” return a saved VM property.
///
/// The real JDK saves certain system properties during early bootstrap
/// before `System.initPhase1` runs. Our implementation delegates to
/// the same property store used by `System.getProperty`.
pub fn native_vm_get_saved_property(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let key = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    match ctx.get_system_property(&key) {
        Some(value) => {
            let s = ctx.create_string(&value);
            Ok(Some(Value::Object(Some(s))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `VM.saveProperties(Map)V` вЂ” the setter half of `getSavedProperty`'s field,
/// never registered until now.
///
/// Real `initPhase1()V`'s own bytecode is deliberately left unintercepted
/// under a real-JDK-backed image (`register_inner`'s
/// `drop_real_layout_synthetic && class_name == "java/lang/System" &&
/// matches!(method_name, "initPhase1" | "initPhase2" | "initPhase3")` arm)
/// so the real JDK's own bootstrap sequence runs, supported by narrower
/// natives -- this is that support for its very first substantive
/// instruction. `javap -c java.lang.System`: `initPhase1` opens with
/// `setJavaLangAccess()`, `SystemProps.initProperties()`, `VersionProps.init()`,
/// then at bci 12, `invokestatic jdk/internal/misc/VM.saveProperties`.
///
/// Real `saveProperties`'s own body (`javap -c jdk.internal.misc.VM`) is
/// `if (initLevel() != 0) throw new IllegalStateException("Wrong init
/// level"); savedProps = props;` -- a precondition this VM used not to
/// satisfy: `VM.initLevel()`'s native (`native-builtins/src/lib.rs`,
/// `phases_early.rs`) used to floor-clamp to 2, which made
/// `initLevel() != 0` permanently true, so real `saveProperties` threw
/// EVERY time real `initPhase1` reached it, before `VM.savedProps` was ever
/// populated. (j27: over a real JDK `initLevel()` now answers the JDK's
/// static -- the JDK body under the `jdk-vm-init-level` HotSpot-alignment
/// row, lane FBTAIL's static-reading native with the row off -- and only the
/// synthetic JDK keeps the floor. This native still skips the check and the
/// `directMemory` / `pageAlignDirectMemory` writes the real body makes.) `jdk/internal/loader/ClassLoaders`'s own `<clinit>` (via
/// `BootLoader.getServicesCatalog()`) is what actually reads that field back
/// during `ModuleLayer.boot()`, so the failure surfaces there as
/// `IllegalStateException: Not yet initialized` -- a different message for
/// the same root cause, and permanent per JVMS 5.5 once `ClassLoaders`
/// fails its own `<clinit>` once.
///
/// The fix is the same shape as `getSavedProperty`'s own native two
/// registrations up: skip the level check entirely and just do what the
/// real setter's last statement does, `savedProps = props`, taking the
/// `Map` real bytecode just built (`SystemProps.initProperties()`'s result)
/// rather than reconstructing one from this VM's own property table --
/// `saveProperties` is a pure assignment, so there is nothing to get wrong
/// by using the caller's own argument.
pub fn native_vm_save_properties(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let props = match args.first() {
        Some(v @ Value::Object(Some(_))) => v.clone(),
        _ => return Ok(None),
    };
    // Initialise `VM` BEFORE the store, not after. Native dispatch does not run a
    // static method's class initialiser (JVMS 5.5) the way an interpreted
    // `invokestatic` would, and this native is the first thing to touch `VM`
    // during `initPhase1`: MEASURED on `--real-jdk`, `is_class_initialized`
    // answers false here, the store lands in the pre-`<clinit>` static block, and
    // `VM.<clinit>` then resets it -- so `savedProps` read back `null` on the first
    // `ClassLoaders.<clinit>` (`VM.getSavedProperty` -> "Not yet initialized"),
    // which JVMS 5.5 makes permanent. Every `ServiceLoader`/`java.util.logging`
    // user in the process then died with `NoClassDefFoundError: ClassLoaders`.
    //
    // gc-common w19-e: `VM.<clinit>` is Java, and the map (from `args`) was
    // stored at its address from before it. Rooted across it.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let props_h = crate::lang_misc::w19e_root_value(&mut scope, props);
    let _ = scope.ensure_class_initialized("jdk/internal/misc/VM");
    let props = crate::lang_misc::w19e_rooted_value(&scope, props, props_h.as_ref());
    scope.set_static_field_by_name("jdk/internal/misc/VM", "savedProps", props);
    Ok(None)
}

/// `VM.getRuntimeArguments()[String` вЂ” return the VM runtime arguments.
///
/// Returns an empty String array (we don't expose internal runtime args).
pub fn native_vm_get_runtime_arguments(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // `String[]`, the declared type (i6-L2).
    let component = crate::lang_class::reflection_component_id(ctx, "java/lang/String");
    let arr = ctx.new_ref_array(component, 0);
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// T15 вЂ” Remaining missing natives
// ---------------------------------------------------------------------------

/// `java/lang/ref/Finalizer.register(Object)V`
///
/// Registers an object for finalization with the VM's own reference processor
/// (this VM finalizes through it; the JDK's `Finalizer` queue stays empty).
///
/// gc-common w5-d (2026-09-24), `handoff-w2d-finalizer-registration-native`
/// item 2: this used to call `discover_reference(3, obj, obj, None)`, and wire
/// `3` is `ReferenceType::Cleaner` (`vm_exec.rs::discover_reference`), not
/// `Finalizer` — a registration that reached it was neither resurrected nor
/// finalized, only screened out later as a malformed cleaner action. It now
/// asks the VM to register a `Finalizer` row, and only for a class that
/// overrides `finalize()`, which is `Finalizer.register`'s own precondition.
/// Nothing in the VM calls `Finalizer.register` for an object it already
/// registered at allocation, so this adds no second row.
pub fn native_finalizer_register(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = the object to register for finalization
    if let Some(Value::Object(Some(obj))) = args.first() {
        ctx.register_if_finalizable(*obj);
    }
    Ok(None)
}

/// `java/lang/ref/Finalizer.isFinalizationEnabled()Z`
///
/// HotSpot's `JVM_IsFinalizationEnabled`: the launcher's
/// `--finalization=enabled|disabled` (JEP 421). zt t18/hunt2: this VM has the
/// switch now (`VmConfig::finalization_enabled`, per VM); it answered `true`
/// unconditionally and the launcher refused the option. `Finalizer.<clinit>`
/// reads it once into `ENABLED`, so a `false` here is also why no `Finalizer`
/// thread starts, as on HotSpot. The class manager gives no class a finalizer
/// in that mode, so `native_finalizer_register` is never reached for one.
pub fn native_finalizer_is_finalization_enabled(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(i32::from(ctx.finalization_enabled()))))
}

/// `java/lang/reflect/Array.newArray(Class<?> componentType, int length) в†’ Object`
///
/// Allocates a new array with the given component type and length.
/// This is an alias for `Array.newInstance` but with a different name used
/// internally by the JDK reflection framework.
pub fn native_array_new_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = component type (Class mirror), args[1] = length
    //
    // HotSpot's `Reflection::reflect_new_array` checks, in this order (probe
    // `L4W29ReflectNewArray.java`, rows 1-8): a null
    // component is a `NullPointerException` (before the length), a negative
    // length is `NegativeArraySizeException`, and `void` or an array class
    // that already has 255 dimensions is an `IllegalArgumentException` with
    // no message. Under `--real-jdk` this native is what `Array.newInstance`
    // reaches (the JDK's own `newInstance` body calls it), and it used to
    // allocate an `Object[]` for a null component and a bogus array for
    // `void`; the compatible-mode `newInstance` native made the same checks
    // itself (`reflect_array_component_ok`), so it never gets here with them.
    if !matches!(args.first(), Some(Value::Object(Some(_)))) {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    }
    let length = match args.get(1) {
        Some(Value::Int(n)) => {
            if *n < 0 {
                return Err(RuntimeError::NegativeArraySizeException { size: *n }.into());            }
            *n as usize        }
        _ => 0,
    };

    // Determine element type from the Class mirror.
    //
    // `Array.newInstance(Class,int)` / `Arrays.copyOf(.., Class)` delegate here
    // (`reflect/Array.newArray`). The component Class is frequently a
    // *synthesized array-class mirror* (e.g. `[Ljava/lang/String;` from
    // `String[][].class.getComponentType()`), whose name is only recoverable
    // via the VM reverse-map. The previous `read_string`/slot-1 read returned
    // nothing for those mirrors and defaulted to `java/lang/Object`, so
    // `rows.toArray(new Value[0][])` (H2 SortOrder.sort) allocated a bare
    // `Object[]` and CCE'd on the `(Value[][])` checkcast. Resolve via
    // `mirror_class_name` first (handles array + ordinary classes), keeping the
    // old readers as a fallback for legacy/unit-test mirrors.
    let comp_mirror = match args.first() {
        Some(Value::Object(Some(mirror))) => Some(*mirror),
        _ => None,
    };
    // JIT round 23 wave 2 (proxy-define page;
    // `CRATONVM_REFLECT_NEW_ARRAY_ONE_RESOLVE`, default ON): the reverse map
    // answers the mirror's `ClassId` once. `mirror_class_name` asks it first
    // and `mirror_class_id` below asks it again, so when it answers (with a
    // name) the result is the same `(name, id)` pair the two lookups give.
    // Wave 2e: the name is the class record's own `Arc<str>`
    // (`class_name_arc_of_id`, the same text `class_name_of_id` copies), so a
    // reference component costs no `String` at all.
    let one_resolve = crate::nbflags().reflect_new_array_one_resolve;
    let resolved: Option<(std::sync::Arc<str>, cratonvm_types::ClassId)> = match comp_mirror {
        Some(mirror) if one_resolve => ctx.class_id_from_mirror(mirror).and_then(|cid| {
            ctx.class_name_arc_of_id(cid)
                .filter(|s| !s.is_empty())
                .map(|name| {
                    let name = if name.contains('.') {
                        std::sync::Arc::from(name.replace('.', "/"))                    } else {
                        name                    };
                    (name, cid)                })        }),
        _ => None,
    };
    let (comp_name, resolved_id): (std::sync::Arc<str>, _) = match resolved {
        Some((name, cid)) => (name, Some(cid)),
        None => (std::sync::Arc::from(comp_name_slow(&*ctx, comp_mirror)), None),
    };

    if crate::nbflags().dbg_toarray_ok {
        eprintln!(
            "[DBG_TOARRAY] newArray comp_name={:?} len={}",
            comp_name, length
        );
    }

    // Map primitive type names to ArrayElementType. The human name only:
    // `mirror_class_name` answers `int`, never the descriptor `I`, and a class named
    // `C` or `B` (a Groovy script's, a one-letter test class) is not `char`/`byte`.
    // gc-common w8-b (`common-w3c-native-callback-allocations-cannot-collect`,
    // route 1): the array is the LAST thing this native does, and it holds no
    // heap reference past this point (the component mirror is read above and
    // never again), so the allocation may reclaim and retry before the
    // `OutOfMemoryError`, exactly as `anewarray` would. `live` is empty.
    // This is the genuine JDK native behind `Array.newInstance(Class, int)`,
    // live in every mode (a `Bridge`).
    use cratonvm_types::ArrayElementType as Elem;
    let primitive = match &*comp_name {
        "int" => Some(Elem::Int),
        "long" => Some(Elem::Long),
        "float" => Some(Elem::Float),
        "double" => Some(Elem::Double),
        "boolean" => Some(Elem::Boolean),
        "byte" => Some(Elem::Byte),
        "char" => Some(Elem::Char),
        "short" => Some(Elem::Short),
        _ => None,
    };
    let arr = match primitive {
        Some(elem) => crate::new_array_reclaiming(ctx, elem, length, &mut [], "Array.newInstance")?,
        None => {
            if reflect_new_array_refuses_component(&comp_name, 1) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: String::new(),
                }
                .into());
            }
            // Reference array вЂ” resolve the component class. `ensure_class_initialized`
            // synthesizes array-descriptor components (`[L...;`) on demand, so a
            // multi-dimensional template yields the correct nested array type.
            //
            // Prefer the component mirror's OWN ClassId (`mirror_class_id`,
            // reverse-map lookup) over re-resolving by name:
            // `ensure_class_initialized` has no loader context here, so for
            // a class name registered under more than one ClassId it can
            // silently pick the wrong one. Observed for Jackson's
            // `KeyDeserializers` interface in the AOT `web.service.registry`
            // suite: `old.getClass().getComponentType()` correctly names the
            // interface, but re-resolving that name here returned a SECOND,
            // distinct ClassId for the "same" class, so the freshly
            // allocated array's component type disagreed with the ClassId
            // already tagging `old`'s elements and `System.arraycopy`
            // correctly rejected the mismatch with `ArrayStoreException`.
            // Falling back to the name-based lookup only when the mirror
            // isn't in the reverse map at all (synthesized/legacy mirrors)
            // preserves the multi-dimensional-array-descriptor synthesis
            // this function otherwise relies on `ensure_class_initialized`
            // for.
            let comp_id = resolved_id
                .or_else(|| {
                    comp_mirror.and_then(|m| crate::lang_class::mirror_class_id(&*ctx, m))
                })
                .or_else(|| ctx.ensure_class_initialized(&comp_name).ok())
                .unwrap_or(cratonvm_types::ClassId::new(0));
            crate::new_ref_array_reclaiming(ctx, comp_id, length, &mut [], "Array.newInstance")?
        }
    };
    Ok(Some(Value::Object(Some(arr))))
}

/// `Array.newArray`'s component name read the long way (the mirror's name,
/// then its legacy `read_string` / slot-1 readers), `/`-separated;
/// `java/lang/Object` when nothing answers.
pub fn comp_name_slow(
    ctx: &dyn NativeContext,
    comp_mirror: Option<cratonvm_types::ObjectRef>,
) -> String {
    match comp_mirror {
        Some(mirror) => crate::lang_class::mirror_class_name(ctx, mirror)
            .filter(|s| !s.is_empty())
            .or_else(|| ctx.read_string(mirror))
            .or_else(|| match ctx.get_field(mirror, 1) {
                Value::Object(Some(name_obj)) => ctx.read_string(name_obj),
                _ => None,
            })
            .map(|s| s.replace('.', "/"))
            .unwrap_or_else(|| "java/lang/Object".to_string()),
        _ => "java/lang/Object".to_string(),
    }
}

/// `java/lang/reflect/Array.multiNewArray(Class componentType, int[] dimensions)`
///
/// Allocates a fully-materialized multi-dimensional array. Distinct from the
/// single-dim `newArray`: the result's runtime class must be the *precise*
/// nested array type вЂ” `Array.newInstance(String.class, {2,2})` в†’
/// `[[Ljava/lang/String;`, not `[Ljava/lang/String;` (SpEL
/// `ArrayConstructorTests.multiDimensionalArrays` asserts this exactly).
///
/// Each non-leaf level is therefore allocated with the resolved nested-array
/// component `ClassId` (via `ensure_class_initialized` on the `[вЂ¦` descriptor)
/// rather than the loose `ClassId(0)` the `multianewarray` *bytecode* path
/// uses вЂ” bytecode-built multiarrays are rarely inspected via `getClass()`,
/// reflective ones are.
///
/// Previously this descriptor (and `Array.newInstance(Class, int[])`) was wired
/// to a single-dim allocator that read only `dims[0]`, collapsing the result to
/// one dimension.
pub fn native_array_multi_new_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::error::MethodCallFailed;

    // HotSpot's `Reflection::reflect_new_multi_array` checks, in this order
    // (probe `L4W29ReflectNewArray.java`, rows 9-29):
    // a null component or a null dimensions array is a
    // `NullPointerException`; zero or more than 255 dimensions is an
    // `IllegalArgumentException`; then each negative dimension, in order, is
    // `NegativeArraySizeException`; then a `void` component, or an array
    // component whose own dimensions plus these exceed 255, is an
    // `IllegalArgumentException`. All the `IllegalArgumentException`s carry
    // no message. Before wave 29 an empty dimensions array or a null one
    // answered `int[0]`, a null component built `Object[]...`, and 256
    // dimensions were built.
    let comp_mirror = match (args.first(), args.get(1)) {
        (Some(Value::Object(Some(m))), Some(Value::Object(Some(_)))) => *m,
        // Defensive: a single Int (1-D shape) вЂ” defer to the single-dim path.
        (_, Some(Value::Int(_))) => return native_array_new_array(ctx, args),
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    // args[1] = int[] of per-dimension lengths.
    let dims_arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let ndims = ctx.array_length(dims_arr);
    if ndims == 0 || ndims > REFLECT_ARRAY_MAX_DIMENSIONS {
        return Err(RuntimeError::IllegalArgumentException {
            message: String::new(),        }
        .into());
    }
    if ndims == 1 {
        // A single dimension behaves exactly like `newArray`; reuse it so the
        // (already well-tested) Class-mirror element-type resolution applies.
        // Its checks are the same ones in the same order for one dimension.
        let one_dim = [
            Value::Object(Some(comp_mirror)),
            ctx.get_array_element(dims_arr, 0),
        ];
        return native_array_new_array(ctx, &one_dim);
    }

    let mut sizes: Vec<usize> = Vec::with_capacity(ndims);
    for i in 0..ndims {
        match ctx.get_array_element(dims_arr, i) {
            Value::Int(n) => {
                if n < 0 {
                    return Err(RuntimeError::NegativeArraySizeException { size: n }.into());
                }
                sizes.push(n as usize);
            }
            _ => sizes.push(0),
        }
    }

    // Resolve the leaf component type from the Class mirror (mirrors the
    // resolution in `native_array_new_array`).
    // A lambda proxy is virtual: its mirror's name is a placeholder, so name it as its arrays
    // are named and let the class manager know it (SBR5 walker).
    let lambda_leaf = crate::lang_class::mirror_class_id(&*ctx, comp_mirror)
        .and_then(|cid| crate::lang_class::register_lambda_array_component(ctx, cid));
    let comp_name = lambda_leaf
        .or_else(|| {
            crate::lang_class::mirror_class_name(&*ctx, comp_mirror).filter(|s| !s.is_empty())
        })
        .or_else(|| ctx.read_string(comp_mirror))
        .map(|s| s.replace('.', "/"))
        .unwrap_or_else(|| "java/lang/Object".to_string());

    // Either a primitive leaf (single-letter descriptor) or a reference leaf
    // (`L...;`, or the array class's own descriptor when the component is
    // itself an array: `int[].class` is `[I`, not `L[I;`). The descriptor is
    // what the nested array-class names are built from.
    let (prim_et, leaf_desc) = match comp_name.as_str() {
        "int" => (Some(cratonvm_types::ArrayElementType::Int), "I".to_string()),
        "long" => (
            Some(cratonvm_types::ArrayElementType::Long),
            "J".to_string(),
        ),
        "float" => (
            Some(cratonvm_types::ArrayElementType::Float),
            "F".to_string(),
        ),
        "double" => (
            Some(cratonvm_types::ArrayElementType::Double),
            "D".to_string(),
        ),
        "boolean" => (
            Some(cratonvm_types::ArrayElementType::Boolean),
            "Z".to_string(),
        ),
        "byte" => (
            Some(cratonvm_types::ArrayElementType::Byte),
            "B".to_string(),
        ),
        "char" => (
            Some(cratonvm_types::ArrayElementType::Char),
            "C".to_string(),
        ),
        "short" => (
            Some(cratonvm_types::ArrayElementType::Short),
            "S".to_string(),
        ),
        other if other.starts_with('[') => (None, other.to_string()),
        other => (None, format!("L{other};")),
    };
    if prim_et.is_none() && reflect_new_array_refuses_component(&comp_name, ndims) {
        return Err(RuntimeError::IllegalArgumentException {
            message: String::new(),        }
        .into());
    }

    // Reference leaf: the base component class, used for the innermost
    // `String[]`-style array. Primitives ignore it. The mirror's OWN class
    // first, as `native_array_new_array` does: re-resolving the name ran the
    // component's `<clinit>` (HotSpot's `Array.newInstance` initializes
    // nothing), picked the caller's namespace for another loader's class, and
    // found nothing at all for a hidden class. The by-name lookup stays only
    // for mirrors the reverse map does not know (synthesized / legacy ones).
    let base_ref_id = if prim_et.is_none() {
        crate::lang_class::mirror_class_id(&*ctx, comp_mirror)
            .or_else(|| ctx.ensure_class_initialized(&comp_name).ok())
            .unwrap_or(cratonvm_types::ClassId::new(0))
    } else {
        cratonvm_types::ClassId::new(0)
    };

    // The component class of each intermediate level, resolved once per
    // level (not once per sub-array): level `k` holds arrays of
    // `'[' * (ndims - 1 - k) + leaf`. A reference leaf's nested array classes
    // are the ones its DEFINING loader sees (JVMS §5.3.3): for a class of a
    // user-defined loader, or a hidden class, the name alone resolves to
    // nothing or to another class. The by-name path stays as the fallback
    // (primitive leaves, and contexts with no loader-faithful answer).
    let leaf_loader = if prim_et.is_none() && base_ref_id != cratonvm_types::ClassId::new(0) {
        u32::try_from(ctx.loader_id_of_class(base_ref_id)).ok()
    } else {
        None
    };
    let mut level_components: Vec<cratonvm_types::ClassId> = Vec::with_capacity(ndims - 1);
    for level in 0..ndims - 1 {
        let comp_desc: String = "[".repeat(ndims - 1 - level) + &leaf_desc;
        let faithful = match leaf_loader {
            Some(loader) => ctx.array_class_id_for_loader(&comp_desc, loader),
            None => None,
        };
        let comp_id = match faithful {
            Some(id) => id,
            None => ctx
                .ensure_class_initialized(&comp_desc)
                .unwrap_or(cratonvm_types::ClassId::new(0)),
        };
        level_components.push(comp_id);
    }

    fn build(
        ctx: &mut dyn NativeContext,
        sizes: &[usize],
        level: usize,
        prim_et: Option<cratonvm_types::ArrayElementType>,
        base_ref_id: cratonvm_types::ClassId,
        level_components: &[cratonvm_types::ClassId],
    ) -> Result<ObjectRef, MethodCallFailed> {
        let ndims = sizes.len();
        let len = sizes[level];
        if level == ndims - 1 {
            // Innermost specified dimension: leaf array of the base type.
            let arr = match prim_et {
                Some(et) => ctx.new_array(et, len),
                None => ctx.new_ref_array(base_ref_id, len),
            };
            return Ok(arr);
        }
        // Intermediate level: a reference array whose component is the nested
        // array type one level down.
        let mut arr = ctx.new_ref_array(level_components[level], len);
        // Pin the parent across each sub-array allocation: under a moving
        // collector `arr` may relocate while `build` allocates.
        let pin = ctx.pin_native_root(arr);
        for i in 0..len {
            arr = ctx.read_native_pin(pin, arr);
            let sub = build(
                ctx,
                sizes,
                level + 1,
                prim_et,
                base_ref_id,
                level_components,
            )?;
            arr = ctx.read_native_pin(pin, arr);
            ctx.set_array_element(arr, i, Value::Object(Some(sub)));
        }
        ctx.unpin_native_roots(pin);
        Ok(arr)
    }

    let arr = build(ctx, &sizes, 0, prim_et, base_ref_id, &level_components)?;
    Ok(Some(Value::Object(Some(arr))))
}

/// JVMS §4.3.2: an array type has at most 255 dimensions. HotSpot's
/// `Reflection` refuses a reflective allocation that would exceed it with an
/// `IllegalArgumentException` (no message).
pub const REFLECT_ARRAY_MAX_DIMENSIONS: usize = 255;

/// Whether `Array.newInstance` refuses a reference (non-primitive-array)
/// component named `comp_name` for an allocation of `added_dims` dimensions:
/// `void` (there is no `void[]`), or an array component whose own dimensions
/// plus `added_dims` exceed 255. Only reached off the primitive arms, so the
/// common `Arrays.copyOf` path pays a short string compare.
pub fn reflect_new_array_refuses_component(comp_name: &str, added_dims: usize) -> bool {
    if comp_name == "void" || comp_name == "V" {
        return true;
    }
    if !comp_name.starts_with('[') {
        return false;
    }
    let own_dims = comp_name.bytes().take_while(|&b| b == b'[').count();
    own_dims + added_dims > REFLECT_ARRAY_MAX_DIMENSIONS
}

pub const CLASS_FILE_MAGIC: [u8; 4] = [0xCA, 0xFE, 0xBA, 0xBE];

pub fn define_class_format_error(class_name: &str, method: &str, message: String) -> MethodCallFailed {
    LinkageError::ClassFormatError {
        class_name: class_name.to_string(),
        message: format!("{method}: {message}"),
    }
    .into()
}

// ---------------------------------------------------------------------------
// defineClass0/1/2 — recovering the TYPE of a backend failure (W7-31 §Falsifier 3)
//
// `NativeContext::define_class_full` is typed `Result<ClassId, String>`, and the
// VM's implementation fills that `String` with
// `class_manager::define_class_with_options`'s `VmError` rendered by
// `.map_err(|e| format!("{e:?}"))` — a Rust `Debug` string. Every `defineClassN`
// failure arm then re-wrapped it as `ClassFormatError`, so a class file that
// must raise `UnsupportedClassVersionError` produced instead:
//
//   HotSpot : java.lang.UnsupportedClassVersionError: Preview features are not
//             enabled for <Unknown> (class file version 69.65535). Try running
//             with '--enable-preview'
//   CratonVM: java.lang.ClassFormatError: : defineClass1:
//             Linkage(UnsupportedClassVersionError { class_name: "", message:
//             "Preview features are not enabled for <Unknown> (class file
//             version 69.65535). Try running with '--enable-preview'" })
//
// Two separate breakages in one line. The TYPE is wrong — a container catching
// the JDK's typed exception (application servers probing whether they can load
// a bundle, test frameworks branching on linkage kind) does not catch ours — and
// the MESSAGE is a Rust value dump.
//
// The right repair is to widen `define_class_full` to carry `VmError`; that is a
// trait-signature change across ~25 call sites in six crates and is nominated,
// not done here. What is done here is the repair the record asks for: give the
// `defineClassN` tail a pass-through that recovers the already-typed
// `VmError::Linkage(..)` from the rendering it was flattened into, and rebuild
// the typed variant so `runtime::exceptions::linkage_throwable` — which already
// has a complete, correct arm per variant — produces the JDK exception.
//
// **Never re-emit the Debug text.** Every arm below either carries a field the
// backend wrote (already human-readable — `LinkageError`'s own `#[error]`
// strings and HotSpot's verbatim version wording) or names the variant. The raw
// `msg` is used only when the string is NOT a `Debug` rendering at all, which is
// the case for `define_class_full`'s own plain-string failures ("define_class_full
// failed for X", "initialize after define failed for X: ...").
//
// **`class_name: ""` is not a lost name.** It is the caller's own argument: the
// Java call was `ClassLoader.defineClass(null, bytes, off, len)`, and the class
// manager's version check runs BEFORE `this_class` is read from the constant
// pool, so no name exists to substitute. HotSpot has the identical ordering and
// prints `<Unknown>` mid-message — which the recovered `message` field already
// contains. What was visibly damaged was the outer wrapper's
// `format!("{class_name}: {message}")`, i.e. the stray leading `": "` above;
// that disappears with the flattening, because the `UnsupportedClassVersionError`
// arm of `linkage_throwable` deliberately does not prefix the name.
// ---------------------------------------------------------------------------

/// Split a Rust `Debug` rendering of an enum into `(outer, inner, body)`.
///
/// `Linkage(ClassFormatError { class_name: "A", message: "b" })`
///   -> `("Linkage", "ClassFormatError", "class_name: \"A\", message: \"b\"")`
///
/// A tuple variant with no struct body (`JdkOnly(..)`) yields an empty body.
/// Returns `None` for anything that is not shaped like `Ident(..)` — a plain
/// human-written error string, which the caller must pass through unchanged.
pub fn split_debug_error(msg: &str) -> Option<(&str, &str, &str)> {
    let open = msg.find('(')?;
    let outer = &msg[..open];
    if outer.is_empty() || !outer.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let close = msg.rfind(')')?;
    if close <= open {
        return None;
    }
    let inner_all = &msg[open + 1..close];
    // `#[derive(Debug)]` renders a struct variant as `Name { a: 1, b: 2 }`,
    // with exactly one space inside each brace.
    match inner_all.find(" { ") {
        Some(brace) => {
            let end = inner_all.rfind(" }")?;
            if end < brace + 3 {
                return None;
            }
            Some((outer, &inner_all[..brace], &inner_all[brace + 3..end]))
        }
        None => Some((outer, inner_all, "")),
    }
}

/// Undo the escaping `Debug` applies to a `String`, stopping at the closing
/// quote. `\u{..}` forms are not decoded — they are rare (control characters in
/// a class name) and a literal `u{7f}` in a diagnostic is better than a partial
/// parse that drops the rest of the sentence.
pub fn unescape_debug_string(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                // Covers `\\`, `\"` and `\'`.
                Some(other) => out.push(other),
                None => break,
            },
            other => out.push(other),
        }
    }
    out
}

/// Read a `name: "value"` `String` field out of a `Debug` struct body.
///
/// The name must sit at a field boundary (start of the body, or just after a
/// `", "` separator) so a `field:` appearing INSIDE another field's text cannot
/// be mistaken for the field itself.
pub fn debug_string_field(body: &str, field: &str) -> Option<String> {
    let mut from = 0usize;
    while from < body.len() {
        let at = from + body[from..].find(field)?;
        let after = at + field.len();
        let at_boundary = at == 0 || body[..at].ends_with(", ");
        if at_boundary && body[after..].starts_with(": \"") {
            return Some(unescape_debug_string(&body[after + 3..]));
        }
        from = after;
    }
    None
}

/// Recover a typed, catchable `java.lang.ExceptionInInitializerError` for an
/// `initialize=true` define-class failure, instead of the message-only
/// `IllegalStateException` with no `getCause()` this used to build.
///
/// `msg` is the backend's own
/// `"initialize after define failed for {name}: {cause}"` string, where
/// `{cause}` is `describe_throwable`'s `"{internal/class/Name}: {message}"`
/// rendering (bare `{internal/class/Name}` when the Throwable carries no
/// message). See
/// defineclass0-two-registrations-and-the-init-contract-20260912.md
/// §5: HotSpot raises `ExceptionInInitializerError` wrapping the real
/// `<clinit>` exception; this VM raised `IllegalStateException` with `<none>`
/// as the cause, so a caller catching the JDK's type or reading `getCause()`
/// got neither.
///
/// The ORIGINAL exception object is gone by the time the backend has
/// flattened it to this string (`NativeContext::define_class_full` is typed
/// `Result<ClassId, String>` — see the module note above `split_debug_error`).
/// Best-effort recovery: build a NEW instance of the cause's own class from
/// its recovered name and message, which matches HotSpot's `getCause()`
/// class and text for the common case of a plain exception thrown from
/// `<clinit>` (exactly what `apps/probes/L2InitFail.java` exercises), even
/// though it is not the identical object HotSpot would hand back. Falls back
/// to a plain `RuntimeException` carrying the raw cause text when the named
/// class cannot be recovered (not loaded, or no matching constructor) —
/// still typed and catchable, unlike the previous `<none>`.
pub fn exception_in_initializer_error(
    ctx: &mut dyn NativeContext,
    hidden_name: &str,
    msg: &str,
) -> MethodCallFailed {
    let prefix = format!("initialize after define failed for {hidden_name}: ");
    let cause_desc = msg.strip_prefix(&prefix).unwrap_or(msg);
    let (cause_class, cause_message) = match cause_desc.split_once(": ") {
        Some((class, message)) => (class, Some(message)),
        None => (cause_desc, None),
    };
    let cause_obj = build_throwable(ctx, cause_class, cause_message)
        .or_else(|| build_throwable(ctx, "java/lang/RuntimeException", Some(cause_desc)));
    let fallback = || {
        RuntimeError::IllegalStateException {
            message: format!("ExceptionInInitializerError for {hidden_name}: {msg}"),
        }
        .into()
    };
    match cause_obj {
        Some(cause) => cratonvm_native_api::throwable_builder::throw_new(
            ctx,
            "java/lang/ExceptionInInitializerError",
            "(Ljava/lang/Throwable;)V",
            &[Value::Object(Some(cause))],
            fallback,
        ),
        None => fallback(),
    }
}

/// Build a `class_name` instance for [`exception_in_initializer_error`],
/// preferring `(String)` when `message` is present and falling back to the
/// no-arg constructor. Only classes already loaded — never triggers a fresh
/// class load, so a cause type that itself fails to load cannot recurse into
/// the same failure this function exists to report. `None` on any failure
/// (not loaded, no matching constructor, construction threw) — the caller
/// treats that as "try the next fallback", not as a hard error.
pub fn build_throwable(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    message: Option<&str>,
) -> Option<ObjectRef> {
    ctx.class_id_by_name(class_name)?;
    let result = match message {
        Some(m) => {
            let s = ctx.create_string(m);
            ctx.new_object_initialized(
                class_name,
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(s))],
            )
        }
        None => ctx.new_object_initialized(class_name, "()V", &[]),
    };
    match result {
        Ok(Some(Value::Object(Some(obj)))) => Some(obj),
        _ => None,
    }
}

/// The FORMAT half of [`define_class_linkage_error`], for the callers that must
/// not take the rest.
///
/// `MethodHandles.Lookup.define{,Hidden}Class` is specified with a SPLIT
/// contract, and the split falls exactly on `VmError`'s Linkage/Runtime line:
///
/// * bytes that are not a well-formed ClassFile  -> `ClassFormatError`
/// * bytes that name a class in a different package from the lookup class, or
///   a lookup without the right mode -> `IllegalArgumentException`
///
/// So those doors cannot use `define_class_linkage_error`, which re-types
/// EVERY recovered variant: the backend reports a define into `java.util` as
/// `RuntimeError::SecurityException` (JVMS 5.3.5, and correct for the
/// `ClassLoader` door), and handing that to Java would answer
/// `SecurityException` where the JDK answers `IllegalArgumentException`. This
/// returns `Some` only for the linkage/format family, leaving the caller's own
/// `IllegalArgumentException` wrapper in place for everything else.
///
/// Measured, not assumed: `probes/DynClassGenSweep.java` carries a
/// `define.lookupWrongPackage` row precisely so a change on this surface that
/// over-reaches is visible as a regression rather than as a silent one.
pub fn lookup_define_format_error(
    class_name: &str,
    method: &str,
    msg: &str,
) -> Option<MethodCallFailed> {
    match typed_define_class_error(class_name, method, msg) {
        Some(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Linkage(e))) => Some(
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Linkage(e)),
        ),
        // `typed_define_class_error` already re-homes every `ClassFile` variant
        // onto a `Linkage` one (a `ClassFile` variant is UNCATCHABLE), so the
        // arm above is the whole format family.
        _ => None,
    }
}

/// zt t7/cl3: the state of `System.out` and `System.err` as the program's own
/// bytecode sees them. Printed by the launcher after `main` under
/// `CRATONVM_DBG_PRINT_LOSS=1`, so a run whose output went nowhere can be
/// told apart in one pass. The cases it separates:
/// * the `System` static is not the VM's stream (`same_as_vm=false`);
/// * the stream lost its wiring (`out` / `textOut` / `charOut` null), so the
///   JDK's `PrintStream` bytecode throws `IOException` in `ensureOpen()` and
///   absorbs it into `trouble` (`install_real_stream_fields`' doc: every write
///   is discarded and the VM exits 0);
/// * a write failed (`trouble=1`);
/// * the `FileOutputStream` at the bottom has no descriptor the VM can resolve
///   (`fd=-` / `handle=-`). `native-io`'s write natives then return without
///   writing.
///
/// Reads only; allocates nothing.
pub fn system_stream_report(ctx: &dyn NativeContext) -> Vec<String> {
    let class_of = |obj: ObjectRef| {
        ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string())
    };
    let field_text = |obj: ObjectRef, name: &str| match ctx.get_field_by_name(obj, name) {
        Value::Object(Some(o)) => class_of(o),
        Value::Object(None) => "null".to_string(),
        Value::Int(v) => v.to_string(),
        Value::Long(v) => v.to_string(),
        other => format!("{other:?}"),
    };
    let system = ctx.class_id_by_name("java/lang/System");
    let mut lines = Vec::new();
    for name in ["out", "err"] {
        let java = system
            .and_then(|cid| {
                ctx.static_field_index_by_name(cid, name)
                    .map(|i| ctx.get_static_field(cid, i))
            })
            .and_then(|v| match v {
                Value::Object(o) => o,
                _ => None,
            });
        let vm = ctx.get_system_stream(name);
        let same = match (java, vm) {
            (Some(a), Some(b)) => a.as_ptr() == b.as_ptr(),
            (None, None) => true,
            _ => false,
        };
        let Some(stream) = java else {
            lines.push(format!("System.{name}: null same_as_vm={same}"));
            continue;
        };
        // The descriptor under `out` (a `FileOutputStream`, or one level down
        // through a `FilterOutputStream`'s own `out`).
        let mut sink = match ctx.get_field_by_name(stream, "out") {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        };
        if let Some(s) = sink {
            if !matches!(ctx.get_field_by_name(s, "fd"), Value::Object(Some(_))) {
                if let Value::Object(Some(inner)) = ctx.get_field_by_name(s, "out") {
                    sink = Some(inner);
                }
            }
        }
        let fd = match sink.map(|s| ctx.get_field_by_name(s, "fd")) {
            Some(Value::Object(Some(fd_obj))) => format!(
                "fd={} handle={}",
                field_text(fd_obj, "fd"),
                field_text(fd_obj, "handle")
            ),
            _ => "fd=- handle=-".to_string(),
        };
        lines.push(format!(
            "System.{name}: class={} same_as_vm={same} out={} textOut={} charOut={} \
             charset={} trouble={} closing={} autoFlush={} sink={} {fd}",
            class_of(stream),
            field_text(stream, "out"),
            field_text(stream, "textOut"),
            field_text(stream, "charOut"),
            field_text(stream, "charset"),
            field_text(stream, "trouble"),
            field_text(stream, "closing"),
            field_text(stream, "autoFlush"),
            sink.map(class_of).unwrap_or_else(|| "-".to_string()),
        ));
    }
    lines
}


/// zs s2/nd2 (`zgc-zr11nd-native-oom-replay-residuals-20261013.md` §1, class
/// definers): the mirror of a class a definer has just defined, minted with a
/// collection before the `OutOfMemoryError` on ZGC.
///
/// A definer cannot be on the funnel's restartable list (a re-run defines the
/// class again: `LinkageError`), and its mirror is minted AFTER the define, so
/// on a full heap the one-attempt mirror door raised `OutOfMemoryError` without
/// a collection, leaving a defined class whose `Class` object Java never saw.
/// HotSpot allocates the mirror inside the define, after a collection if need
/// be. The callers hold every reference they still use in a pin (loader,
/// protection domain, class data) and nothing raw across this call -- the
/// define before it is already a GC point -- so the reclaim ladder's
/// precondition holds. ZGC only: gci i48/zl2 made
/// `native_doors_collect_on_refusal` true on Generational and G1 too, and the
/// zt round keeps dev's one-attempt mirror door there (zt t1, page
/// `zgc-zt1nd-define-and-door-residuals-after-the-merge-SUPERSEDED-20261019.md` item 2).
/// `CRATONVM_ZGC_DEFINE_MIRROR_COLLECT=0` restores the one-attempt door on ZGC.
/// Spent ladder: the one-attempt door raises the error as before.
pub fn defined_class_mirror(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> ObjectRef {
    if !ctx.heap_is_zgc()
        || !ctx.native_doors_collect_on_refusal()
    {
        return ctx.get_class_mirror(class_id);
    }
    if let Some(mirror) = ctx.try_get_class_mirror(class_id) {
        return mirror;
    }
    if let Some(mirror) = crate::util_concurrent_ext::retry_after_reclaim(ctx, |ctx| {
        ctx.try_get_class_mirror(class_id)
    }) {
        return mirror;
    }
    ctx.get_class_mirror(class_id)
}

/// zs s7/nd6 (`zgc-zs2nd2-proposal-mirror-inside-the-define-20261014.md`,
/// "Variant: a reserved mirror"): reserve the mirror object of a named class
/// about to be defined, BEFORE `define_class_full`, through the fallible door
/// and the reclaim ladder; a spent ladder is the define's `OutOfMemoryError`
/// with nothing defined, as HotSpot's (the mirror is created inside
/// `SystemDictionary::define_instance_class`), so the program's retry defines
/// the class instead of meeting `LinkageError: duplicate class definition`.
/// `Ok(Some((pin, object)))`: pinned; hand it to
/// [`take_mirror_reservation`] right after the define. `Ok(None)`: no
/// reservation (not ZGC, a switch off, or a context that does not reserve):
/// the define mints its mirror afterwards, as before. The caller must hold
/// no unpinned reference (the ladder collects).
/// `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED=0` (default on) is the kill.
pub fn reserve_defined_class_mirror(
    ctx: &mut dyn NativeContext,
    internal_name: &str,
) -> Result<Option<(usize, ObjectRef)>, MethodCallFailed> {
    use cratonvm_native_api::MirrorReservation;
    if !ctx.native_doors_collect_on_refusal()
    {
        return Ok(None);
    }
    let pooled = (!internal_name.is_empty()).then_some(internal_name);
    let reserved = match ctx.try_reserve_class_mirror(pooled) {
        MirrorReservation::Unsupported => return Ok(None),
        MirrorReservation::Reserved(mirror) => mirror,
        MirrorReservation::Refused => {
            match crate::util_concurrent_ext::retry_after_reclaim(ctx, |ctx| {
                match ctx.try_reserve_class_mirror(pooled) {
                    MirrorReservation::Reserved(mirror) => Some(mirror),
                    _ => None,
                }
            }) {
                Some(mirror) => mirror,
                // HotSpot's message for a refused mirror inside a define.
                None => {
                    return Err(RuntimeError::OutOfMemoryError {
                        message: "Java heap space".to_string(),
                    }
                    .into())
                }
            }
        }
    };
    Ok(Some((ctx.pin_native_root(reserved), reserved)))
}

/// zs s7/nd6: the reserved mirror's current address, unpinned (read it right
/// after the define, before anything else allocates).
pub fn take_mirror_reservation(
    ctx: &mut dyn NativeContext,
    reservation: Option<(usize, ObjectRef)>,
) -> Option<ObjectRef> {
    let (pin, object) = reservation?;
    let object = ctx.read_native_pin(pin, object);
    ctx.unpin_native_roots(pin);
    Some(object)
}

/// [`defined_class_mirror`] that adopts a reserved mirror object first (zs
/// s7/nd6); call it with nothing allocated since [`take_mirror_reservation`].
pub fn defined_class_mirror_reserved(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    reserved: Option<ObjectRef>,
) -> ObjectRef {
    if let Some(reserved) = reserved {
        if let Some(mirror) = ctx.adopt_reserved_class_mirror(class_id, reserved) {
            return mirror;
        }
    }
    defined_class_mirror(ctx, class_id)
}

/// zs s8/nd7 (`zgc-zs7nd6-native-door-residuals-FIXED-20261019.md` §1): whether the
/// definers other than `defineClass1/2` reserve their mirror too.
/// `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER=0` (default on) is their
/// kill; nd6's `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED=0` turns every door off.
pub fn every_definer_reserves_mirror() -> bool {
    true
}

/// zs s9/nd8: whether a hidden define parks its name in the reserved mirror
/// (`CRATONVM_ZGC_DEFINE_MIRROR_HIDDEN_NAME_PARKED`, default on, `=0` kill;
/// needs `CRATONVM_ZGC_DEFINE_MIRROR_NAME_POOLED_AT_ADOPTION`, whose `=0`
/// would intern the parked name).
pub fn hidden_name_parked() -> bool {
    true
}

/// zs s9/nd8 (`zgc-zs8nd7-define-mirror-residuals-FIXED-20261019.md` §1): the name
/// a define's mirror reservation parks. A NAMELESS define
/// (`defineClass(null, bytes, ..)`, `Unsafe.defineClass(null, ..)`) takes
/// the class file's own `this_class`, so the adoption does not allocate the
/// name after the define (a refusal there left the class defined and the
/// retry met `LinkageError`). A wrong guess costs nothing: the adoption
/// reuses a parked name only when it equals the defined class's.
/// `CRATONVM_ZGC_DEFINE_MIRROR_NAME_FROM_BYTES=0` (default on) parks none.
pub fn define_name_hint<'a>(name: &'a str, bytes: &[u8]) -> std::borrow::Cow<'a, str> {
    if !name.is_empty()
    {
        return std::borrow::Cow::Borrowed(name);
    }
    match crate::classloader::extract_this_class_name(bytes) {
        Some(this_class) => std::borrow::Cow::Owned(this_class),
        None => std::borrow::Cow::Borrowed(name),
    }
}

/// zs s8/nd7: [`reserve_defined_class_mirror`] for a definer whose
/// `DefineClassFull` is already built. The reservation can collect, so the
/// one reference `opts` carries -- `protection_domain`, which the define
/// reads at its entry (the load-time transformers are handed it) -- is
/// pinned across it and written back current. `name_hint` is the class's
/// internal name when the caller knows it (pooled now, so the adoption's name
/// lookup is a pool hit); a hidden define pools nothing (its name String is
/// not interned). The caller holds no other unpinned reference across this
/// call, and hands the answer to [`take_mirror_reservation`] right after the
/// define, then to [`defined_class_mirror_reserved`] with nothing allocated in
/// between.
pub fn reserve_mirror_for_define(
    ctx: &mut dyn NativeContext,
    name_hint: &str,
    opts: &mut cratonvm_native_api::DefineClassFull,
) -> Result<Option<(usize, ObjectRef)>, MethodCallFailed> {
    if !every_definer_reserves_mirror() {
        return Ok(None);
    }
    // zs s9/nd8 (`zgc-zs8nd7-define-mirror-residuals-FIXED-20261019.md` §1): a
    // hidden class's (mangled) name is parked too, UNPOOLED -- the VM parks
    // an unpooled String when the name is not in the pool, and only while
    // `CRATONVM_ZGC_DEFINE_MIRROR_NAME_POOLED_AT_ADOPTION` is on (its `=0`
    // pools at the reservation, which a hidden name must never be) -- so the
    // adoption reuses it instead of allocating after the define.
    let pooled = if opts.hidden && !hidden_name_parked() {
        ""
    } else {
        opts.override_name.as_deref().unwrap_or(name_hint)
    };
    let pooled = pooled.to_string();
    let Some(pd) = opts.protection_domain else {
        return reserve_defined_class_mirror(ctx, &pooled);
    };
    let pd_pin = ctx.pin_native_root(pd);
    let reservation = reserve_defined_class_mirror(ctx, &pooled);
    opts.protection_domain = Some(ctx.read_native_pin(pd_pin, pd));
    match reservation {
        Ok(Some((mirror_pin, mirror))) => {
            // Pins are a stack: re-pin the mirror at the domain's slot so the
            // caller's single `take_mirror_reservation` releases both. Nothing
            // allocates between the release and the re-pin.
            let mirror = ctx.read_native_pin(mirror_pin, mirror);
            ctx.unpin_native_roots(pd_pin);
            Ok(Some((ctx.pin_native_root(mirror), mirror)))
        }
        other => {
            ctx.unpin_native_roots(pd_pin);
            other
        }
    }
}

/// Turn a `define_class_full` failure string back into the typed JVM error the
/// backend actually raised.
///
/// `class_name` is the caller's own name argument, used only when the recovered
/// error names nothing (or names nothing useful). See the module note above for
/// why an empty name here is faithful rather than lost.
pub fn define_class_linkage_error(class_name: &str, method: &str, msg: String) -> MethodCallFailed {
    match typed_define_class_error(class_name, method, &msg) {
        Some(err) => err,
        // Not a `Debug` rendering — `define_class_full`'s own plain-string
        // failures land here, and they are already readable.
        None => define_class_format_error(class_name, method, msg),
    }
}

/// The recovery half of [`define_class_linkage_error`]. Split out so the
/// borrow of `msg` that [`split_debug_error`] produces ends before the caller
/// needs to move the `String` into its fallback.
pub fn typed_define_class_error(class_name: &str, method: &str, msg: &str) -> Option<MethodCallFailed> {
    let (outer, inner, body) = split_debug_error(msg)?;

    // The backend's own name when it has one (a supertype-resolution failure
    // names the SUPERTYPE, not the class being defined), else the caller's.
    let named = |field: &str| {
        debug_string_field(body, field)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| class_name.to_string())
    };
    // A recovered variant always has a readable message: the field the backend
    // wrote, or — if the field cannot be read — the variant's own name. Never
    // `msg`, which is the `Debug` text this function exists to remove.
    let detail = |field: &str| {
        debug_string_field(body, field).unwrap_or_else(|| format!("{method}: {inner}"))
    };

    Some(match (outer, inner) {
        // -- VmError::Linkage: already the right shape, just re-typed. --------
        ("Linkage", "UnsupportedClassVersionError") => LinkageError::UnsupportedClassVersionError {
            class_name: named("class_name"),
            // HotSpot's wording verbatim; it already names the class
            // mid-sentence, which is why `linkage_throwable` does not prefix.
            message: detail("message"),
        }
        .into(),
        ("Linkage", "ClassFormatError") => LinkageError::ClassFormatError {
            class_name: named("class_name"),
            message: detail("message"),
        }
        .into(),
        ("Linkage", "VerifyError") => LinkageError::VerifyError {
            class_name: named("class_name"),
            method_name: debug_string_field(body, "method_name").unwrap_or_default(),
            message: detail("message"),
        }
        .into(),
        ("Linkage", "NoClassDefFoundError") => LinkageError::NoClassDefFoundError {
            class_name: named("class_name"),
        }
        .into(),
        ("Linkage", "IncompatibleClassChangeError") => LinkageError::IncompatibleClassChangeError {
            message: detail("message"),
        }
        .into(),
        ("Linkage", "ClassCircularityError") => LinkageError::ClassCircularityError {
            class_name: named("class_name"),
        }
        .into(),
        ("Linkage", "DuplicateClassDefinition") => LinkageError::DuplicateClassDefinition {
            class_name: named("class_name"),
            loader: debug_string_field(body, "loader").unwrap_or_else(|| "<unknown>".to_string()),
        }
        .into(),
        ("Linkage", "NoSuchFieldError") => LinkageError::NoSuchFieldError {
            class_name: named("class_name"),
            field_name: debug_string_field(body, "field_name").unwrap_or_default(),
            field_descriptor: debug_string_field(body, "field_descriptor").unwrap_or_default(),
        }
        .into(),
        ("Linkage", "NoSuchMethodError") => LinkageError::NoSuchMethodError {
            class_name: named("class_name"),
            method_name: debug_string_field(body, "method_name").unwrap_or_default(),
            method_descriptor: debug_string_field(body, "method_descriptor").unwrap_or_default(),
        }
        .into(),
        ("Linkage", "IllegalAccessError") => LinkageError::IllegalAccessError {
            message: detail("message"),
        }
        .into(),
        // JVMS §5.3.4: a define a recorded loader constraint refuses
        // (`ClassManager`'s define-time check, `--real-jdk`; interpreter round
        // i1 wave 37, lane L5). `java.lang.LinkageError` itself, with
        // HotSpot's message; without this arm it became a `ClassFormatError`.
        ("Linkage", "LoaderConstraintViolation") => LinkageError::LoaderConstraintViolation {
            message: detail("message"),
        }
        .into(),
        ("Linkage", "AbstractMethodError") => LinkageError::AbstractMethodError {
            class_name: named("class_name"),
            method_name: debug_string_field(body, "method_name").unwrap_or_default(),
        }
        .into(),
        ("Linkage", "UnsupportedClassRedefinitionError") => {
            LinkageError::UnsupportedClassRedefinitionError {
                class_name: named("class_name"),
                message: detail("message"),
            }
            .into()
        }

        // -- VmError::Runtime: only the one JVMS-mandated shape. --------------
        //
        // JVMS §5.3.5 / `ClassLoader.preDefineClass`: a non-bootstrap loader
        // defining into `java.*` is a `SecurityException`, NOT a linkage error,
        // and `class_manager.rs` raises it as `RuntimeError::SecurityException`.
        // `classify_fastpath_invoke_error` routes `VmError::Runtime` to the
        // ordinary runtime-exception path, so this arrives at Java as
        // `java.lang.SecurityException` with the backend's own sentence.
        ("Runtime", "SecurityException") => RuntimeError::SecurityException {
            message: detail("message"),
        }
        .into(),
        // gc-common w6-f: `-XX:MaxMetaspaceSize` refused the define after a
        // collection (`vm_exec.rs::define_class_full`). HotSpot throws
        // `OutOfMemoryError: Metaspace` from `defineClass`, not a linkage error.
        ("Runtime", "OutOfMemoryError") => RuntimeError::OutOfMemoryError {
            message: detail("message"),
        }
        .into(),

        // -- VmError::ClassFile: MUST be re-homed onto a Linkage variant. -----
        //
        // Not cosmetic. `classify_fastpath_invoke_error` (vm/src/runtime/
        // interpreter.rs) converts `VmError::Linkage` and `VmError::Runtime`
        // into Java throwables and sends everything else to
        // `FastPathInvokeError::Fatal` — so returning a `ClassFile` variant from
        // a native is UNCATCHABLE and unwinds past every handler.
        ("ClassFile", "ClassNotFound") => LinkageError::NoClassDefFoundError {
            class_name: named("class_name"),
        }
        .into(),
        ("ClassFile", "UnsupportedVersion") => LinkageError::UnsupportedClassVersionError {
            class_name: named("class_name"),
            // This variant carries `major`/`minor` ints rather than a message,
            // so build HotSpot's shape by hand instead of dumping the fields.
            message: format!(
                "{} has an unsupported class file version",
                named("class_name")
            ),
        }
        .into(),

        // Everything else recognised-but-unmapped keeps `ClassFormatError` —
        // today's answer — but with a readable message rather than the dump.
        // (A circular hierarchy is no longer one of them: the loading guard
        // returns `Linkage(ClassCircularityError)`, re-typed above.)
        _ => LinkageError::ClassFormatError {
            class_name: named("class_name"),
            message: format!(
                "{method}: {}",
                debug_string_field(body, "message").unwrap_or_else(|| inner.to_string())
            ),
        }
        .into(),
    })
}

pub fn validate_classfile_header(
    class_name: &str,
    method: &str,
    bytes: &[u8],
) -> Result<(), MethodCallFailed> {
    if bytes.len() < 8 || bytes[0..4] != CLASS_FILE_MAGIC {
        return Err(define_class_format_error(
            class_name,
            method,
            "not a valid class file (bad magic)".to_string(),
        ));
    }
    Ok(())
}

/// `--real-jdk`: HotSpot's `ClassFileParser` header errors for the bytes a
/// `defineClass1` / `defineClass2` call hands over, measured on JDK 25
/// (`tools/probes/interp/L5/L5W42DefineClassErrors.java`): fewer than 8 bytes
/// (magic, minor, major) is `ClassFormatError: Truncated class file`; a wrong
/// magic is `ClassFormatError: Incompatible magic value <u32, decimal> in class
/// file <name>`, the name in internal form, `<Unknown>` for a null name. `None`
/// when the header is well-formed (the backend parses the rest). CratonVM said
/// `<name>: defineClass1: not a valid class file (bad magic)` for both before
/// interpreter round i1 wave 42 (lane L5); `--compatible` still does. The
/// VM's other define doors (`classloader::classfile_parser_header_refusal`)
/// use it in both modes since wave 95.
pub fn jdk_only_classfile_header_error(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    bytes: &[u8],
) -> Option<MethodCallFailed> {
    let message = if bytes.len() < 8 {
        "Truncated class file".to_string()
    } else if bytes[0..4] != CLASS_FILE_MAGIC {
        let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let named = if class_name.is_empty() {
            "<Unknown>"        } else {
            class_name        };
        format!("Incompatible magic value {magic} in class file {named}")    } else {
        return None;
    };
    Some(
        match crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassFormatError",
            1,
            &message,
        ) {
            Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
            Err(e) => e,
        },
    )
}

/// `--real-jdk`: HotSpot's `ClassFormatError: Truncated class file` for a
/// class file the reader ran off the end of (`ClassReaderError::
/// UnexpectedEndOfData`, which the backend hands back as a `ClassFormatError`
/// whose `Debug` text carries "unexpected end of data"), measured on JDK 25
/// (`L5W42DefineClassErrors`, `magic-version-only`, `cut-in-pool`). CratonVM
/// said `<name>: unexpected end of data at position <n>` before wave 42
/// (lane L5); `--compatible` still does. `None` for any other backend error.
pub fn jdk_only_truncated_class_file(
    ctx: &mut dyn NativeContext,
    backend_msg: &str,
) -> Option<MethodCallFailed> {
    // Two reader shapes of one fact: a read past the end
    // (`UnexpectedEndOfData`), and `reader::limits::ensure_count_fits`'s
    // pre-size refusal of a declared count the remaining bytes cannot hold
    // ("<table>: declared count N needs at least M bytes but only K remain",
    // `InvalidClassData`; `cut-in-pool` on the host, wave 42 follow-up).
    if !(backend_msg.contains("ClassFormatError")
        && (backend_msg.contains("unexpected end of data")
            || (backend_msg.contains("needs at least") && backend_msg.contains("bytes but only"))))
    {
        return None;
    }
    Some(
        match crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassFormatError",
            1,
            "Truncated class file",
        ) {
            Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
            Err(e) => e,
        },
    )
}

/// `--real-jdk`: the `ArrayIndexOutOfBoundsException` HotSpot's
/// `ClassLoader.defineClass1` native raises for `(off, len)` over a `byte[]` of
/// `array_len` elements, measured on JDK 25 (`L5W42DefineClassErrors`): a
/// negative `len` first, with a null message (`JNU_ThrowArrayIndexOutOfBounds
/// Exception(env, 0)`), then JNI `GetByteArrayRegion`'s "Array region
/// <off>..<off+len> out of bounds for length <n>". CratonVM said "Array index
/// out of range: <n>" for all of them before wave 42 (lane L5).
pub fn jdk_only_define_region_error(off: i32, len: i32, array_len: usize) -> Option<MethodCallFailed> {
    if len < 0 {
        return Some(RuntimeError::aioobe_no_message(len).into());
    }
    let array_len = array_len as i64;
    if off < 0 || i64::from(off) > array_len - i64::from(len) {
        return Some(
            RuntimeError::aioobe_with_message(
                off,
                format!(
                    "Array region {off}..{} out of bounds for length {array_len}",
                    i64::from(off) + i64::from(len)
                ),
            )
            .into(),
        );
    }
    None
}

pub fn read_define_class_nonnegative_int(
    args: &[Value],
    idx: usize,
) -> Result<usize, MethodCallFailed> {
    match args.get(idx) {
        Some(Value::Int(v)) if *v >= 0 => Ok(*v as usize),
        Some(Value::Int(v)) => Err(RuntimeError::aioobe_index_only(*v).into()),
        _ => Ok(0),
    }
}

pub fn read_byte_array_define_class_slice(
    ctx: &dyn NativeContext,
    array: ObjectRef,
    offset: usize,
    length: usize,
) -> Result<Vec<u8>, MethodCallFailed> {
    let arr_len = ctx.array_length(array);
    let end = offset
        .checked_add(length)
        .ok_or_else(|| RuntimeError::aioobe_index_only(i32::MAX))?;
    if end > arr_len {
        return Err(RuntimeError::aioobe_index_only(end.min(i32::MAX as usize) as i32).into());
    }

    // ONE bulk copy, not a `get_array_element` per byte (SBR5 `clload`: 6.3 million element
    // reads, each with a flag read, for 1 500 classes). The window is bounds-checked above.
    Ok(crate::classloader::read_class_bytes_bulk(
        ctx, array, offset, length,
    ))
}

/// Where a `java.nio.ByteBuffer`'s backing array and cursors actually live.
///
/// The synthetic stub this crate fabricates puts the backing `byte[]` at slot
/// 0 and `position`/`limit`/`capacity` at 1/2/3. A **real** JDK heap buffer
/// does not: `java.nio.HeapByteBuffer` declares no instance fields of its
/// own, so its layout is its ancestors' —
/// `java.nio.Buffer{mark,position,limit,capacity,address,segment}` at 0..5 and
/// `java.nio.ByteBuffer{hb,offset,isReadOnly,bigEndian,nativeByteOrder}` at
/// 6..10 (`javap -p java.nio.Buffer java.nio.ByteBuffer`). Slot 0 on a real
/// buffer is therefore `mark`, an `int` — the array branch below was skipped
/// for every real heap buffer, execution fell into the direct-buffer path,
/// `address` read back 0, and
/// `ClassLoader.defineClass(String, ByteBuffer, ProtectionDomain)` threw
/// "direct ByteBuffer has no native address" for a perfectly valid heap
/// buffer. (`position`/`limit`/`capacity` at 1/2/3 happen to coincide with
/// the real layout; `hb` does not, and neither does `offset`.)
///
/// `hb` is also the discriminator, and it needs no mode flag: a fabricated
/// synthetic stub names its fields `_f0.._fN`, so the by-name resolve MISSES
/// there and the synthetic slots are used unchanged. A real *direct* buffer
/// still resolves `hb` (it is declared on `ByteBuffer`, not `HeapByteBuffer`)
/// and simply reads back null, which correctly routes to the address path.
pub struct BbDefineLayout {
    /// Backing `byte[]`, or `None` for a direct buffer.
    pub hb: usize,
    /// `ByteBuffer.offset` — index of the buffer's element 0 inside `hb`.
    /// Non-zero for anything produced by `slice()`. `None` on the synthetic
    /// stub, which has no such field and always starts at 0.
    pub offset: Option<usize>,
    pub position: usize,
    pub limit: usize,
    pub capacity: usize,
}

pub fn bb_define_layout(ctx: &dyn NativeContext, bb: ObjectRef) -> BbDefineLayout {
    let cid = ctx.class_id_of_object(bb);
    let named = |n: &str| ctx.resolve_field_index_by_class_id(cid, n);
    match (
        named("hb"),
        named("position"),
        named("limit"),
        named("capacity"),
    ) {
        (Some(hb), Some(position), Some(limit), Some(capacity)) => BbDefineLayout {
            hb,
            offset: named("offset"),
            position,
            limit,
            capacity,
        },
        _ => BbDefineLayout {
            hb: 0,
            offset: None,
            position: 1,
            limit: 2,
            capacity: 3,
        },
    }
}

/// `pub`: this is the ONLY `defineClass2` ByteBuffer decoder in the
/// crate. `classloader::cl_define_class2` — which SHADOWS this module's
/// `defineClass2` registration in synthetic-JDK mode, because
/// `register_classloader_natives` runs after `register_essential_natives` and
/// `NativeMethodRegistry::register` is last-wins — used to carry a second,
/// unhardened copy that hardcoded slot 0 as the backing array and CLAMPED
/// out-of-range `(off, len)` instead of rejecting them. Both entry points now
/// call this one, so the layout witness above and the `checked_add` bounds
/// below cannot be in effect for one caller and inert for the other.
pub fn read_byte_buffer_define_class_slice(
    ctx: &dyn NativeContext,
    bb: ObjectRef,
    offset: usize,
    length: usize,
    class_name: &str,
) -> Result<Vec<u8>, MethodCallFailed> {
    let layout = bb_define_layout(ctx, bb);
    let pos_slot = layout.position;
    let limit_slot = layout.limit;
    let capacity_slot = layout.capacity;

    if let Value::Object(Some(array)) = ctx.get_field(bb, layout.hb) {
        // `hb` is shared with every other view of the same array; the buffer's
        // own element 0 sits at `hb_base`. Zero for the synthetic stub and for
        // a whole-array `ByteBuffer.wrap`, non-zero for a `slice()`.
        let hb_base = layout
            .offset
            .map(|s| ctx.get_field(bb, s).as_int().unwrap_or(0).max(0) as usize)
            .unwrap_or(0);
        let arr_len = ctx.array_length(array);
        let cap = arr_len.saturating_sub(hb_base);
        let pos = ctx.get_field(bb, pos_slot).as_int().unwrap_or(0).max(0) as usize;
        let limit = ctx
            .get_field(bb, limit_slot)
            .as_int()
            .unwrap_or(cap as i32)
            .max(0) as usize;
        // Bounds are checked in buffer-relative coordinates, then translated.
        let relative_off = pos
            .checked_add(offset)
            .ok_or_else(|| RuntimeError::aioobe_index_only(i32::MAX))?;
        let upper = limit.min(cap);
        let end = relative_off
            .checked_add(length)
            .ok_or_else(|| RuntimeError::aioobe_index_only(i32::MAX))?;
        if end > upper {
            return Err(RuntimeError::aioobe_index_only(end.min(i32::MAX as usize) as i32).into());
        }
        let absolute_off = hb_base
            .checked_add(relative_off)
            .ok_or_else(|| RuntimeError::aioobe_index_only(i32::MAX))?;
        return read_byte_array_define_class_slice(ctx, array, absolute_off, length);
    }

    let addr = match ctx.get_field_by_name(bb, "address") {
        // zt t20/df8: a real direct buffer whose `address` is 0 (the
        // zero-length `MemorySegment.NULL.asByteBuffer()`): HotSpot's
        // `Java_java_lang_ClassLoader_defineClass2` reads
        // `GetDirectBufferAddress(data)`, gets NULL and throws
        // `JNU_ThrowNullPointerException(env, 0)` (a null message) before it
        // reads a byte. Both modes. Only a buffer without the field (the
        // synthetic stub) keeps the format error below.
        Value::Long(0) => {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
        Value::Long(v) => v,
        _ => 0,
    };
    let cap = ctx
        .get_field(bb, capacity_slot)
        .as_int()
        .unwrap_or(0)
        .max(0) as usize;
    let limit = ctx
        .get_field(bb, limit_slot)
        .as_int()
        .unwrap_or(cap as i32)
        .max(0) as usize;
    if addr == 0 {
        return Err(define_class_format_error(
            class_name,
            "defineClass2",
            "direct ByteBuffer has no native address".to_string(),
        ));
    }
    // `offset` IS the buffer's position here: `ClassLoader.defineClass(String,
    // ByteBuffer, ProtectionDomain)` sends a direct buffer as
    // `defineClass2(.., b, b.position(), b.remaining(), ..)`, and HotSpot's
    // native reads `GetDirectBufferAddress(data) + offset`. Adding the
    // position again read `address + 2 * position` (gci i3/jd,
    // `gci-i2jm-defineclass2-direct-buffer-counts-its-position-twice`). The
    // heap arm above keeps its buffer-relative reading: the JDK never sends a
    // heap buffer here (it takes `defineClass1` for those). Dev fixed the
    // same defect independently (interpreter round i5 wave 100, lane L5,
    // probe `L5W100DefineClassBufferPosition`); this arm no longer reads
    // `position` at all.
    let absolute_off = offset;
    let upper = limit.min(cap);
    let end = absolute_off
        .checked_add(length)
        .ok_or_else(|| RuntimeError::aioobe_index_only(i32::MAX))?;
    if end > upper {
        return Err(RuntimeError::aioobe_index_only(end.min(i32::MAX as usize) as i32).into());
    }
    let mut out = vec![0u8; length];
    if length > 0 {
        let src = addr.wrapping_add(absolute_off as i64);
        if !ctx.copy_from_native_memory(src, &mut out) {
            return Err(define_class_format_error(
                class_name,
                "defineClass2",
                format!("direct ByteBuffer copy failed (addr={src:#x}, len={length})"),
            ));
        }
    }
    Ok(out)
}

/// Whether a class the real `ClassLoader.defineClass*` natives define for this
/// loader is filed under the loader's own namespace rather than the global one.
///
/// Strict mode: a plain `URLClassLoader` is a loader like any other. Filing what
/// it defines under the application namespace made a second definition of the
/// same generated name (a CGLIB subclass, first defined in a `URLClassLoader`
/// and then again in the application loader) a duplicate.
/// Pin the `ProtectionDomain` argument of a `defineClass*` native so
/// [`install_protection_domain`] can store it after the define allocates.
/// `None` for a null argument. `--real-jdk` since the fat-jar fix; both
/// modes since interpreter round i1 wave 47, lane L6: `--compatible` handed
/// a user-loader class's load-time transformers a `null` domain and answered
/// a code-source domain of its own from `getProtectionDomain()`
/// (`interpreter-L6-transformers-are-handed-a-null-protection-domain-FIXED-20261011`; probe
/// `L6W47CompatibleAgentModuleOpens.java`).
pub fn pin_protection_domain_arg(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    index: usize,
) -> Option<(usize, ObjectRef)> {
    match args.get(index) {
        Some(Value::Object(Some(pd))) => Some((ctx.pin_native_root(*pd), *pd)),
        _ => None,
    }
}

/// Store the `ProtectionDomain` a class was defined with in its
/// mirror's `protectionDomain` field, which is what HotSpot does and what JDK
/// 25's `Class.getProtectionDomain()` reads. `ClassLoader.preDefineClass` has
/// already replaced a null domain with the loader's `defaultDomain`, so every
/// loader-defined class gets one. The VM used to derive a code source from
/// the class's file instead, so a class Spring Boot's `LaunchedClassLoader`
/// defined reported `file:/app.jar` where HotSpot reports the
/// `jar:nested:/app.jar/!BOOT-INF/classes/!/` URL the loader passed. See
/// `fatjar-classes-load-from-the-vms-flattened-class-path-not-launchedclassloader-FIXED-20260924.md`.
pub fn install_protection_domain(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
    pd: Option<(usize, ObjectRef)>,
) {
    if let Some((pin, pd)) = pd {
        let pd = ctx.read_native_pin(pin, pd);
        ctx.set_field_by_name(mirror, "protectionDomain", Value::Object(Some(pd)));
    }
}

/// Does `loader_obj` define into its own namespace: a user-defined loader,
/// or a BARE `URLClassLoader` where the JDK's `URLClassLoader` bodies run
/// (`--real-jdk`, or `--compatible` over a real JDK with the
/// `JdkUrlClassLoaders` row; interpreter round i1 wave 52, lane L5).
pub fn defines_in_own_namespace(ctx: &mut dyn NativeContext, loader_obj: ObjectRef) -> bool {
    crate::classloader::is_user_defined_loader(ctx, loader_obj)
        || (ctx.jdk_url_class_loaders()
            && crate::classloader::is_bare_url_class_loader(ctx, loader_obj))
}

/// The namespace a define door hands `define_class_full` for `loader` (the
/// door's loader argument) defining `name` (internal form, empty for none),
/// and whether that loader defines in its own namespace
/// ([`defines_in_own_namespace`]), which the door passes on as
/// `DefineClassFull::user_loader_define`. The second answer is the LOADER's,
/// not the namespace's: with `CRATONVM_LOADER_AWARE_RESOLUTION=0` a user
/// loader's define of a fresh name goes to namespace `0`, and the class
/// manager would otherwise take it for an application-loader define
/// (interpreter round i4 wave 96, lane L5;
/// `i95-L5-define-door-loose-ends` items 1 and 3). One copy for
/// `defineClass0/1/2` and both `Unsafe.defineClass` natives, which read a raw
/// field slot as the loader id before wave 96.
pub fn define_loader_namespace(
    ctx: &mut dyn NativeContext,
    loader: Option<&Value>,
    name: &str,
) -> (u32, bool) {
    match loader {
        Some(Value::Object(Some(loader_obj))) if defines_in_own_namespace(ctx, *loader_obj) => {
            let own = crate::classloader::loader_aware_resolution()
                || name.is_empty()
                || ctx.class_id_by_name(name).is_some();
            let ns = if own {
                crate::classloader::loader_namespace_id(ctx, *loader_obj)
            } else {
                0
            };
            (ns, true)
        }
        // zt t21/df9 (define page §9): the PLATFORM loader object defines in
        // its own built-in namespace (`Extension`), as HotSpot defines in its
        // `ClassLoaderData`. Collapsed to `0` (the application namespace) it
        // shared one name space with the application loader: a define of a
        // name the other one had defined served that class (or failed with
        // `LinkageError`), its class was tagged an application-loader class
        // (`findLoadedClass` / `loadClass` of the platform loader missed it),
        // and the class manager's application-loader package rules refused a
        // `java.*` name the platform loader may define.
        // `CRATONVM_DEFINE_PLATFORM_LOADER_OWN_NAMESPACE=0`: `0` again.
        Some(Value::Object(Some(loader_obj)))
            if crate::classloader::is_platform_class_loader(ctx, *loader_obj)
                && define_platform_loader_own_namespace() =>
        {
            (cratonvm_types::ClassLoaderId::NATIVE_EXTENSION, false)
        }
        _ => (0, false),
    }
}

/// `CRATONVM_DEFINE_PLATFORM_LOADER_OWN_NAMESPACE` (zt t21/df9; default ON,
/// `=0` kill; every collector, both modes): the define doors put a class the
/// platform loader object defines in the `Extension` namespace
/// ([`define_loader_namespace`]), and a built-in loader's `findLoadedClass`
/// answers its own definition of a name before an ancestor's Java-defined one
/// (`classloader::builtin_own_definition_first`).
pub fn define_platform_loader_own_namespace() -> bool {
    true
}

/// `ClassLoader.defineClass1(ClassLoader, String, byte[], int, int, ProtectionDomain, String)в†’ Class`
///
/// Defines a class from a byte array. WP2.3: routes through
/// `define_class_full` so this entry point shares the same backend
/// (name-mismatch check, dup-define rejection, PD attribution) as
/// the other three (Unsafe.defineClass + Lookup.defineClass).
/// Pre-resolve a class's direct supertypes (superclass + interfaces) through the
/// DEFINING loader before `define_class_full` links them.
///
/// `class_manager::define_class` resolves a class's superclass/interfaces only
/// through CratonVM's global classpath (`load_class`) вЂ” it never calls back into
/// the user `ClassLoader` that is defining the class. That is wrong for a loader
/// whose classes live somewhere the global classpath cannot see: Tomcat's
/// `WebappClassLoader` serves `/WEB-INF/lib` jars from its `WebResourceRoot`, so
/// when it defines `org.apache.taglibs.standard.tlv.JstlCoreTLV` (a JSTL
/// `TagLibraryValidator`) the superclass `JstlBaseTLV` вЂ” in the SAME jar вЂ” is
/// invisible to the global store and the define fails (`ClassNotFound:
/// JstlBaseTLV`), 500-ing every JSP that triggers TLD validation
/// (`TestScopedAttributeELResolver`). JVMS В§5.3.5 makes the defining loader the
/// *initiating* loader for supertype resolution, so load each not-yet-loaded
/// supertype through it first; once present in the store, `define_class_full`
/// links cleanly.
///
/// Only fires for USER-DEFINED loaders and only for supertypes not already
/// loaded, so built-in/app-loader defines (ByteBuddy, cglib, the bootstrap
/// chain) вЂ” whose supertypes resolve from the classpath вЂ” are unaffected.
///
/// `Ok(Some(class file))` -- this pre-pass's parse of `bytes`, for the define
/// to use instead of parsing them again -- only when `keep_parse`
/// (`CRATONVM_DEFINE_ONE_PARSE`; interpreter round i3 wave 72, lane L5).
pub fn preload_supertypes_via_loader(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
    bytes: &[u8],
    keep_parse: bool,
    parsed: Option<PreparsedDefine>,
    answers: &mut SupertypeAnswers,
) -> Result<Option<PreparsedDefine>, cratonvm_types::error::MethodCallFailed> {
    // A bare `java.net.URLClassLoader` is an ordinary isolated loader (Quarkus's
    // `DevModeMediator` builds one over the deployment jars): its own `loadClass`
    // is the only route to a supertype that lives in its URLs, exactly as for a
    // user subclass. Without the pre-pass `define_class_full` looks the supertype
    // up on the global classpath and `NoClassDefFoundError`s for the first class
    // whose super is not yet defined.
    if !crate::classloader::is_user_defined_loader(ctx, loader_obj)
        && !crate::classloader::is_bare_url_class_loader(ctx, loader_obj)
    {
        return Ok(None);
    }
    // `parsed`: the caller's parse of `bytes` (`CRATONVM_DEFINE_PREPASS_OBJECT_ONLY`).
    let parse = match parsed {
        Some(parsed) => Ok(parsed),
        None => parse_for_supertype_link(bytes),
    };
    let (source, cf) = match parse {
        Ok(c) => c,
        Err(_) => return Ok(None), // malformed bytes вЂ” let define_class_full report it
    };
    let mut supertypes: Vec<String> = Vec::new();
    if let Some(s) = &cf.super_class {
        let n: &str = s;
        if !n.is_empty() && n != "java/lang/Object" {
            supertypes.push(n.to_string());
        }
    }
    for iface in &cf.interfaces {
        let n: &str = iface;
        if !n.is_empty() {
            supertypes.push(n.to_string());
        }
    }
    // `CRATONVM_DEFINE_ASKS_FOR_OBJECT`: an `Object` superclass is asked
    // too, after the loop (the superclass is asked last); not in `answers`.
    let ask_object = define_asks_for_object(&*ctx, &cf);
    if supertypes.is_empty() && !ask_object {
        return Ok(keep_parse.then_some((source, cf)));
    }
    answers.begin(&cf, supertypes.len());
    let p_loader = ctx.pin_native_root(loader_obj);
    let mut loader = loader_obj;
    // Loader-faithful gate: under CRATONVM_LOADER_AWARE_RESOLUTION the
    // "already loaded" short-circuit must be keyed on THIS loader's namespace,
    // not the global name index. An isolating/enhancing loader (Hibernate's
    // package-scoped `EnhancingClassLoader`) defines its OWN enhanced copy of an
    // in-package supertype; if a different loader already loaded the un-enhanced
    // copy, the global `class_id_by_name` probe would wrongly skip the drive and
    // the subclass would link the un-enhanced super (NoSuchMethodError on the
    // enhanced `$$_hibernate_*` accessors). Driving `loadClass` here defines the
    // loader's enhanced copy first, which `define_class_full` then prefers via
    // the (loader,name) exact link. Gate-off keeps the global short-circuit в†’
    // byte-identical.
    let loader_faithful = crate::classloader::loader_aware_resolution();
    // `CRATONVM_DEFINE_SUPERTYPE_DICTIONARY` (interpreter round i3 wave 72,
    // lane L5): a name this loader's `loadClass` already answered for a define
    // is not asked again (HotSpot's dictionary hit); the namespace keys it.
    let dictionary = ctx.define_supertype_dictionary();
    let loader_ns = if loader_faithful || dictionary {
        let ns = crate::classloader::loader_namespace_id(ctx, loader);
        loader = ctx.read_native_pin(p_loader, loader);
        ns
    } else {
        0
    };
    // `CRATONVM_DEFINE_RECORDS_SUPERTYPE_INITIATION` (wave 79, L5): the
    // loader is recorded as an initiating loader of each supertype it
    // resolves for this define.
    let record_ns = supertype_initiation_record_ns(ctx, loader);
    loader = ctx.read_native_pin(p_loader, loader);
    // JIT round 23 wave 10 (`CRATONVM_DEFINE_INTERFACES_BEFORE_SUPER`): ask in
    // HotSpot's order (the interfaces, then the superclass); `slot` keeps the
    // pre-pass's own order (superclass first) for `answers`.
    let has_super = cf
        .super_class
        .as_deref()
        .is_some_and(|s| !s.is_empty() && s != "java/lang/Object");
    for slot in supertype_ask_order(supertypes.len(), has_super) {
        let internal = &supertypes[slot];
        loader = ctx.read_native_pin(p_loader, loader);
        let present_id = if loader_faithful {
            ctx.class_id_defined_by_loader_exact(internal, loader_ns)
        } else {
            ctx.class_id_by_name(internal)
        };
        let present = present_id.is_some();
        answers.note_present(slot, present_id);
        let known = !present
            && dictionary
            && crate::classloader::supertype_link_known(ctx, loader_ns, internal);
        if known {
            // The class the loader answered when the row was filed, linked
            // by identity (wave 96, lane L5; `SupertypeLinks`).
            if let Some(id) = crate::classloader::supertype_link_answer(ctx, loader_ns, internal) {
                answers.note_known_answer(&*ctx, slot, id);
            }
        }
        let already = present || known;
        if already {
            // A supertype the flat store already holds is the class
            // `define_class_full` links: HotSpot's `resolve_super_or_fail`
            // would have asked the loader and filed the answer in its
            // dictionary. (Under the loader-faithful gate `present` is the
            // loader's own definition: nothing to record.)
            if let (Some(ns), true, false) = (record_ns, present, loader_faithful) {
                if let Err(e) = note_supertype_initiation(ctx, ns, internal, None) {
                    ctx.unpin_native_roots(p_loader);
                    return Err(e);
                }
            }
            continue; // already present for this loader вЂ” define_class_full links it
        }
        let dotted = internal.replace('/', ".");
        // The G1 near-full proxy-define page: the collecting door. This
        // body calls the loader's Java `loadClass` right below, which may
        // collect, so nothing it holds is unpinned here (the loader is
        // pinned and re-read). The one-attempt door refused at G1's reserve
        // and `defineClass1`, which is not restartable, threw
        // `OutOfMemoryError` (`Zs3Nd3UpcallMints proxy 4096 -Xmx64m`).
        let name_str = ctx.create_string_uninterned_gc_safe(&dotted); // may relocate `loader`
        loader = ctx.read_native_pin(p_loader, loader);
        let p_name = ctx.pin_native_root(name_str);
        loader = ctx.read_native_pin(p_loader, loader);
        let name_str = ctx.read_native_pin(p_name, name_str);
        // Best-effort: a genuine miss/throw is swallowed here and left for
        // `define_class_full` to surface as the spec-mandated linkage error.
        // Under the loader's monitor when it is not parallel-capable
        // (`--real-jdk`), as a VM-initiated load (wave 29, lane L5).
        let result = crate::classloader_real::invoke_load_class_as_the_vm(ctx, loader, name_str);
        // JIT round 20 lane superAcc: the class the loader answered is the
        // supertype the define links (HotSpot's `resolve_super_or_fail`); an
        // answer of another name is HotSpot's `NoClassDefFoundError`.
        if let Ok(Some(Value::Object(Some(mirror)))) = &result {
            if let Err(e) = answers.note_answer(&*ctx, slot, internal, *mirror) {
                ctx.unpin_native_roots(p_name);
                ctx.unpin_native_roots(p_loader);
                return Err(e);
            }
        }
        if let (true, Ok(Some(Value::Object(Some(mirror))))) = (dictionary, &result) {
            let answer = crate::lang_class::mirror_class_id(&*ctx, *mirror);
            crate::classloader::note_supertype_link(ctx, loader_ns, internal, answer);
        }
        if let (Some(ns), Ok(Some(Value::Object(Some(mirror))))) = (record_ns, &result) {
            if let Err(e) = note_supertype_initiation(ctx, ns, internal, Some(*mirror)) {
                ctx.unpin_native_roots(p_name);
                ctx.unpin_native_roots(p_loader);
                return Err(e);
            }
        }
        // Except a `ClassCircularityError`: the class being defined is
        // among its own supertypes, and nothing `define_class_full` can link
        // afterwards names that (it would report the half-defined class as
        // missing). HotSpot propagates it unchanged.
        if let Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(e)) = result {
            if is_class_circularity_error(ctx, e) {
                ctx.unpin_native_roots(p_loader);
                return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(e));
            }
            // JIT round 21 wave 26, lane nesting: a `ClassNotFoundException`
            // becomes the cause of the define's `NoClassDefFoundError`
            // (`supertype_miss_with_cause`). Read before anything allocates.
            answers.note_miss(ctx, internal, e);
        }
        loader = ctx.read_native_pin(p_loader, loader); // invoke may relocate
        ctx.unpin_native_roots(p_name);
    }
    if ask_object {
        // Best-effort, as above: a refusal is left to `define_class_full`.
        if let Err(e) = ask_defining_loader_for_object(ctx, p_loader, loader, loader_ns, record_ns)
        {
            ctx.unpin_native_roots(p_loader);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(p_loader);
    Ok(keep_parse.then_some((source, cf)))
}

/// `CRATONVM_DEFINE_RECORDS_SUPERTYPE_INITIATION` (interpreter round i3 wave
/// 79, lane L5; default OFF): the namespace id of `loader` when the define
/// pre-pass should record it as an initiating loader of the supertypes it
/// resolves -- the switch on, the initiating records on (`--real-jdk`, or
/// `CRATONVM_COMPAT_INITIATING_RECORDS`), and a user-defined namespace (>= 3).
/// HotSpot: `SystemDictionary::resolve_super_or_fail` -> `load_instance_class`
/// files the class the loader answered in that loader's dictionary
/// (`update_dictionary`) when the loader is not its defining loader, so
/// `L.findLoadedClass("java.io.Serializable")` answers the interface once `L`
/// defined a class implementing it
/// (`docs/feature-designs/native-dispatch-architecture.md`).
/// May allocate a namespace id: the caller re-reads its pinned loader.
pub fn supertype_initiation_record_ns(ctx: &mut dyn NativeContext, loader: ObjectRef) -> Option<u32> {
    if !ctx.define_records_supertype_initiation() || !ctx.initiating_records_on() {
        return None;
    }
    crate::classloader::peek_loader_namespace_id(ctx, loader)
        .or_else(|| Some(crate::classloader::loader_namespace_id(ctx, loader)))
        .filter(|&ns| ns >= 3)
}

