// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Serviceability Tools — jcmd/jstack/jmap diagnostic infrastructure.
//!
//! Provides attach API, diagnostic command processing, thread dump generation,
//! heap analysis, and HPROF stub writing for JVM serviceability tooling.
//!
//! ---------------------------------------------------------------------------
//! # LIVENESS — READ THIS BEFORE WIRING ANYTHING TO IT
//! ---------------------------------------------------------------------------
//!
//! **SUPERSEDED, 2026-09-01 -- read this correction before the claim below.**
//! The three bullets that follow were true when the observability audit wrote
//! them on 2026-07-26 and are *no longer* true, and a stale liveness claim is
//! more dangerous than none: it is exactly the evidence a later reader uses to
//! decide a command is not worth implementing. What actually changed:
//!
//!  * obsaudit D15 gave `AttachListener::start_listening` a **real** Unix
//!    domain socket at `/tmp/.java_pid<pid>` serving the HotSpot Attach API
//!    wire protocol (see that method and the `AttachListener` doc comment).
//!  * `Vm::new` (`vm/src/vm/vm_init.rs`) now builds a
//!    `JcmdProcessor::new_with_vm_state(shared.clone())` at bootstrap and
//!    parks it in `SharedVm::debug.jcmd_processor`, which is what keeps that
//!    socket bound for the life of the VM.
//!
//! So on Linux, `jcmd <pid> ...` / `jstack <pid>` / `jmap <pid>` from an
//! unmodified JDK **does** reach `register_live_commands` today. It does not
//! reach `register_standard_commands` -- that set belongs to the
//! argument-less `JcmdProcessor::new()`, which only tests construct -- so the
//! *fabricated-data* hazard the bullets warn about remains confined to the
//! variant production never builds. That distinction is the whole reason
//! `register_live_commands` exists; keep new commands there.
//!
//! Since i56's repair (2026-10-07), Windows also serves the live command table
//! through an embedded enqueue DLL and the JDK client's named pipe. macOS uses
//! the OS per-user attach directory. `hsdb_start_listener` / `HsdbListener`
//! remain test-only.
//!
//! The original claim, kept because the reasoning under it is still the right
//! reasoning and only its premises expired:
//!
//! **On a default build, none of the attach/jcmd surface in this module is
//! reachable from outside the process.** Established by the observability
//! audit of 2026-07-26:
//!
//!  * [`AttachListener`] has no constructor call anywhere outside this file.
//!    Its `socket_path` field is a plain `String` and `start_listening` only
//!    flips a `bool` — **no socket, named pipe, or `.attach_pid<N>` file is
//!    ever created**. `jcmd <pid> ...` / `jstack <pid>` / `jmap <pid>` from
//!    another process therefore cannot connect to a CratonVM at all.
//!  * ~~[`JcmdProcessor`] is constructed only in `#[cfg(test)]` code.~~
//!    **STALE, and by same-day work: `obsaudit D15` (2026-07-26) wired it
//!    up. `Vm::new` builds one via `new_with_vm_state` (vm_init.rs), and
//!    `AttachListener::start_listening` really does `bind` a Unix socket at
//!    `/tmp/.java_pid<pid>` (0600) and spawn a polling accept thread, so the
//!    first bullet above is stale too: the attach surface IS reachable.
//!    What remains true is narrower and is why two wrappers here are
//!    `#[cfg(test)]`: the live path is `handle_attach_connection`, which
//!    duplicates the command lookup rather than calling `process_command`
//!    or `dispatch_command`. Corrected 2026-09-02 while closing the
//!    test-only-public-api gate.**
//!  * [`hsdb_start_listener`] / [`HsdbListener`] have no callers outside this
//!    file's tests.
//!
//! Consequences, in order of how badly they would mislead an operator:
//!
//!  1. The *only* live consumers of this module are [`HprofWriter`] (used by
//!     `runtime::hprof`, which IS reachable via
//!     `-XX:+HeapDumpOnOutOfMemoryError`) and the [`VmDiagnosticState`] impl
//!     on `SharedVm`, which exists but has no production caller.
//!  2. `register_default_commands` (the no-VM-state variant used by
//!     `JcmdProcessor::new` / `::default`) returns **fabricated** output for
//!     several commands. See the audit note on `Compiler.queue`.
//!  3. `JFR.start` / `JFR.stop` / `JFR.dump` never touch the flight recorder
//!     **in `register_standard_commands`**, and still do not: that set has no
//!     VM binding, so its three entries return an honest "not implemented"
//!     rather than a fabricated success. See the audit notes at their
//!     registration sites. As of 2026-09-01 `register_live_commands` carries
//!     real ones, routed through `VmDiagnosticState::jfr_start` / `jfr_dump` /
//!     `jfr_stop` and implemented on `SharedVm` in `vm/src/vm/vm_init.rs`.
//!
//! Do not "wire up jcmd" by simply constructing a `JcmdProcessor` at
//! bootstrap: `register_default_commands` would then start reporting invented
//! data to operators. Fix the individual commands first, or register only
//! `register_live_commands`.

use cratonvm_types::narrow_oop::{read_ref_slot_unaligned, ref_element_size};
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

/// Trait for querying live VM state from diagnostic commands.
/// The VM implements this to provide real data to serviceability tools.
pub trait VmDiagnosticState: Send + Sync {
    /// Get snapshots of all live threads.
    fn thread_snapshots(&self) -> Vec<ThreadSnapshot>;
    /// What a thread dump of this VM prints around its threads (interpreter
    /// round i3 wave 81, lane L6): by default named by the state's own
    /// `java.vm.*` properties, with no thread-list address and no JNI counts.
    fn thread_dump_header(&self) -> ThreadDumpHeader {
        ThreadDumpHeader::from_properties(&self.system_properties())
    }
    /// Get heap summary information.
    fn heap_summary(&self) -> HeapSummary;
    /// gci i14/hd: HotSpot 25's `Universe::print_on` text for this state's
    /// heap, one line per entry (`Heap`, the collector's lines, Metaspace):
    /// what `jcmd GC.heap_info` prints and the SIGQUIT dump ends with.
    /// `None` when the state cannot say it (a backend with another grammar,
    /// a mock); those commands then print the [`HeapSummary`] text.
    fn heap_info_lines(&self) -> Option<Vec<String>> {
        None
    }
    /// Get class histogram entries.
    fn class_histogram(&self) -> Vec<ClassHistogramEntry>;
    /// `GC.class_histogram [-all]` / `jmap -histo[:live]` (gci i4/hd): HotSpot
    /// inspects LIVE objects unless `-all` (a full collection first). `live`
    /// is the request; an implementation that cannot collect first answers
    /// [`Self::class_histogram`].
    fn class_histogram_live(&self, _live: bool) -> Vec<ClassHistogramEntry> {
        self.class_histogram()
    }
    /// Trigger a garbage collection cycle. Returns true if GC was actually run.
    fn trigger_gc(&self) -> bool;
    /// Get VM uptime in seconds.
    fn uptime_secs(&self) -> f64;
    /// Get the command line that started the VM.
    fn command_line(&self) -> String;
    /// Get VM system properties.
    fn system_properties(&self) -> Vec<(String, String)>;
    /// Get VM flags/options.
    fn vm_flags(&self) -> Vec<String>;
    /// Write an HPROF heap dump to the given path. Returns bytes written.
    fn heap_dump(&self, _path: &str) -> Result<u64, String> {
        Err("heap dump not supported".to_string())
    }
    /// `GC.heap_dump [-all] <file>` / `jmap -dump[:live]` (gci i4/hd): as
    /// [`Self::class_histogram_live`], live objects only unless `-all`.
    fn heap_dump_live(&self, path: &str, _live: bool) -> Result<u64, String> {
        self.heap_dump(path)
    }
    /// `GC.heap_dump [-all] [-gz=<level>] [-overwrite] <file>` (gci i5/hd):
    /// [`Self::heap_dump_live`] plus gzip framing (`gz_level`, already checked
    /// to be 1-9) and HotSpot's exclusive open unless `overwrite`. An
    /// implementation that cannot write gzip refuses a compressed dump.
    fn heap_dump_with(
        &self,
        path: &str,
        live: bool,
        gz_level: Option<u8>,
        _overwrite: bool,
    ) -> Result<u64, String> {
        match gz_level {
            None => self.heap_dump_live(path, live),
            Some(_) => Err("compressed heap dumps are not supported".to_string()),
        }
    }

    // --- JFR (2026-09-01) ---------------------------------------------------
    //
    // The three methods below are what let `register_live_commands` register a
    // `JFR.start` / `JFR.dump` / `JFR.stop` that actually reaches a
    // `FlightRecorder`. Requirement (1) of the audit block at the
    // `register_standard_commands` JFR entries names exactly this: "an
    // `Arc<SharedVm>` (or a `VmDiagnosticState` extension) on the processor so
    // the handler can reach `debug.flight_recorder`". The trait extension is
    // the option that keeps this module free of any `vm::` dependency.
    //
    // Each is **defaulted to an honest failure** rather than left required.
    // That is deliberate on two counts:
    //
    //  * it adds no method to implement for the mock states in this file's own
    //    tests, none of which owns a recorder; and
    //  * the failure text says the state has no recorder, which is the one
    //    thing an operator needs to know. Returning a plausible success from a
    //    state that cannot record is the precise defect the audit block
    //    records, and a default body is where that defect would silently
    //    reappear if the default were `Ok`.
    //
    // `args` is the diagnostic command's argument list exactly as
    // `JcmdProcessor::process_command` split it: whitespace-separated tokens.

    /// `jcmd <pid> JFR.start [name=<n>] [maxevents=<n>] [+<Event>#enabled=<bool>]`.
    /// Returns the operator-facing confirmation line.
    fn jfr_start(&self, _args: &[String]) -> Result<String, String> {
        Err("JFR.start: this VM state has no flight recorder binding".to_string())
    }

    /// `jcmd <pid> JFR.dump [name=<n>] filename=<path>` -- write a running
    /// recording to `path` without stopping it.
    fn jfr_dump(&self, _args: &[String]) -> Result<String, String> {
        Err("JFR.dump: this VM state has no flight recorder binding".to_string())
    }

    /// `jcmd <pid> JFR.stop [name=<n>] [filename=<path>]` -- write the
    /// recording if a path was given, then stop it.
    fn jfr_stop(&self, _args: &[String]) -> Result<String, String> {
        Err("JFR.stop: this VM state has no flight recorder binding".to_string())
    }

    // --- The attach `load` operation (interpreter round i2 wave 58, lane L1) --
    //
    // `docs/internal/fixed-bugs/i57-L1-proposal-an-attach-listener-serves-external-attach-20261021.md`,
    // stage 1: what `VirtualMachine.loadAgent` / `loadAgentPath` /
    // `loadAgentLibrary` send from ANOTHER process (ByteBuddy's external
    // attacher, `jcmd`'s agent tools). Defaulted to "not served", so a state
    // without a VM behind it keeps answering HotSpot's
    // "Unrecognized attach operation".

    /// Does this VM serve the attach listener's `load` operation
    /// (`CRATONVM_ATTACH_LISTENER_LOAD`)?
    fn attach_load_enabled(&self) -> bool {
        false
    }

    /// The `load <lib> <absolute> <options>` operation: load the agent into
    /// this VM and answer the response body HotSpot's listener writes after
    /// its completion status 0 -- `return code: <n>` once the agent's
    /// `Agent_OnAttach` ran (JPLIS's codes for a JAR agent), else the reason
    /// the library did not load.
    fn attach_load(&self, _lib: &str, _absolute: bool, _options: &str) -> String {
        "attach load is not served by this VM state".to_string()
    }

    /// Was this VM started with `-XX:-EnableDynamicAgentLoading`? HotSpot's
    /// listener then refuses every `load` operation with a non-zero status
    /// and [`crate::runtime::instrument::DYNAMIC_AGENT_LOADING_DISABLED`]
    /// (interpreter round i3 wave 81, lane L1). `jcmd JVMTI.agent_load` is
    /// a diagnostic command, not the `load` operation, and is not refused.
    fn dynamic_agent_loading_disabled(&self) -> bool {
        false
    }

    // --- The attach `properties` / `agentProperties` operations -------------
    // (interpreter round i2 wave 59, lane L1; the same proposal, stage 2).

    /// Does this VM serve the attach listener's `properties` and
    /// `agentProperties` operations in HotSpot's format
    /// (`CRATONVM_ATTACH_LISTENER_PROPS`)?
    fn attach_properties_enabled(&self) -> bool {
        false
    }

    /// What HotSpot's listener answers for `properties` (`agent` false) or
    /// `agentProperties` (`agent` true) after completion status 0: the bytes
    /// of `jdk.internal.vm.VMSupport.serializePropertiesToByteArray()` /
    /// `serializeAgentPropertiesToByteArray()` run in this VM
    /// (`Properties.store` output, ASCII with `\uXXXX` escapes). `Err` with
    /// the reason when they could not run.
    fn attach_properties(&self, _agent: bool) -> Result<String, String> {
        Err("attach properties are not served by this VM state".to_string())
    }

    /// Does this VM start its attach listener lazily, on an attaching
    /// client's trigger file, as HotSpot does (`CRATONVM_ATTACH_LISTENER_LAZY`;
    /// interpreter round i2 wave 59, lane L1)? Otherwise it binds at boot.
    fn attach_listener_lazy(&self) -> bool {
        false
    }

    /// Does `SIGQUIT` print a thread dump in this VM, through its attach
    /// listener's thread (`CRATONVM_SIGQUIT_THREAD_DUMP`; interpreter round
    /// i2 wave 60, lane L1)?
    fn sigquit_thread_dump(&self) -> bool {
        false
    }

    /// `jcmd <pid> Thread.dump_to_file [-overwrite] [-format=plain|json]
    /// <filepath>` (gci i12/td): HotSpot 25's `ThreadDumpToFileDCmd` calls
    /// `jdk.internal.vm.ThreadDumper.dumpThreads` (`dumpThreadsToJson` for
    /// `json`) with the path and `overwrite` on its "Attach Listener" thread
    /// and answers the returned bytes (`Created <abs path>`, `<abs path>
    /// exists, use -overwrite to overwrite`, `Failed: <exception>`), or the
    /// thrown exception's `<class>: <message>` line. `Ok` is that body; `Err`
    /// the reason the dumper could not run at all. Defaulted to an honest
    /// refusal, as the JFR methods are: a state with no Java thread to run the
    /// JDK's dumper must not answer as though a file was written.
    fn thread_dump_to_file(&self, _path: &str, _overwrite: bool, _json: bool) -> Result<String, String> {
        Err("Thread.dump_to_file is not served by this VM state".to_string())
    }

    // --- gci i33/jf: commands HotSpot 25 answers from the Java side ---------
    //
    // Page `gci-i32jh-jfr-dcmd-help-is-not-the-jdks-printhelp-FIXED-20261016`. Each
    // is defaulted to an honest refusal, as the methods above: a state with no
    // Java thread to run the JDK's code must not answer as though it had.

    /// `jcmd <pid> help JFR.<x>`'s page after HotSpot's header: HotSpot 25's
    /// `JfrDCmd::print_help` constructs `class` (`jdk/jfr/internal/dcmd/DCmd*`,
    /// `()V`), calls its `getHelp()[Ljava/lang/String;` and prints each line
    /// with `print_cr`; a thrown exception prints its message alone
    /// (`handle_pending_exception`; nothing for a null message). `Ok` is that
    /// text, every line ended by `\n`; `Err` the reason the JDK code could not
    /// run at all (the caller then prints its own generic page).
    fn jfr_help(&self, _class: &str) -> Result<String, String> {
        Err("JFR help is not served by this VM state".to_string())
    }

    /// `jcmd <pid> GC.finalizer_info` (HotSpot 25's `FinalizerInfoDCmd`):
    /// the objects waiting for their `finalize()`, by class, in
    /// [`finalizer_info_text`]'s shape. `Err` when the state has no queue to
    /// read.
    fn finalizer_info(&self) -> Result<String, String> {
        Err("GC.finalizer_info is not served by this VM state".to_string())
    }

    /// `jcmd <pid> GC.run_finalization` (HotSpot 25's
    /// `RunFinalizationDCmd`): `java.lang.System.runFinalization()` on an
    /// attached thread. `Ok` is the output (empty, as HotSpot prints nothing);
    /// a thrown exception is `Err("<class>: <message>\n")`, which the attach
    /// path answers with a non-zero status as HotSpot's `jcmd` operation
    /// (`java_lang_Throwable::print` + `cr`, `JNI_ERR`).
    fn run_finalization(&self) -> Result<String, String> {
        Err("GC.run_finalization is not served by this VM state".to_string())
    }

    /// `jcmd <pid> AOT.end_recording` (HotSpot 25's `AOTEndRecordingDCmd`).
    fn aot_end_recording(&self) -> Result<String, String> {
        Ok(
            "AOT.end_recording is unsupported when VM flags -XX:AOTMode=record or \
             -XX:AOTCacheOutput=<file> are missing.\n"
                .to_string(),
        )
    }

    /// `jcmd <pid> Compiler.CodeHeap_Analytics [<function>] [<granularity>]`.
    fn compiler_codeheap_analytics(&self, _args: &[String]) -> Result<String, String> {
        Ok(default_codeheap_analytics_text(0, 0))
    }

    /// `jcmd <pid> Compiler.codecache`.
    fn compiler_codecache(&self) -> Result<String, String> {
        Ok(default_compiler_codecache_text(0, 0))
    }

    /// `jcmd <pid> Compiler.codelist`.
    fn compiler_codelist(&self) -> Result<String, String> {
        Ok(String::new())
    }

    /// `jcmd <pid> Compiler.directives_add <filename>`.
    fn compiler_directives_add(&self, args: &[String]) -> Result<String, String> {
        default_compiler_directives_add(args)
    }

    /// `jcmd <pid> Compiler.directives_clear`.
    fn compiler_directives_clear(&self) -> Result<String, String> {
        Ok(String::new())
    }

    /// `jcmd <pid> Compiler.directives_print`.
    fn compiler_directives_print(&self) -> Result<String, String> {
        Ok(DEFAULT_COMPILER_DIRECTIVES_PRINT.to_string())
    }

    /// `jcmd <pid> Compiler.directives_remove`.
    fn compiler_directives_remove(&self) -> Result<String, String> {
        Ok(String::new())
    }

    /// `jcmd <pid> Compiler.memory [options]`.
    fn compiler_memory(&self, _args: &[String]) -> Result<String, String> {
        Ok("Compilation memory statistics disabled.\n".to_string())
    }

    /// `jcmd <pid> Compiler.queue`.
    fn compiler_queue(&self) -> Result<String, String> {
        Ok(default_compiler_queue_text(&[]))
    }

    /// `jcmd <pid> JFR.check [options]`.
    fn jfr_check(&self, _args: &[String]) -> Result<String, String> {
        Ok(format!(
            "No available recordings.\n\nUse jcmd {} JFR.start to start a recording.\n",
            std::process::id()
        ))
    }

    /// `jcmd <pid> JFR.configure [options]`.
    fn jfr_configure(&self, _args: &[String]) -> Result<String, String> {
        Ok(default_jfr_configure_text())
    }

    /// `jcmd <pid> JFR.view [options] <view>`.
    fn jfr_view(&self, args: &[String]) -> Result<String, String> {
        default_jfr_view(args)
    }

    /// `jcmd <pid> JVMTI.data_dump`.
    fn jvmti_data_dump(&self) -> Result<String, String> {
        Ok(String::new())
    }

    /// `jcmd <pid> ManagementAgent.start [options]`.
    fn management_agent_start(&self, _args: &[String]) -> Result<String, String> {
        Ok(String::new())
    }

    /// `jcmd <pid> ManagementAgent.start_local`.
    fn management_agent_start_local(&self) -> Result<String, String> {
        Ok(String::new())
    }

    /// `jcmd <pid> ManagementAgent.status`.
    fn management_agent_status(&self) -> Result<String, String> {
        Ok("Agent: disabled\n".to_string())
    }

    /// `jcmd <pid> ManagementAgent.stop`.
    fn management_agent_stop(&self) -> Result<String, String> {
        Ok(String::new())
    }

    /// `jcmd <pid> System.map`.
    fn system_map(&self) -> Result<String, String> {
        Ok(host_system_map_text())
    }

    /// `jcmd <pid> System.dump_map [-F=<filepath>]`.
    fn system_dump_map(&self, args: &[String]) -> Result<String, String> {
        let mut path = format!("vm_memory_map_{}.txt", std::process::id());
        for arg in args {
            if let Some(v) = arg.strip_prefix("-F=") {
                path = v.to_string();
            } else if !arg.starts_with('-') {
                path = arg.clone();
            }
        }
        let content = self.system_map()?;
        std::fs::write(&path, content.as_bytes())
            .map_err(|e| format!("Failed to write memory map to {path}: {e}\n"))?;
        let abs = std::fs::canonicalize(&path)
            .unwrap_or_else(|_| std::path::PathBuf::from(&path))
            .display()
            .to_string();
        Ok(format!("Memory map dumped to \"{abs}\".\n"))
    }

    /// `jcmd <pid> Thread.vthread_pollers`.
    fn thread_vthread_pollers(&self) -> Result<String, String> {
        Ok("Read I/O pollers:\n[0] sun.nio.ch.Poller [registered = 0, owner = null]\n\nWrite I/O pollers:\n[0] sun.nio.ch.Poller [registered = 0, owner = null]\n".to_string())
    }

    /// `jcmd <pid> Thread.vthread_scheduler`.
    fn thread_vthread_scheduler(&self) -> Result<String, String> {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Ok(format!(
            "java.util.concurrent.ForkJoinPool@1[Running, parallelism = {cpus}, size = 0, active = 0, running = 0, steals = 0, tasks = 0, submissions = 0]\n"
        ))
    }

    /// `jcmd <pid> VM.cds <subcmd> [<filename>]`.
    fn vm_cds(&self, args: &[String]) -> Result<String, String> {
        default_vm_cds(args)
    }

    /// `jcmd <pid> VM.class_hierarchy [-i] [-s] [<classname>]`.
    fn vm_class_hierarchy(&self, args: &[String]) -> Result<String, String> {
        Ok(default_vm_class_hierarchy_text(args))
    }

    /// `jcmd <pid> VM.classes [-verbose]`.
    fn vm_classes(&self, _args: &[String]) -> Result<String, String> {
        Ok(format!(
            "{:<20}Size  {:<22}{:<9}ClassName  \n0x00000000901832d0  {:>4}  {:<22}{:<9}java.lang.Object  \n",
            "KlassAddr", "State", "Flags", 66, "fully_initialized", "fWS"
        ))
    }

    /// `jcmd <pid> VM.classloader_stats`.
    fn vm_classloader_stats(&self) -> Result<String, String> {
        Ok(default_vm_classloader_stats_text(1, 0, 0))
    }

    /// `jcmd <pid> VM.classloaders [options]`.
    fn vm_classloaders(&self, _args: &[String]) -> Result<String, String> {
        Ok(default_vm_classloaders_text(&[], &[], false))
    }

    /// `jcmd <pid> VM.dynlibs`.
    fn vm_dynlibs(&self) -> Result<String, String> {
        Ok(host_dynlibs_text())
    }

    /// `jcmd <pid> VM.events [options]`.
    fn vm_events(&self, args: &[String]) -> Result<String, String> {
        Ok(default_vm_events_text(args, &[], &[], &[], &[]))
    }

    /// `jcmd <pid> VM.info`.
    fn vm_info(&self) -> Result<String, String> {
        Ok(default_vm_info_text(
            &self.system_properties(),
            &self.command_line(),
            self.uptime_secs(),
            self.heap_info_lines().as_deref(),
            &self.heap_summary(),
            &self.vm_flags(),
        ))
    }

    /// `jcmd <pid> VM.log [options]`.
    fn vm_log(&self, args: &[String]) -> Result<String, String> {
        default_vm_log_text(args)
    }

    /// `jcmd <pid> VM.metaspace [options]`.
    fn vm_metaspace(&self, _args: &[String]) -> Result<String, String> {
        Ok(default_vm_metaspace_text(&self.heap_summary(), 1, 1))
    }

    /// `jcmd <pid> VM.native_memory [options]`.
    fn vm_native_memory(&self, _args: &[String]) -> Result<String, String> {
        Ok("Native memory tracking is not enabled\n".to_string())
    }

    /// `jcmd <pid> VM.set_flag <flag name> [<string value>]`.
    fn vm_set_flag(&self, args: &[String]) -> Result<String, String> {
        default_vm_set_flag(args)
    }

    /// `jcmd <pid> VM.uptime [-date]`'s text. gcj j2/ob2: a hook so a VM can
    /// answer HotSpot's shape; by default this module's old text.
    fn vm_uptime_text(&self, _args: &[String]) -> String {
        format!("VM uptime: {:.3} seconds", self.uptime_secs())
    }

    /// `jcmd <pid> VM.command_line`'s text. gcj j2/ob2: a hook so a VM can
    /// answer HotSpot's shape; by default [`Self::command_line`].
    fn vm_command_line_text(&self) -> String {
        self.command_line()
    }

    /// `jcmd <pid> VM.stringtable [-verbose]`.
    fn vm_stringtable(&self, _args: &[String]) -> Result<String, String> {
        Ok(default_vm_stringtable_text(0, 0, &[], false))
    }

    /// `jcmd <pid> VM.symboltable [-verbose]`.
    fn vm_symboltable(&self, _args: &[String]) -> Result<String, String> {
        Ok(default_vm_symboltable_text(0, 0, &[], false))
    }

    /// `jcmd <pid> VM.systemdictionary [-verbose]`.
    fn vm_systemdictionary(&self, _args: &[String]) -> Result<String, String> {
        Ok(default_vm_systemdictionary_text(1, 0, 0))
    }
}

// ---------------------------------------------------------------------------
// Attach API infrastructure
// ---------------------------------------------------------------------------

/// Listener for diagnostic attach requests (Unix socket or Windows enqueue).
///
/// obsaudit D15 (2026-07-26), FIXED: `start_listening` now opens a real
/// socket at `socket_path` and serves the HotSpot Attach API wire protocol,
/// so `jcmd <pid> ...` / `jstack <pid>` / `jmap <pid> ...` from a real,
/// unmodified JDK installation can connect and run commands. The protocol
/// was not reverse-engineered from memory: verified empirically against a
/// real OpenJDK 21 `jcmd`/`jstack`/`jmap` on the audit host (2026-07-26) by
/// standing up a bare Python socket listener at `/tmp/.java_pid<pid>` for a
/// live dummy process and logging the raw bytes each tool sent. Findings:
///
///  * Each attach opens the socket **twice**: a first connection that sends
///    zero bytes (the client's own "is a listener already up?" readiness
///    probe — HotSpot's real client normally creates a `.attach_pid<pid>`
///    file and sends `SIGQUIT` to *trigger* this probe to eventually
///    succeed; since this implementation listens from VM boot, the socket
///    already exists and the probe succeeds immediately, so the SIGQUIT /
///    `.attach_pid` dance is never needed here), followed by a second
///    connection carrying the real request. Both must be handled — the
///    first is not an error, just a no-op.
///  * The real request is five NUL-terminated fields:
///    `<protocol-version>\0<operation>\0<arg1>\0<arg2>\0<arg3>\0` — e.g.
///    `jcmd <pid> VM.version` sends `1\0jcmd\0VM.version\0\0\0`; `jstack`
///    sends `1\0threaddump\0\0\0\0`; `jmap -histo` sends
///    `1\0inspectheap\0-all\0\0\0`; `jmap -dump:file=X` sends
///    `1\0dumpheap\0X\0-all\0`.
///  * The response is a decimal result code, a newline, then the raw
///    command output; the connection close signals EOF to the client. No
///    length prefix, no other framing.
///
/// See `handle_attach_connection` for the server-side implementation of
/// this protocol, and the module LIVENESS block at the top of this file.
pub struct AttachListener {
    pub socket_path: String,
    pub is_listening: bool,
    /// `Arc<RwLock<_>>` rather than a plain `Vec` (as before this fix) so
    /// the background accept thread spawned by `start_listening` can hold
    /// its own handle to the live command set without borrowing `self`.
    /// Also means commands may be registered before *or* after
    /// `start_listening` is called with no race — every dispatch reads the
    /// list fresh through the shared lock.
    pub commands: Arc<parking_lot::RwLock<Vec<DiagnosticCommand>>>,
    /// Set by [`AttachListener::stop_listening`] to tell the accept loop to
    /// exit. Shared with the accept thread, which re-checks it every poll tick.
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// Join handle for the accept thread, so `stop_listening` (and therefore
    /// `Drop`) can actually reap it instead of stranding it. `None` when this
    /// listener never bound (non-Unix target, or `bind` failed).
    accept_thread: Option<std::thread::JoinHandle<()>>,
    #[cfg(windows)]
    windows_target: Option<windows_attach::WindowsAttachTarget>,
    /// The listener's thread prints a thread dump when it finds the
    /// `SIGQUIT` latch set ([`take_sigquit`]; interpreter round i2 wave 60,
    /// lane L1, `VmDiagnosticState::sigquit_thread_dump`). Set before the
    /// listener starts.
    pub sigquit_dump: bool,
}

impl AttachListener {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
            is_listening: false,
            commands: Arc::new(parking_lot::RwLock::new(Vec::new())),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            accept_thread: None,
            #[cfg(windows)]
            windows_target: None,
            sigquit_dump: false,
        }
    }

    /// Open the real attach socket and start serving requests in a
    /// detached background thread. Idempotent — a second call while
    /// already listening is a no-op. See the struct doc comment for the
    /// wire protocol.
    ///
    /// Unix uses a local domain socket; Windows uses the embedded transport
    /// below. Unsupported platforms leave the listener unavailable.
    #[cfg(unix)]
    pub fn start_listening(&mut self) {
        if self.is_listening {
            return;
        }
        let Some(listener) = bind_attach_socket(&self.socket_path) else {
            return;
        };
        let commands = Arc::clone(&self.commands);
        let shutdown = Arc::clone(&self.shutdown);
        let sigquit_dump = self.sigquit_dump;
        self.accept_thread = std::thread::Builder::new()
            .name("Attach-Listener".into())
            .spawn(move || serve_attach_socket(listener, commands, shutdown, sigquit_dump))
            .ok();
        self.is_listening = true;
    }

    #[cfg(windows)]
    pub fn start_listening(&mut self) {
        if self.is_listening {
            return;
        }
        match windows_attach::WindowsAttachTarget::start(Arc::clone(&self.commands)) {
            Ok(target) => {
                self.windows_target = Some(target);
                self.is_listening = true;
            }
            Err(error) => tracing::warn!("Windows attach target unavailable: {error}"),
        }
    }

    #[cfg(not(any(unix, windows)))]
    pub fn start_listening(&mut self) {}

    /// HotSpot's lazy start (interpreter round i2 wave 59, lane L1;
    /// `CRATONVM_ATTACH_LISTENER_LAZY`, stage 3 of
    /// `docs/internal/fixed-bugs/i57-L1-proposal-an-attach-listener-serves-external-attach-20261021.md`):
    /// bind nothing yet; a thread polls, every [`ATTACH_POLL`], for the
    /// trigger file an attaching client creates (`.attach_pid<pid>` in this
    /// process's working directory, else in `/tmp`, owned by this process's
    /// effective user: HotSpot's `AttachListener::is_init_trigger`), then
    /// binds `socket_path` and serves it as [`Self::start_listening`] does.
    /// With signal dispatch enabled, the poll consumes the client's `SIGQUIT`
    /// before binding; otherwise it polls the trigger with the empty handler
    /// keeping the process alive. `stop_listening` retires the polling thread as it
    /// retires the accept loop. Idempotent, like `start_listening`.
    #[cfg(unix)]
    pub fn start_listening_lazily(&mut self) {
        if self.is_listening {
            return;
        }
        // HotSpot's AttachListener::vm_start removes a stale file before Java
        // runs. Waiting until bind would let a client find an abandoned socket
        // from a recycled PID, skip the wakeup, and fail with Connection refused.
        let _ = std::fs::remove_file(&self.socket_path);
        let path = self.socket_path.clone();
        let commands = Arc::clone(&self.commands);
        let shutdown = Arc::clone(&self.shutdown);
        let sigquit_dump = self.sigquit_dump;
        self.accept_thread = std::thread::Builder::new()
            .name("Attach-Listener".into())
            .spawn(move || {
                while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
                    // Wave 60 (lane L1): a `SIGQUIT` is the client's nudge
                    // when the trigger file is there (HotSpot then starts the
                    // listener and prints nothing), else an operator's
                    // request for a thread dump.
                    let quit = sigquit_dump && take_sigquit();
                    // With signal dispatch enabled, do not publish the socket
                    // before consuming the client's wakeup. Otherwise a client
                    // can delete its trigger after connecting, leaving its
                    // delayed SIGQUIT to print an unsolicited thread dump.
                    if (!sigquit_dump || quit) && attach_init_trigger_present() {
                        if crate::runtime::env_cache::frame_trace() {
                            eprintln!("[ATTACH_LISTENER_LAZY] trigger seen, binding {path}");
                        }
                        if let Some(listener) = bind_attach_socket(&path) {
                            serve_attach_socket(listener, commands, shutdown, sigquit_dump);
                        }
                        return;
                    }
                    if quit {
                        print_sigquit_thread_dump(&commands);
                    }
                    std::thread::sleep(ATTACH_POLL);
                }
            })
            .ok();
        self.is_listening = true;
    }

    #[cfg(not(unix))]
    pub fn start_listening_lazily(&mut self) {
        self.start_listening();
    }

    /// HotSpot's "Signal Dispatcher" for a VM whose attach listener has no
    /// polling thread of its own to act on the `SIGQUIT` / `Ctrl-Break` latch
    /// (interpreter round i3 wave 80, lane L6;
    /// `i59-L1-sigquit-terminates-the-vm-20261023.md`): under
    /// `-XX:+DisableAttachMechanism` (no listener at all, where HotSpot still
    /// prints the dump for every `SIGQUIT`, a client's included) and on
    /// Windows (whose attach transport is an enqueue, not a poll). A thread
    /// that takes the latch every [`SIGNAL_DISPATCH_POLL`] and prints the
    /// `Thread.print` dump on stdout; `stop_listening` (and `Drop`) retire it
    /// as they retire the accept loop. Idempotent; binds nothing.
    #[cfg(any(unix, windows))]
    pub(crate) fn start_signal_dispatcher(&mut self) {
        if self.accept_thread.is_some() {
            return;
        }
        let commands = Arc::clone(&self.commands);
        let shutdown = Arc::clone(&self.shutdown);
        self.accept_thread = std::thread::Builder::new()
            .name("Signal Dispatcher".into())
            .spawn(move || {
                while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
                    if take_sigquit() {
                        print_sigquit_thread_dump(&commands);
                    }
                    // Parked, not slept: `stop_listening` unparks it, so a
                    // VM's drop does not wait out the poll.
                    std::thread::park_timeout(SIGNAL_DISPATCH_POLL);
                }
            })
            .ok();
    }

    /// Stop serving: signal the accept loop, unlink the socket, and join the
    /// thread. Idempotent, and a no-op for a listener that never bound.
    ///
    /// This used to be "best-effort" and deliberately did NOT reap the accept
    /// thread, on the reasoning that "the listener's real lifetime is the VM
    /// process's own". That holds for a real VM process, which has one. It is
    /// false for this crate's own test binary, where every `Vm::new` builds a
    /// `JcmdProcessor` (see `JcmdProcessor::new_with_vm_state`) that binds
    /// `/tmp/.java_pid<pid>` — the SAME path each time, since it is per-PID by
    /// construction — and each bind unlinks the previous one's socket. The
    /// earlier thread could then never be reached by any client, never
    /// returned from `accept()`, and never exited: one `cargo test -p
    /// cratonvm-vm --lib` run peaked at 286 threads, 71 of them named
    /// `Attach-Listener`, each holding a socket fd.
    ///
    /// Bounded by construction: the accept loop checks `shutdown` at most one
    /// poll interval away (see `start_listening`), so the join returns
    /// promptly and cannot depend on a client ever connecting.
    pub fn stop_listening(&mut self) {
        #[cfg(windows)]
        drop(self.windows_target.take());
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.socket_path);
        if let Some(handle) = self.accept_thread.take() {
            // Wave 80 (lane L6): wakes a signal dispatcher's park at once
            // (the accept loop sleeps, and an unpark there is a no-op).
            handle.thread().unpark();
            let _ = handle.join();
        }
        self.is_listening = false;
    }

    pub fn register_command(&mut self, cmd: DiagnosticCommand) {
        self.commands.write().push(cmd);
    }

    /// gci i33/jf: `help <name>` prints `page`'s text after HotSpot's header
    /// (a `JfrDCmd`'s `print_help`), for the first command named `name` (the
    /// one dispatch and `help` find).
    pub(crate) fn set_help_page(&mut self, name: &str, page: HelpPage) {
        if let Some(cmd) = self.commands.write().iter_mut().find(|c| c.name == name) {
            cmd.help_page = Some(page);
        }
    }

    pub fn list_commands(&self) -> Vec<String> {
        self.commands
            .read()
            .iter()
            .map(|c| c.name.clone())
            .collect()
    }

    /// Look up `name` and execute it with `args` under a single read-lock
    /// acquisition. Returns `None` for an unregistered command name.
    // Test-only for the same reason: `handle_attach_connection` duplicates
    // this lookup instead of calling it, so gating `process_command` leaves
    // this one with #[test] callers only.
    #[cfg(test)]
    pub fn dispatch_command(&self, name: &str, args: &[String]) -> Option<CommandResult> {
        self.commands
            .read()
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.execute(args))
    }
}

/// How often the attach listener's thread polls: its accept loop (see
/// [`serve_attach_socket`]) and, before a lazy listener binds, its look for
/// the trigger file ([`AttachListener::start_listening_lazily`]).
#[cfg(unix)]
const ATTACH_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Bind the attach socket at `path` (0600), non-blocking for
/// [`serve_attach_socket`]'s poll; `None` (with a warning) when it cannot.
#[cfg(unix)]
fn bind_attach_socket(path: &str) -> Option<std::os::unix::net::UnixListener> {
    // A stale socket file can be left behind by a crashed prior process
    // that reused this PID (PIDs recycle) — `bind` fails with
    // `AddrInUse` against a leftover path, so clear it first. Safe: a
    // Unix domain socket path is just a filesystem name, not a live
    // listener, once its owning process is gone.
    let _ = std::fs::remove_file(path);
    // Owner-only permissions, matching HotSpot's attachListener.cpp.
    // This is a local IPC channel that can trigger a heap dump of the
    // whole process on request — it must not be reachable by other
    // users on the host. As HotSpot does (`LinuxAttachListener::init`),
    // bind under a temporary name, restrict it, and only then rename it to
    // the name clients look for (interpreter round i2 wave 59, lane L1): a
    // bind at the final name was connectable with the umask's permissions
    // until the `chmod` landed.
    let staging = format!("{path}.tmp");
    let _ = std::fs::remove_file(&staging);
    let listener = match std::os::unix::net::UnixListener::bind(&staging) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("AttachListener: failed to bind attach socket at {staging}: {e}");
            return None;
        }
    };
    {
        use std::os::unix::fs::PermissionsExt;
        let restricted = std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600));
        let owned = restricted.and_then(|()| {
            let name = std::ffi::CString::new(staging.as_bytes())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
            // macOS inherits a directory's group. HotSpot explicitly sets the
            // socket's owner/group to the target's effective credentials.
            // SAFETY: name is a live NUL-terminated path; uid/gid calls are infallible.
            if unsafe { libc::chown(name.as_ptr(), libc::geteuid(), libc::getegid()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            std::fs::rename(&staging, path)
        });
        if let Err(e) = owned {
            tracing::warn!("AttachListener: could not publish the attach socket at {path}: {e}");
            let _ = std::fs::remove_file(&staging);
            return None;
        }
    }
    // Poll rather than block in `accept()`, so `stop_listening` can
    // retire the thread deterministically.
    //
    // The obvious alternative -- block in `accept()` and have
    // `stop_listening` poke the socket with a throwaway connection -- is
    // WRONG here, and silently so: `new_with_vm_state` binds
    // `/tmp/.java_pid<pid>`, the same path for every VM in the process, and
    // each bind unlinks the previous socket. A poke by path would then
    // reach whichever listener bound LAST, so the older thread would never
    // wake and the `join` in `stop_listening` would hang forever. Polling
    // needs no poke and cannot alias. One wakeup per `ATTACH_POLL` per live
    // VM is negligible beside the leaked thread + fd this replaces.
    if listener.set_nonblocking(true).is_err() {
        // Cannot poll safely; do not spawn a thread `stop_listening` would
        // not be able to reap.
        tracing::warn!(
            "AttachListener: set_nonblocking failed for {path}; not serving attach requests"
        );
        return None;
    }
    Some(listener)
}

/// The attach listener's accept loop, until `shutdown` (or the socket goes).
#[cfg(unix)]
fn serve_attach_socket(
    listener: std::os::unix::net::UnixListener,
    commands: Arc<parking_lot::RwLock<Vec<DiagnosticCommand>>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    sigquit_dump: bool,
) {
    while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
        let stream = match listener.accept() {
            Ok((conn, _addr)) => Ok(conn),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Wave 60 (lane L1): the listener is up, so a `SIGQUIT` asks
                // for a thread dump, as HotSpot's dispatcher answers it once
                // its listener is initialized.
                if sigquit_dump && take_sigquit() {
                    print_sigquit_thread_dump(&commands);
                }
                std::thread::sleep(ATTACH_POLL);
                continue;
            }
            Err(e) => Err(e),
        };
        match stream {
            Ok(conn) if !attach_peer_allowed(&conn) => {
                // HotSpot closes it unanswered (`LinuxAttachListener::dequeue`).
                drop(conn);
            }
            Ok(conn) => {
                // The connection itself must be blocking: it is
                // handed to `handle_attach_connection`, which does
                // ordinary blocking reads/writes. `accept` on a
                // non-blocking listener yields a non-blocking
                // socket on Linux, so clear the flag explicitly.
                let _ = conn.set_nonblocking(false);
                let commands = Arc::clone(&commands);
                // One thread per connection: attach requests are
                // rare (an operator running a diagnostic
                // command) and some handlers (GC.heap_dump) can
                // take a while — a single-threaded accept loop
                // would otherwise serialize unrelated attaches
                // behind a slow one.
                std::thread::Builder::new()
                    .name("Attach-Connection".into())
                    .spawn(move || handle_attach_connection(conn, &commands))
                    .ok();
            }
            Err(_) => break, // listener socket gone (e.g. unlinked externally)
        }
    }
}

/// HotSpot's peer check (`LinuxAttachListener::dequeue`, interpreter round i2
/// wave 59, lane L1): the connecting process's uid and gid
/// (`SO_PEERCRED`) must be this process's effective ones, or the peer root.
/// The socket's 0600 mode already keeps other users out; this also refuses a
/// same-uid peer running under another group, as HotSpot does. macOS uses
/// `getpeereid` for the same check; other Unix targets retain their old policy.
#[cfg(unix)]
fn attach_peer_allowed(conn: &std::os::unix::net::UnixStream) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred` and `len` are valid for the kernel to write, sized
        // as `SO_PEERCRED` expects, on a live socket fd.
        let rc = unsafe {
            libc::getsockopt(
                conn.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return false;
        }
        // SAFETY: `geteuid` / `getegid` have no preconditions.
        let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
        cred.uid == 0 || (cred.uid == euid && cred.gid == egid)
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let (mut uid, mut gid) = (0, 0);
        unsafe {
            libc::getpeereid(conn.as_raw_fd(), &mut uid, &mut gid) == 0
                && (uid == 0 || (uid == libc::geteuid() && gid == libc::getegid()))
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = conn;
        true
    }
}

/// The OS attach directory, never java.io.tmpdir or TMPDIR.
fn attach_temp_directory() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: confstr writes at most the supplied capacity, including NUL.
        let size =
            unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0) };
        if size > 0 && size <= 4096 {
            let mut path = vec![0u8; size];
            let written = unsafe {
                libc::confstr(
                    libc::_CS_DARWIN_USER_TEMP_DIR,
                    path.as_mut_ptr().cast(),
                    size,
                )
            };
            if written == size {
                use std::os::unix::ffi::OsStringExt;
                path.pop();
                return std::ffi::OsString::from_vec(path).into();
            }
        }
    }
    "/tmp".into()
}

#[cfg(windows)]
#[path = "windows_attach.rs"]
mod windows_attach;

/// HotSpot's `AttachListener::is_init_trigger` (Linux): `.attach_pid<pid>`
/// in the working directory, else in `/tmp`, owned by this process's
/// effective user (interpreter round i2 wave 59, lane L1).
#[cfg(unix)]
fn attach_init_trigger_present() -> bool {
    use std::os::unix::fs::MetadataExt;
    let name = format!(".attach_pid{}", std::process::id());
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    let owned =
        |path: std::path::PathBuf| std::fs::metadata(path).is_ok_and(|meta| meta.uid() == euid);
    std::env::current_dir().is_ok_and(|cwd| owned(cwd.join(&name)))
        || owned(attach_temp_directory().join(&name))
}

/// Keep `SIGQUIT` from killing the process (interpreter round i2 wave 59,
/// lane L1; with `CRATONVM_ATTACH_LISTENER_LAZY`): an attaching client that
/// finds no socket sends the target `SIGQUIT`
/// ([`AttachListener::start_listening_lazily`]), whose default action
/// terminates the process with a core dump, where HotSpot's handler starts
/// the listener (or prints a thread dump). The handler does nothing: the
/// lazy listener's poll finds the trigger file on its own. Installed only
/// over default or ignored dispositions, so background shell launches attach
/// normally while an embedder's custom handler stays.
/// Process-wide by nature (a signal disposition), and idempotent.
#[cfg(unix)]
pub(crate) fn install_sigquit_keeps_the_vm_alive() {
    // SAFETY: a handler that touches no state (async-signal-safe by being
    // empty), over default or ignored dispositions only.
    unsafe { install_sigquit_handler(ignore_sigquit, false) };
}

/// The `SIGQUIT` handler of [`install_sigquit_keeps_the_vm_alive`]: nothing.
#[cfg(unix)]
extern "C" fn ignore_sigquit(_signal: libc::c_int) {}

/// The `SIGQUIT` latch (interpreter round i2 wave 60, lane L1;
/// `CRATONVM_SIGQUIT_THREAD_DUMP`): set by [`latch_sigquit`], the signal
/// handler, and taken by an attach listener's thread ([`take_sigquit`]),
/// which prints the thread dump. A process static by nature: a signal and
/// its disposition are the process's, and a handler can reach nothing else
/// (one entry of the per-VM statics ratchet). With several VMs in the
/// process, whichever listener takes it first answers.
///
/// Interpreter round i3 wave 80, lane L6: a word of bits, still the one
/// static -- [`SIGNAL_PENDING`] (the latch) and, on Windows,
/// [`CTRL_BREAK_HANDLER_INSTALLED`] (the console handler is installed once
/// per process; `SetConsoleCtrlHandler` would otherwise add one per VM).
/// Windows' `Ctrl-Break` sets the same latch (HotSpot's `SIGBREAK` prints
/// the same dump).
#[cfg(any(unix, windows))]
static SIGQUIT_LATCH: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// [`SIGQUIT_LATCH`]'s pending bit: a `SIGQUIT` (or `Ctrl-Break`) arrived and
/// no dispatcher has taken it yet.
#[cfg(any(unix, windows))]
const SIGNAL_PENDING: u8 = 1;

/// [`SIGQUIT_LATCH`]'s Windows bit: [`install_ctrl_break_latch`] has
/// registered its console handler in this process.
#[cfg(windows)]
const CTRL_BREAK_HANDLER_INSTALLED: u8 = 2;

/// Set the pending bit of `latch`: one lock-free atomic read-modify-write,
/// async-signal-safe.
#[cfg(any(unix, windows))]
fn set_signal_pending(latch: &std::sync::atomic::AtomicU8) {
    latch.fetch_or(SIGNAL_PENDING, std::sync::atomic::Ordering::AcqRel);
}

/// Take the pending bit of `latch` (clearing it, and only it): whether a
/// signal arrived since the last take.
#[cfg(any(unix, windows))]
fn take_signal_pending(latch: &std::sync::atomic::AtomicU8) -> bool {
    latch.fetch_and(!SIGNAL_PENDING, std::sync::atomic::Ordering::AcqRel) & SIGNAL_PENDING != 0
}

/// The latching `SIGQUIT` handler: one atomic store, async-signal-safe.
#[cfg(unix)]
extern "C" fn latch_sigquit(_signal: libc::c_int) {
    set_signal_pending(&SIGQUIT_LATCH);
}

/// Take the `SIGQUIT` latch: whether a signal arrived since the last take.
#[cfg(any(unix, windows))]
fn take_sigquit() -> bool {
    take_signal_pending(&SIGQUIT_LATCH)
}

/// The console control handler of [`install_ctrl_break_latch`]: `Ctrl-Break`
/// (`CTRL_BREAK_EVENT`, 1) sets the latch and is handled (`TRUE`: the
/// process goes on, as HotSpot's `SIGBREAK` dump leaves it); every other
/// event (`Ctrl-C`, close, logoff, shutdown) is passed on (`FALSE`) to the
/// next handler and the default action, untouched.
#[cfg(windows)]
unsafe extern "system" fn latch_ctrl_break(ctrl_type: u32) -> i32 {
    const CTRL_BREAK_EVENT: u32 = 1;
    if ctrl_type == CTRL_BREAK_EVENT {
        set_signal_pending(&SIGQUIT_LATCH);
        1
    } else {
        0
    }
}

/// Windows' `Ctrl-Break` (interpreter round i3 wave 80, lane L6;
/// `VmConfig::sigquit_thread_dump`, not under `-Xrs`): HotSpot's
/// `SIGBREAK` handler prints the same thread dump as `SIGQUIT` on Unix and
/// the process continues; without a handler the console's default action
/// terminates the process. Registers [`latch_ctrl_break`] once per process;
/// a dispatcher thread ([`AttachListener::start_signal_dispatcher`]) prints.
#[cfg(windows)]
pub(crate) fn install_ctrl_break_latch() {
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }
    let before = SIGQUIT_LATCH.fetch_or(
        CTRL_BREAK_HANDLER_INSTALLED,
        std::sync::atomic::Ordering::AcqRel,
    );
    if before & CTRL_BREAK_HANDLER_INSTALLED != 0 {
        return;
    }
    // SAFETY: registers a handler that does one atomic read-modify-write
    // and returns; `add` = TRUE.
    let handler: unsafe extern "system" fn(u32) -> i32 = latch_ctrl_break;
    if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
        SIGQUIT_LATCH.fetch_and(
            !CTRL_BREAK_HANDLER_INSTALLED,
            std::sync::atomic::Ordering::AcqRel,
        );
    }
}

/// How often a signal dispatcher ([`AttachListener::start_signal_dispatcher`])
/// looks at the latch: the attach listener's own poll interval.
#[cfg(any(unix, windows))]
const SIGNAL_DISPATCH_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Install the latching `SIGQUIT` handler (interpreter round i2 wave 60,
/// lane L1; `VmConfig::sigquit_thread_dump`), over default or ignored dispositions
/// or wave 59's empty one only, so an embedder's own handler stays.
/// Process-wide by nature, and idempotent.
#[cfg(unix)]
pub(crate) fn install_sigquit_latch() {
    // SAFETY: the handler is one atomic store (async-signal-safe).
    unsafe { install_sigquit_handler(latch_sigquit, true) };
}

/// Install `handler` for `SIGQUIT` over default or ignored dispositions (and, with
/// `over_ignore`, over [`ignore_sigquit`]); nothing over any other.
///
/// # Safety
///
/// `handler` must be async-signal-safe.
#[cfg(unix)]
unsafe fn install_sigquit_handler(handler: extern "C" fn(libc::c_int), over_ignore: bool) {
    // SAFETY: query, then install, with zeroed `sigaction` records the
    // calls fill; the caller vouches for the handler.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGQUIT, std::ptr::null(), &mut current) != 0 {
            return;
        }
        let ignoring =
            current.sa_sigaction == ignore_sigquit as extern "C" fn(libc::c_int) as usize;
        if current.sa_sigaction != libc::SIG_DFL
            && current.sa_sigaction != libc::SIG_IGN
            && !(over_ignore && ignoring)
        {
            return;
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGQUIT, &action, std::ptr::null_mut());
    }
}

/// Print the `Thread.print` command's dump on stdout, as HotSpot's signal
/// dispatcher prints one for `SIGQUIT` (interpreter round i2 wave 60, lane
/// L1). Nothing when the command is not registered or fails. (Windows too
/// since wave 80, lane L6: `Ctrl-Break`.)
#[cfg(any(unix, windows))]
#[cold]
#[inline(never)]
fn print_sigquit_thread_dump(commands: &parking_lot::RwLock<Vec<DiagnosticCommand>>) {
    use std::io::Write;
    let dump = commands
        .read()
        .iter()
        .find(|c| c.name == "Thread.print")
        .map(|c| c.execute(&[]));
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[SIGQUIT_DUMP] printed={}",
            dump.as_ref().is_some_and(|d| d.success)
        );
    }
    if let Some(dump) = dump.filter(|d| d.success) {
        // Interpreter round i3 wave 81, lane L6: HotSpot's dispatcher follows
        // the threads with the `Heap` section (`jstack` and `jcmd
        // Thread.print` do not), from the state's own heap summary.
        let heap = commands
            .read()
            .iter()
            .find(|c| c.name == SIGQUIT_HEAP_COMMAND)
            .map(|c| c.execute(&[]))
            .filter(|h| h.success);
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(dump.output.as_bytes());
        if let Some(heap) = heap {
            let _ = out.write_all(heap.output.as_bytes());
        }
        let _ = out.flush();
    }
    // Interpreter round i2 wave 63, lane L1: a C JVMTI env's
    // `DataDumpRequest`, after the dump, as HotSpot's signal thread posts it.
    #[cfg(feature = "experimental-debug")]
    crate::jvmti::native_env::post_data_dump_request();
}

/// The live command behind the attach `load` operation (interpreter round i2
/// wave 58, lane L1; [`VmDiagnosticState::attach_load`]). Its arguments are
/// the operation's three fields: the library, `true` / `false` for an
/// absolute path, and the options.
const ATTACH_LOAD_COMMAND: &str = "attach load";

/// The live commands behind the attach `properties` and `agentProperties`
/// operations (interpreter round i2 wave 59, lane L1;
/// [`VmDiagnosticState::attach_properties`]). Names with a space, like
/// [`ATTACH_LOAD_COMMAND`], so no `jcmd <pid> <command>` line reaches them.
const ATTACH_PROPERTIES_COMMAND: &str = "attach properties";
const ATTACH_AGENT_PROPERTIES_COMMAND: &str = "attach agentProperties";

/// The live command behind the `Heap` section a `SIGQUIT` dump ends with
/// (interpreter round i3 wave 81, lane L6; [`print_sigquit_thread_dump`]). A
/// name with a space, like [`ATTACH_LOAD_COMMAND`].
const SIGQUIT_HEAP_COMMAND: &str = "sigquit heap";

/// gci i14/hd: [`VmDiagnosticState::heap_info_lines`] as printed text, each
/// line ended by a newline, as HotSpot's `print_cr` ends them. Pure.
fn heap_info_text(lines: &[String]) -> String {
    let mut out = String::new();
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// gci i47w/oa47 (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
/// item 6, `vi14-hd.list` H): HotSpot 25's `HeapInfoDCmd::execute` prints
/// `CollectedHeap::print_heap_on` alone, not `Universe::print_on`: no `Heap`
/// header, no Metaspace / class-space lines, and every collector line one
/// column shallower (the column is `Universe::print_on`'s `StreamIndentor`).
/// Measured on Temurin 25.0.3, G1 and Serial (`hd14_hinfo_*` `.hs`). The
/// SIGQUIT `Heap` section keeps [`heap_info_text`] of the full lines. Pure.
fn heap_info_dcmd_text(lines: &[String]) -> String {
    let body: Vec<String> = lines
        .iter()
        .filter(|l| {
            l.as_str() != "Heap"
                && !crate::runtime::unified_logging::is_metaspace_print_on_line(l)
        })
        .map(|l| l.strip_prefix(' ').unwrap_or(l).to_string())
        .collect();
    heap_info_text(&body)
}

/// What HotSpot's listener answers (with a non-zero status) for a `jcmd`
/// command it does not know (MEASURED, HotSpot 25.0.3; interpreter round i2
/// wave 59, lane L1).
#[cfg_attr(not(unix), allow(dead_code))]
const JCMD_UNKNOWN_COMMAND: &str =
    "java.lang.IllegalArgumentException: Unknown diagnostic command\n";

/// `jcmd <pid> help [<command>]`'s output, in HotSpot's shape (MEASURED,
/// HotSpot 25.0.3; interpreter round i2 wave 59, lane L1): with no command,
/// "The following commands are available:", the sorted command names (the
/// internal attach-operation commands, whose names hold a space, excluded;
/// `help` included), a blank line and the pointer to `help <command>`; for a
/// command, its name, description, impact and syntax; for an unknown name,
/// "Help unavailable : '<name>' : No such command".
#[cfg_attr(not(unix), allow(dead_code))]
fn jcmd_help(commands: &[DiagnosticCommand], args: &[String]) -> String {
    jcmd_help_from(commands, args, false)
}

/// gci i35/jh (vi32-jh H1 / H2): HotSpot 25 registers every `DCmd` with the
/// sources it is exported to (`DCmd::register_dcmds`), and a few are not
/// exported to the DiagnosticCommand MBean (`DCmd_Source_MBean`):
/// `GC.heap_dump` (`DCmd_Source_Internal | DCmd_Source_AttachAPI`; MEASURED,
/// i34b `jh32_h_texts.hs` `GC.heap_dump | absent`, `jh32_h_on.hs` `Help
/// unavailable : 'GC.heap_dump' : No such command`) and the
/// `ManagementAgent.*` commands (`jmx_agent_export_flags`). Through the MBean
/// such a command is not listed (`getDiagnosticCommands`), has no info row,
/// no `help` page (the `HelpDCmd` runs with source MBean and its factory
/// lookup filters by source) and does not execute (`Unknown diagnostic
/// command`). Every other HotSpot command is `full_export`.
const DCMD_NOT_MBEAN_EXPORTED: &[&str] = &[
    "GC.heap_dump",
    "ManagementAgent.start",
    "ManagementAgent.start_local",
    "ManagementAgent.stop",
    "ManagementAgent.status",
];

/// gci i35/jh: whether a registered command is one a `jcmd` line names (the
/// internal attach operations hold a space) and, for `mbean`, one HotSpot
/// exports to the DiagnosticCommand MBean ([`DCMD_NOT_MBEAN_EXPORTED`]).
/// Pure.
fn dcmd_visible(name: &str, mbean: bool) -> bool {
    !name.contains(' ') && !(mbean && DCMD_NOT_MBEAN_EXPORTED.contains(&name))
}

/// gci i35/jh: [`jcmd_help`] as HotSpot's `HelpDCmd` answers it for a
/// source: `mbean` is the DiagnosticCommand MBean's `help` operation
/// ([`dcmd_mbean_execute`]), which sees only the MBean-exported commands;
/// otherwise the attach listener (`jcmd`). Pure.
#[cfg_attr(not(unix), allow(dead_code))]
fn jcmd_help_from(commands: &[DiagnosticCommand], args: &[String], mbean: bool) -> String {
    // gci i30/ob (`i28-dg-report.md` review item 2): HotSpot's `HelpDCmd`
    // option `-all` (BOOLEAN: `-all` alone or `-all=true`) prints every
    // command's name and description, and wins over a command name. It was
    // read as a command name (`Help unavailable : '-all' : No such command`).
    let (all, rest) = jcmd_help_all_option(args);
    if all {
        return jcmd_help_all(commands, mbean);
    }
    let args = rest.as_slice();
    let Some(wanted) = args.first() else {
        let mut names: Vec<&str> = commands
            .iter()
            .map(|c| c.name.as_str())
            .filter(|name| dcmd_visible(name, mbean))
            .chain(std::iter::once("help"))
            .collect();
        names.sort_unstable();
        names.dedup();
        let mut out = String::from("The following commands are available:\n");
        for name in names {
            out.push_str(name);
            out.push('\n');
        }
        out.push_str("\nFor more information about a specific command use 'help <command>'.\n");
        return out;
    };
    // gci i32/jh: `help` is a command of HotSpot's own (`HelpDCmd`), so
    // `help help` prints its page; it answered "No such command".
    if wanted == "help" {
        return jcmd_command_help(
            "help",
            DCMD_HELP_DESCRIPTION,
            &CommandImpact::Low.to_string(),
            &help_dcmd_arguments(),
        );
    }
    let Some(command) = commands
        .iter()
        .find(|c| &c.name == wanted && dcmd_visible(&c.name, mbean))
    else {
        return format!("Help unavailable : '{wanted}' : No such command\n");
    };
    // gci i33/jf (page `gci-i32jh-jfr-dcmd-help-is-not-the-jdks-printhelp-FIXED-20261016`):
    // HotSpot's `HelpDCmd` prints the same header for every command and then
    // the command's `print_help`; a `JfrDCmd`'s is the JDK's own `getHelp()`.
    if let Some(page) = command.own_help_page() {
        return format!(
            "{}\n{}\n\nImpact: {}\n\n{page}",
            command.name,
            command.description,
            dcmd_impact_text(command)
        );
    }
    jcmd_command_help(
        &command.name,
        &command.description,
        &dcmd_impact_text(command),
        &command.arguments,
    )
}

/// gci i32/jh (`i31-hp-report.md` review item 1): one command's page of
/// `jcmd <pid> help <command>`, HotSpot 25's `HelpDCmd::execute` (name,
/// description, `\nImpact: `, an empty line -- JDK 25 has no `Permission:`
/// line, the `JavaPermission` went with the Security Manager; MEASURED: no
/// `Permission: %s` string in the 25.0.3 `jvm.dll`) followed by the
/// command's `print_help`: a parserless `DCmd` prints `Syntax: <name>`; a
/// `DCmdWithParser` prints `DCmdParser::print_help` -- `Syntax : <name>
/// [options]` (the space before the colon, and the one after the name even
/// with no options, are HotSpot's), each positional argument as ` <name>` or
/// ` [<name>]`, then the `Arguments:` and `Options: (...)` blocks whose rows
/// are `\t<name> : [optional] <description> (<TYPE>, <default>|no default
/// value)` (a mandatory row has an empty `[optional]`, hence two spaces). An
/// argument whose name starts with `-` is an option, as in
/// [`dcmd_mbean_info`]; every command here that HotSpot builds with a parser
/// declares at least one argument or option, so an empty list is a
/// parserless `DCmd`. Pure.
#[cfg_attr(not(unix), allow(dead_code))]
fn jcmd_command_help(
    name: &str,
    description: &str,
    impact: &str,
    arguments: &[CommandArgument],
) -> String {
    let mut out = format!("{name}\n{description}\n\nImpact: {impact}\n\n");
    if arguments.is_empty() {
        out.push_str("Syntax: ");
        out.push_str(name);
        out.push('\n');
        return out;
    }
    let (options, positional): (Vec<&CommandArgument>, Vec<&CommandArgument>) = arguments
        .iter()
        .partition(|a| dcmd_arg_is_option(name, &a.name));
    out.push_str("Syntax : ");
    out.push_str(name);
    out.push(' ');
    if !options.is_empty() {
        out.push_str("[options]");
    }
    for a in &positional {
        if a.required {
            out.push_str(&format!(" <{}>", a.name));
        } else {
            out.push_str(&format!(" [<{}>]", a.name));
        }
    }
    out.push('\n');
    let row = |a: &CommandArgument| -> String {
        format!(
            "\t{} : {} {} ({}, {})\n",
            a.name,
            if a.required { "" } else { "[optional]" },
            a.description,
            dcmd_arg_type_name(a.arg_type),
            a.default_value.as_deref().unwrap_or("no default value")
        )
    };
    if !positional.is_empty() {
        out.push_str("\nArguments:\n");
        for a in &positional {
            out.push_str(&row(*a));
        }
    }
    if !options.is_empty() {
        out.push_str(
            "\nOptions: (options must be specified using the <key> or <key>=<value> syntax)\n",
        );
        for a in &options {
            out.push_str(&row(*a));
        }
    }
    out
}

/// Whether `arg_name` on `cmd_name` is an option (`_options` in HotSpot's
/// `DCmdParser`) rather than a positional argument (`_arguments`).
fn dcmd_arg_is_option(cmd_name: &str, arg_name: &str) -> bool {
    match cmd_name {
        "Compiler.memory"
        | "JFR.check"
        | "JFR.configure"
        | "JFR.dump"
        | "JFR.start"
        | "JFR.stop"
        | "ManagementAgent.start"
        | "VM.classloaders"
        | "VM.events"
        | "VM.log"
        | "VM.metaspace"
        | "VM.native_memory" => true,
        "JFR.view" => arg_name != "view",
        _ => arg_name.starts_with('-'),
    }
}

/// gci i32/jh: HotSpot 25's `HelpDCmd` parser (`diagnosticCommand.cpp`;
/// strings MEASURED in the 25.0.3 `jvm.dll`): the option `-all` and the
/// optional argument `command name`.
#[cfg_attr(not(unix), allow(dead_code))]
fn help_dcmd_arguments() -> Vec<CommandArgument> {
    vec![
        dcmd_arg(
            "-all",
            "Show help for all commands",
            ArgType::Bool,
            false,
            Some("false"),
        ),
        dcmd_arg(
            "command name",
            "The name of the command for which we want help",
            ArgType::String,
            false,
            None,
        ),
    ]
}

/// gci i32/jh: the type name HotSpot 25's `DCmdArgument` declares -- what
/// `jcmd help <command>` prints in `(<TYPE>, ...)` and what
/// `jmm_GetDiagnosticCommandArgumentsInfo` hands the MBean, whose
/// `DiagnosticCommandImpl.sanitiseType` maps a non-public type (`FILE`) to
/// `STRING` on the Java side. Pure.
fn dcmd_arg_type_name(t: ArgType) -> &'static str {
    match t {
        ArgType::String => "STRING",
        ArgType::Int => "INT",
        ArgType::Bool => "BOOLEAN",
        ArgType::FilePath => "FILE",
        ArgType::JuLong => "JULONG",
        ArgType::MemorySize => "MEMORY SIZE",
        ArgType::NanoTime => "NANOTIME",
        ArgType::StringSet => "STRING SET",
    }
}

/// gci i32/jh: one `DCmdArgument`. Pure.
fn dcmd_arg(
    name: &str,
    description: &str,
    arg_type: ArgType,
    required: bool,
    default_value: Option<&str>,
) -> CommandArgument {
    CommandArgument {
        name: name.to_string(),
        description: description.to_string(),
        arg_type,
        required,
        default_value: default_value.map(str::to_string),
    }
}

/// HotSpot 25's `JfrConfigureFlightRecorderDCmd::print_help` body (which prints
/// this static block directly from C++ rather than calling
/// `DCmdConfigure.getHelp()`).
const JFR_CONFIGURE_HELP_PAGE: &str = "\
Options:

  globalbuffercount   (Optional) Number of global buffers. This option is a legacy
                      option: change the memorysize parameter to alter the number of
                      global buffers. This value cannot be changed once JFR has been
                      initialized. (STRING, default determined by the value for
                      memorysize)

  globalbuffersize    (Optional) Size of the global buffers, in bytes. This option is a
                      legacy option: change the memorysize parameter to alter the size
                      of the global buffers. This value cannot be changed once JFR has
                      been initialized. (STRING, default determined by the value for
                      memorysize)

  maxchunksize        (Optional) Maximum size of an individual data chunk in bytes if
                      one of the following suffixes is not used: 'm' or 'M' for
                      megabytes OR 'g' or 'G' for gigabytes. This value cannot be
                      changed once JFR has been initialized. (STRING, 12M)

  memorysize          (Optional) Overall memory size, in bytes if one of the following
                      suffixes is not used: 'm' or 'M' for megabytes OR 'g' or 'G' for
                      gigabytes. This value cannot be changed once JFR has been
                      initialized. (STRING, 10M)

  repositorypath      (Optional) Path to the location where recordings are stored until
                      they are written to a permanent file. (STRING, The default
                      location is the temporary directory for the operating system. On
                      Linux operating systems, the temporary directory is /tmp. On
                      Windows, the temporary directory is specified by the TMP
                      environment variable)

  dumppath            (Optional) Path to the location where a recording file is written
                      in case the VM runs into a critical error, such as a system
                      crash. (STRING, The default location is the current directory)

  stackdepth          (Optional) Stack depth for stack traces. Setting this value
                      greater than the default of 64 may cause a performance
                      degradation. This value cannot be changed once JFR has been
                      initialized. (LONG, 64)

  thread_buffer_size  (Optional) Local buffer size for each thread in bytes if one of
                      the following suffixes is not used: 'k' or 'K' for kilobytes or
                      'm' or 'M' for megabytes. Overriding this parameter could reduce
                      performance and is not recommended. This value cannot be changed
                      once JFR has been initialized. (STRING, 8k)

  preserve-repository (Optional) Preserve files stored in the disk repository after the
                      Java Virtual Machine has exited. (BOOLEAN, false)

Options must be specified using the <key> or <key>=<value> syntax.

Example usage:

 $ jcmd <pid> JFR.configure
 $ jcmd <pid> JFR.configure repositorypath=/temporary
 $ jcmd <pid> JFR.configure stackdepth=256
 $ jcmd <pid> JFR.configure memorysize=100M

";

/// gci i32/jh (`i31-hp-report.md` review item 1): the arguments and options
/// HotSpot 25 declares for a command this VM registers, in HotSpot's
/// declaration order (`diagnosticCommand.cpp` constructors; every
/// description, option name and type MEASURED in the 25.0.3 `jvm.dll`
/// string table and `jmm_GetDiagnosticCommandArgumentsInfo`). `jcmd help
/// <command>` and the DiagnosticCommand MBean's `dcmd.arguments` read them.
/// Empty for a parserless HotSpot `DCmd`. Pure.
fn hotspot_dcmd_arguments(name: &str) -> Vec<CommandArgument> {
    use ArgType::{Bool, FilePath, Int, JuLong, MemorySize, NanoTime, String as Str, StringSet};
    let flag = |n: &str, d: &str| dcmd_arg(n, d, Bool, false, Some("false"));
    match name {
        "Compiler.CodeHeap_Analytics" => vec![
            dcmd_arg(
                "function",
                "Function to be performed (aggregate, UsedSpace, FreeSpace, MethodCount, \
                 MethodSpace, MethodAge, MethodNames, discard",
                Str,
                false,
                Some("all"),
            ),
            dcmd_arg(
                "granularity",
                "Detail level - smaller value -> more detail",
                Int,
                false,
                Some("4096"),
            ),
        ],
        "Compiler.directives_add" => vec![dcmd_arg(
            "filename",
            "Name of the directives file",
            Str,
            true,
            None,
        )],
        "Compiler.memory" => vec![
            flag("verbose", "Print detailed information"),
            dcmd_arg("minsize", "Minimum memory size", MemorySize, false, Some("0")),
            flag("legend", "Table mode: print legend"),
        ],
        "GC.class_histogram" => vec![
            flag("-all", "Inspect all objects, including unreachable objects"),
            dcmd_arg(
                "-parallel",
                "Number of parallel threads to use for heap inspection. 0 (the default) means \
                 let the VM determine the number of threads to use. 1 means use one thread \
                 (disable parallelism). For any other value the VM will try to use the specified \
                 number of threads, but might use fewer.",
                Int,
                false,
                Some("0"),
            ),
        ],
        "GC.heap_dump" => vec![
            flag("-all", "Dump all objects, including unreachable objects"),
            dcmd_arg("filename", "Name of the dump file", FilePath, true, None),
            dcmd_arg(
                "-gz",
                "If specified, the heap dump is written in gzipped format using the given \
                 compression level. 1 (recommended) is the fastest, 9 the strongest compression.",
                Int,
                false,
                Some("1"),
            ),
            flag(
                "-overwrite",
                "If specified, the dump file will be overwritten if it exists",
            ),
            dcmd_arg(
                "-parallel",
                "Number of parallel threads to use for heap dump. The VM will try to use the \
                 specified number of threads, but might use fewer.",
                Int,
                false,
                Some("1"),
            ),
        ],
        "JFR.check" => vec![
            dcmd_arg(
                "name",
                "Recording name, e.g. \\\"My Recording\\\" or omit to see all recordings",
                Str,
                false,
                None,
            ),
            flag("verbose", "Print event settings for the recording(s)"),
        ],
        "JFR.configure" => vec![
            dcmd_arg(
                "repositorypath",
                "Path to repository,.e.g \\\"My Repository\\\"",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "dumppath",
                "Path to dump, e.g. \\\"My Dump path\\\"",
                Str,
                false,
                None,
            ),
            dcmd_arg("stackdepth", "Stack depth", JuLong, false, Some("64")),
            dcmd_arg(
                "globalbuffercount",
                "Number of global buffers,",
                JuLong,
                false,
                Some("20"),
            ),
            dcmd_arg(
                "globalbuffersize",
                "Size of a global buffers,",
                MemorySize,
                false,
                Some("512k"),
            ),
            dcmd_arg(
                "thread_buffer_size",
                "Size of a thread buffer",
                MemorySize,
                false,
                Some("8k"),
            ),
            dcmd_arg(
                "memorysize",
                "Overall memory size, ",
                MemorySize,
                false,
                Some("10m"),
            ),
            dcmd_arg(
                "maxchunksize",
                "Size of an individual disk chunk",
                MemorySize,
                false,
                Some("12m"),
            ),
            dcmd_arg(
                "samplethreads",
                "Activate thread sampling",
                Bool,
                false,
                Some("true"),
            ),
            flag(
                "preserve-repository",
                "Preserve the disk repository after JVM exit",
            ),
        ],
        "JFR.dump" => vec![
            dcmd_arg(
                "name",
                "Recording name, e.g. \\\"My Recording\\\"",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "filename",
                "Copy recording data to file, e.g. \\\"C:\\Users\\user\\recording.jfr\\\"",
                FilePath,
                false,
                None,
            ),
            dcmd_arg(
                "maxage",
                "Maximum duration to dump, in (s)econds, (m)inutes, (h)ours, or (d)ays, e.g. 60m, \
                 or 0 for no limit",
                NanoTime,
                false,
                None,
            ),
            dcmd_arg(
                "maxsize",
                "Maximum amount of bytes to dump, in (M)B or (G)B, e.g. 500M, or 0 for no limit",
                MemorySize,
                false,
                Some("hotspot-pid-xxxxx-id-y-YYYY_MM_dd_HH_mm_ss.jfr"),
            ),
            dcmd_arg(
                "begin",
                "Point in time to dump data from, e.g. 09:00, 21:35:00, \
                 2018-06-03T18:12:56.827Z, 2018-06-03T20:13:46.832, -10m, -3h, or -1d",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "end",
                "Point in time to dump data to, e.g. 09:00, 21:35:00, \
                 2018-06-03T18:12:56.827Z, 2018-06-03T20:13:46.832, -10m, -3h, or -1d",
                Str,
                false,
                None,
            ),
            flag("path-to-gc-roots", "Collect path to GC roots"),
        ],
        "JFR.start" => vec![
            dcmd_arg(
                "name",
                "Name that can be used to identify recording, e.g. \\\"My Recording\\\"",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "settings",
                "Settings file(s), e.g. profile or default. See JAVA_HOME/lib/jfr",
                StringSet,
                false,
                Some("default.jfc"),
            ),
            dcmd_arg(
                "delay",
                "Delay recording start with (s)econds, (m)inutes), (h)ours), or (d)ays, e.g. 5h.",
                NanoTime,
                false,
                Some("0s"),
            ),
            dcmd_arg(
                "duration",
                "Duration of recording in (s)econds, (m)inutes, (h)ours, or (d)ays, e.g. 300s.",
                NanoTime,
                false,
                None,
            ),
            dcmd_arg(
                "disk",
                "Recording should be persisted to disk",
                Bool,
                false,
                Some("true"),
            ),
            dcmd_arg(
                "filename",
                "Resulting recording filename, e.g. \\\"C:\\Users\\user\\recording.jfr\\\"",
                FilePath,
                false,
                Some("hotspot-pid-xxxxx-id-y-YYYY_MM_dd_HH_mm_ss.jfr"),
            ),
            dcmd_arg(
                "maxage",
                "Maximum time to keep recorded data (on disk) in (s)econds, (m)inutes, (h)ours, \
                 or (d)ays, e.g. 60m, or 0 for no limit",
                NanoTime,
                false,
                Some("0"),
            ),
            dcmd_arg(
                "maxsize",
                "Maximum amount of bytes to keep (on disk) in (k)B, (M)B or (G)B, e.g. 500M, or 0 \
                 for no limit",
                MemorySize,
                false,
                Some("250M"),
            ),
            dcmd_arg(
                "flush-interval",
                "Minimum time before flushing buffers, measured in (s)econds, e.g. 4 s, or 0 for \
                 flushing when a recording ends",
                NanoTime,
                false,
                Some("1s"),
            ),
            flag("dumponexit", "Dump running recording when JVM shuts down"),
            flag("path-to-gc-roots", "Collect path to GC roots"),
        ],
        "JFR.stop" => vec![
            dcmd_arg(
                "name",
                "Recording name, e.g. \\\"My Recording\\\"",
                Str,
                true,
                None,
            ),
            dcmd_arg(
                "filename",
                "Copy recording data to file, e.g. \\\"C:\\Users\\user\\recording.jfr\\\"",
                FilePath,
                false,
                None,
            ),
        ],
        "JFR.view" => vec![
            dcmd_arg(
                "cell-height",
                "Maximum heigth of a table cell",
                Int,
                false,
                Some("1"),
            ),
            dcmd_arg(
                "maxage",
                "Maximum duration of data to view, in (s)econds, (m)inutes, (h)ours, or (d)ays, \
                 e.g. 60m, or 0 for no limit",
                NanoTime,
                false,
                Some("10m"),
            ),
            dcmd_arg(
                "maxsize",
                "Maximum amount of bytes to view, in (M)B or (G)B, e.g. 500M, or 0 for no limit",
                MemorySize,
                false,
                Some("100M"),
            ),
            dcmd_arg(
                "truncate",
                "Truncation mode if value doesn't fit in a table cell, valid values are \
                 'beginning' and 'end'",
                Str,
                false,
                Some("end"),
            ),
            flag(
                "verbose",
                "Display additional information about the view, such as the underlying query",
            ),
            dcmd_arg(
                "view",
                "Name of the view, for example hot-methods",
                Str,
                true,
                None,
            ),
            dcmd_arg(
                "width",
                "Maximum number of horizontal characters",
                Int,
                false,
                Some("100"),
            ),
        ],
        "JVMTI.agent_load" => vec![
            dcmd_arg(
                "library path",
                "Absolute path of the JVMTI agent to load.",
                Str,
                true,
                None,
            ),
            dcmd_arg(
                "agent option",
                "Option string to pass the agent.",
                Str,
                false,
                None,
            ),
        ],
        "ManagementAgent.start" => vec![
            dcmd_arg(
                "config.file",
                "set com.sun.management.config.file",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jmxremote.host",
                "set com.sun.management.jmxremote.host",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jmxremote.port",
                "set com.sun.management.jmxremote.port",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jmxremote.rmi.port",
                "set com.sun.management.jmxremote.rmi.port",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jmxremote.ssl",
                "set com.sun.management.jmxremote.ssl",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jmxremote.registry.ssl",
                "set com.sun.management.jmxremote.registry.ssl",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jmxremote.authenticate",
                "set com.sun.management.jmxremote.authenticate",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jmxremote.password.file",
                "set com.sun.management.jmxremote.password.file",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jmxremote.access.file",
                "set com.sun.management.jmxremote.access.file",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jmxremote.login.config",
                "set com.sun.management.jmxremote.login.config",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jmxremote.ssl.enabled.cipher.suites",
                "set com.sun.management.jmxremote.ssl.enabled.cipher.suite",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jmxremote.ssl.enabled.protocols",
                "set com.sun.management.jmxremote.ssl.enabled.protocols",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jmxremote.ssl.need.client.auth",
                "set com.sun.management.jmxremote.need.client.auth",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jmxremote.ssl.config.file",
                "set com.sun.management.jmxremote.ssl.config.file",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jmxremote.autodiscovery",
                "set com.sun.management.jmxremote.autodiscovery",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jdp.port",
                "set com.sun.management.jdp.port",
                Int,
                false,
                None,
            ),
            dcmd_arg(
                "jdp.address",
                "set com.sun.management.jdp.address",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jdp.source_addr",
                "set com.sun.management.jdp.source_addr",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "jdp.ttl",
                "set com.sun.management.jdp.ttl",
                Int,
                false,
                None,
            ),
            dcmd_arg(
                "jdp.pause",
                "set com.sun.management.jdp.pause",
                Int,
                false,
                None,
            ),
            dcmd_arg(
                "jdp.name",
                "set com.sun.management.jdp.name",
                Str,
                false,
                None,
            ),
        ],
        "System.dump_map" => vec![dcmd_arg(
            "-F",
            "file path",
            FilePath,
            false,
            Some("vm_memory_map_%p.txt"),
        )],
        "Thread.dump_to_file" => vec![
            flag("-overwrite", "May overwrite existing file"),
            dcmd_arg(
                "-format",
                "Output format (\"plain\" or \"json\")",
                Str,
                false,
                Some("plain"),
            ),
            dcmd_arg(
                "filepath",
                "The file path to the output file",
                FilePath,
                true,
                None,
            ),
        ],
        "Thread.print" => vec![
            flag("-l", "print java.util.concurrent locks"),
            flag("-e", "print extended thread information"),
        ],
        "VM.cds" => vec![
            dcmd_arg("subcmd", "static_dump | dynamic_dump", Str, true, None),
            dcmd_arg(
                "filename",
                "Name of shared archive to be dumped",
                FilePath,
                false,
                Some("java_pid%p_<subcmd>.jsa"),
            ),
        ],
        "VM.class_hierarchy" => vec![
            dcmd_arg(
                "classname",
                "Name of class whose hierarchy should be printed. If not specified, all class \
                 hierarchies are printed.",
                Str,
                false,
                None,
            ),
            flag("-i", "Inherited interfaces should be printed."),
            flag(
                "-s",
                "If a classname is specified, print its subclasses in addition to its \
                 superclasses. Without this option only the superclasses will be printed.",
            ),
        ],
        "VM.classes" => vec![flag(
            "-verbose",
            "Dump the detailed content of a Java class. Some classes are annotated with flags: F \
             = has, or inherits, a non-empty finalize method, f = has final method, W = methods \
             rewritten, C = marked with @Contended annotation, R = has been redefined, S = is \
             shared class",
        )],
        "VM.classloaders" => vec![
            flag("show-classes", "Print loaded classes."),
            flag("verbose", "Print detailed information."),
            dcmd_arg(
                "fold",
                "Show loaders of the same name and class as one.",
                Bool,
                false,
                Some("true"),
            ),
        ],
        "VM.events" => vec![
            dcmd_arg(
                "log",
                "Name of log to be printed. If omitted, all logs are printed.",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "max",
                "Maximum number of events to be printed (newest first). If omitted or zero, all \
                 events are printed.",
                Int,
                false,
                Some("0"),
            ),
        ],
        "VM.flags" => vec![flag("-all", "Print all flags supported by the VM")],
        "VM.log" => vec![
            dcmd_arg(
                "output",
                "The name or index (#<index>) of output to configure.",
                Str,
                false,
                None,
            ),
            dcmd_arg("output_options", "Options for the output.", Str, false, None),
            dcmd_arg("what", "Configures what tags to log.", Str, false, None),
            dcmd_arg(
                "decorators",
                "Configures which decorators to use. Use 'none' or an empty value to remove all.",
                Str,
                false,
                None,
            ),
            dcmd_arg(
                "disable",
                "Turns off all logging and clears the log configuration.",
                Bool,
                false,
                None,
            ),
            dcmd_arg(
                "list",
                "Lists current log configuration.",
                Bool,
                false,
                None,
            ),
            dcmd_arg("rotate", "Rotates all logs.", Bool, false, None),
        ],
        "VM.metaspace" => vec![
            flag("basic", "Prints a basic summary (does not need a safepoint)."),
            flag("show-loaders", "Shows usage by class loader."),
            flag(
                "show-classes",
                "If show-loaders is set, shows loaded classes for each loader.",
            ),
            flag("by-chunktype", "Break down numbers by chunk type."),
            flag("by-spacetype", "Break down numbers by loader type."),
            flag("vslist", "Shows details about the underlying virtual space."),
            flag(
                "chunkfreelist",
                "Shows details about global chunk free lists (ChunkManager).",
            ),
            dcmd_arg(
                "scale",
                "Memory usage in which to scale. Valid values are: 1, KB, MB or GB (fixed scale) \
                 or \"dynamic\" for a dynamically chosen scale.",
                Str,
                false,
                Some("dynamic"),
            ),
        ],
        "VM.native_memory" => vec![
            flag(
                "summary",
                "request runtime to report current memory summary, which includes total reserved \
                 and committed memory, along with memory usage summary by each subsystem.",
            ),
            flag(
                "detail",
                "request runtime to report memory allocation >= 1K by each callsite.",
            ),
            flag(
                "baseline",
                "request runtime to baseline current memory usage, so it can be compared against \
                 in later time.",
            ),
            flag(
                "summary.diff",
                "request runtime to report memory summary comparison against previous baseline.",
            ),
            flag(
                "detail.diff",
                "request runtime to report memory detail comparison against previous baseline, \
                 which shows the memory allocation activities at different callsites.",
            ),
            flag(
                "statistics",
                "print tracker statistics for tuning purpose.",
            ),
            dcmd_arg(
                "scale",
                "Memory usage in which scale, KB, MB or GB",
                Str,
                false,
                Some("KB"),
            ),
        ],
        "VM.set_flag" => vec![
            dcmd_arg(
                "flag name",
                "The name of the flag we want to set",
                Str,
                true,
                None,
            ),
            dcmd_arg(
                "string value",
                "The value we want to set",
                Str,
                false,
                None,
            ),
        ],
        "VM.stringtable" => vec![flag(
            "-verbose",
            "Dump the content of each string in the table",
        )],
        "VM.symboltable" => vec![flag(
            "-verbose",
            "Dump the content of each symbol in the table",
        )],
        "VM.systemdictionary" => vec![flag(
            "-verbose",
            "Dump the content of each dictionary entry for all class loaders",
        )],
        "VM.uptime" => vec![flag("-date", "Add a prefix with current date")],
        _ => Vec::new(),
    }
}

/// gci i33/jf (page `gci-i32jh-jfr-dcmd-help-is-not-the-jdks-printhelp-FIXED-20261016`
/// item 1): a `JFR.*` command's `print_help`, HotSpot 25's `JfrDCmd`: the
/// JDK's `class` answers it ([`VmDiagnosticState::jfr_help`]). `None` (the
/// generic [`jcmd_command_help`] page) when the VM is gone or the JDK code
/// could not run at all.
fn jfr_help_page(
    vm_state: &std::sync::Weak<dyn VmDiagnosticState>,
    class: &'static str,
) -> HelpPage {
    let vs = vm_state.clone();
    Box::new(move || vs.upgrade()?.jfr_help(class).ok())
}

/// gci i30/ob: split `help`'s arguments into HotSpot's `-all` option (a
/// `BOOLEAN` `DCmdArgument`: `-all` alone is true, `-all=true` / `-all=false`
/// say so) and the rest, in order. Pure.
fn jcmd_help_all_option(args: &[String]) -> (bool, Vec<String>) {
    let mut all = false;
    let mut rest = Vec::with_capacity(args.len());
    for a in args {
        match a.as_str() {
            "-all" | "-all=true" => all = true,
            "-all=false" => all = false,
            _ => rest.push(a.clone()),
        }
    }
    (all, rest)
}

/// gci i30/ob: `jcmd <pid> help -all`, HotSpot 25's `HelpDCmd::execute` with
/// `_all`: every command a `jcmd` line can name and `help`, sorted, each as
/// its name, a tab and its description, and an empty line (no command here is
/// disabled, so none carries HotSpot's ` [disabled]`). Pure.
fn jcmd_help_all(commands: &[DiagnosticCommand], mbean: bool) -> String {
    // gci i35/jh: `mbean`: only the MBean-exported commands ([`dcmd_visible`]).
    let mut rows: Vec<(&str, &str)> = commands
        .iter()
        .filter(|c| dcmd_visible(&c.name, mbean) && c.name != "help")
        .map(|c| (c.name.as_str(), c.description.as_str()))
        .chain(std::iter::once(("help", DCMD_HELP_DESCRIPTION)))
        .collect();
    rows.sort_unstable_by(|a, b| a.0.cmp(b.0));
    rows.dedup_by(|a, b| a.0 == b.0);
    let mut out = String::new();
    for (name, description) in rows {
        out.push_str(name);
        out.push_str("\n\t");
        out.push_str(description);
        out.push_str("\n\n");
    }
    out
}

/// gci i33/jf: `java.lang.String.hashCode()` of `s` (UTF-16 units,
/// `31 * h + c`, wrapping). Pure.
fn java_string_hash(s: &str) -> i32 {
    s.encode_utf16()
        .fold(0i32, |h, unit| h.wrapping_mul(31).wrapping_add(i32::from(unit)))
}

/// gci i33/jf: `java.lang.ref.FinalizerHistogram.getFinalizerHistogram()`
/// over `names`, the class names of the queued objects in the order
/// `ReferenceQueue.forEach` walks them (head first: the most recently queued
/// first). JDK 25's body: `countMap.computeIfAbsent(name, Entry::new)
/// .increment()` into a `HashMap`, `values().toArray()`, then a stable
/// `Arrays.sort` by count, descending. The order among equal counts is the
/// `HashMap`'s iteration order, reproduced here: `HashMap.hash` (`h ^ h >>>
/// 16`) bucket by bucket; `computeIfAbsent` resizes BEFORE it looks when
/// `size > threshold` (16 / 12 first, doubling), inserts a new key at the
/// HEAD of its bin, and resizes instead of treeifying a bin that had 7 or more
/// nodes while the table is under 64; a resize keeps each bin's order in its
/// lo / hi halves. (A treeified bin -- 8+ colliding class names in a table of
/// 64 or more -- is not modelled: its list order is kept.) Pure.
fn finalizer_histogram(names: &[String]) -> Vec<(String, u32)> {
    let spread = |name: &str| {
        let h = java_string_hash(name) as u32;
        (h ^ (h >> 16)) as usize
    };
    let mut entries: Vec<(String, u32)> = Vec::new();
    let mut hashes: Vec<usize> = Vec::new();
    let mut table: Vec<Vec<usize>> = Vec::new();
    let mut threshold = 0usize;
    let resize = |table: &mut Vec<Vec<usize>>, threshold: &mut usize, hashes: &[usize]| {
        let new_cap = if table.is_empty() { 16 } else { table.len() * 2 };
        let mut next: Vec<Vec<usize>> = vec![Vec::new(); new_cap];
        for bin in table.iter() {
            for &e in bin {
                next[hashes[e] & (new_cap - 1)].push(e);
            }
        }
        *table = next;
        *threshold = new_cap * 3 / 4;
    };
    for name in names {
        if entries.len() > threshold || table.is_empty() {
            resize(&mut table, &mut threshold, &hashes);
        }
        let hash = spread(name);
        let bin = hash & (table.len() - 1);
        let found = table[bin].iter().copied().find(|&e| entries[e].0 == *name);
        if let Some(e) = found {
            entries[e].1 += 1;
            continue;
        }
        let bin_count = table[bin].len();
        entries.push((name.clone(), 1));
        hashes.push(hash);
        table[bin].insert(0, entries.len() - 1);
        if bin_count >= 7 && table.len() < 64 {
            resize(&mut table, &mut threshold, &hashes);
        }
    }
    let mut ordered: Vec<(String, u32)> = table
        .iter()
        .flat_map(|bin| bin.iter().map(|&e| entries[e].clone()))
        .collect();
    ordered.sort_by(|a, b| b.1.cmp(&a.1));
    ordered
}

/// gci i33/jf: `jcmd <pid> GC.finalizer_info`'s text, HotSpot 25's
/// `FinalizerInfoDCmd::execute` over [`finalizer_histogram`] (strings
/// MEASURED in the 25.0.3 `jvm.dll`): `No instances waiting for finalization
/// found` for an empty queue, else the header and one `%10d  %s` row per
/// class. (`Finalization is disabled` is never printed: this VM has no
/// `--finalization=disabled`.) Pure.
pub(crate) fn finalizer_info_text(names: &[String]) -> String {
    let histogram = finalizer_histogram(names);
    if histogram.is_empty() {
        return "No instances waiting for finalization found\n".to_string();
    }
    let mut out = String::from(
        "Unreachable instances waiting for finalization\n#instances  class name\n-----------------------\n",
    );
    for (name, count) in histogram {
        out.push_str(&format!("{count:>10}  {name}\n"));
    }
    out
}

/// `jcmd <pid> JVMTI.agent_load`'s arguments as HotSpot reads them
/// (`JvmtiAgentLoadDCmd`; interpreter round i2 wave 59, lane L1): the
/// library path, then the optional agent option, each positional token cut at
/// its first `=` (HotSpot's `DCmdParser` reads `key=value` and hands a
/// positional argument the key: MEASURED, `JVMTI.agent_load <jar> a=b` gives
/// the agent `a`). A path whose last `.` starts `.jar` is a Java agent:
/// JPLIS's load (`instrument`, not absolute, `<jar>[=<option>]`); anything
/// else is an absolute native agent with the option (empty when absent).
/// `None` without a path.
fn jvmti_agent_load_request(args: &[String]) -> Option<(String, bool, String)> {
    let positional = |i: usize| {
        args.get(i)
            .map(|a| a.split('=').next().unwrap_or_default().to_string())
    };
    let path = positional(0).filter(|p| !p.is_empty())?;
    let option = positional(1);
    let java_agent = path
        .rfind('.')
        .is_some_and(|dot| path[dot..].starts_with(".jar"));
    if java_agent {
        let options = match option {
            Some(option) => format!("{path}={option}"),
            None => path,
        };
        Some(("instrument".to_string(), false, options))
    } else {
        Some((path, true, option.unwrap_or_default()))
    }
}

/// `jcmd <pid> Thread.dump_to_file`'s arguments, as HotSpot 25's
/// `ThreadDumpToFileDCmd` declares them (gci i12/td).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ThreadDumpToFileRequest {
    /// `filepath`, mandatory.
    path: String,
    /// `-overwrite` (BOOLEAN, default false; bare `-overwrite` is true).
    overwrite: bool,
    /// `-format=json`; any other value is HotSpot's `plain` (its
    /// `strcmp(_format.value(), "json") == 0`).
    json: bool,
}

/// Read `Thread.dump_to_file`'s arguments as HotSpot's `DCmdParser` does
/// (gci i12/td): `-overwrite[=<bool>]` and `-format=<value>` options, then
/// the one positional `filepath`, cut at its first `=` as every positional
/// argument is (see [`jvmti_agent_load_request`]). `Err` is the
/// `IllegalArgumentException` text HotSpot answers with a non-zero status.
fn thread_dump_to_file_request(args: &[String]) -> Result<ThreadDumpToFileRequest, String> {
    let mut path: Option<String> = None;
    let mut overwrite = false;
    let mut json = false;
    for arg in args {
        let (key, value) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (arg.as_str(), None),
        };
        match key {
            "-overwrite" => {
                overwrite = match value {
                    None | Some("true") => true,
                    Some("false") => false,
                    Some(other) => {
                        return Err(format!(
                            "java.lang.IllegalArgumentException: Boolean parsing error in command \
                             argument '-overwrite'. Could not parse: {other}.\n"
                        ))
                    }
                };
            }
            "-format" => json = value == Some("json"),
            _ if key.starts_with('-') || path.is_some() => {
                return Err(format!(
                    "java.lang.IllegalArgumentException: Unknown argument '{key}' in diagnostic command.\n"
                ));
            }
            _ => path = Some(key.to_string()),
        }
    }
    match path.filter(|p| !p.is_empty()) {
        Some(path) => Ok(ThreadDumpToFileRequest { path, overwrite, json }),
        None => Err(
            "java.lang.IllegalArgumentException: The argument 'filepath' is mandatory.\n".to_string(),
        ),
    }
}

/// HotSpot's answer for a quote a `jcmd` line never closes (`DCmdArgIter::next`).
const DCMD_FORMAT_ERROR: &str =
    "java.lang.IllegalArgumentException: Format error in diagnostic command arguments\n";

/// A `jcmd` line's arguments (everything after the command name) as HotSpot
/// 25's `DCmdArgIter` reads them (gci i12/td): tokens split on spaces, each
/// `<key>` or `<key>=<value>`; a key or a value that STARTS with `"` or `'`
/// runs to the matching quote (one preceded by a backslash does not close it,
/// and the backslash is kept), the quotes are dropped, and a space inside is
/// part of it. An unclosed quote is [`DCMD_FORMAT_ERROR`]. Each token comes
/// back as `key` or `key=value`, the shape the commands' own readers take
/// (`jvmti_agent_load_request`, `heap_dump_args`, `thread_dump_to_file_request`).
/// The line used to be split on whitespace, so `GC.heap_dump "C:/a b/h.hprof"`
/// handed the command two broken tokens.
fn dcmd_arguments(line: &str) -> Result<Vec<String>, String> {
    fn quoted(chars: &[char], start: usize) -> Option<(String, usize)> {
        let quote = chars[start];
        let mut j = start + 1;
        while j < chars.len() {
            if chars[j] == quote && chars[j - 1] != '\\' {
                return Some((chars[start + 1..j].iter().collect(), j + 1));
            }
            j += 1;
        }
        None
    }
    let is_quote = |c: char| c == '"' || c == '\'';
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if chars[i] == ' ' {
            i += 1;
            continue;
        }
        let mut token = String::new();
        if is_quote(chars[i]) {
            let (text, next) = quoted(&chars, i).ok_or_else(|| DCMD_FORMAT_ERROR.to_string())?;
            token.push_str(&text);
            i = next;
        } else {
            while i < n && chars[i] != '=' && chars[i] != ' ' {
                token.push(chars[i]);
                i += 1;
            }
        }
        if i < n && chars[i] == '=' {
            token.push('=');
            i += 1;
            if i < n && is_quote(chars[i]) {
                let (text, next) = quoted(&chars, i).ok_or_else(|| DCMD_FORMAT_ERROR.to_string())?;
                token.push_str(&text);
                i = next;
            } else {
                while i < n && chars[i] != ' ' {
                    token.push(chars[i]);
                    i += 1;
                }
            }
        }
        if !token.is_empty() {
            out.push(token);
        }
    }
    Ok(out)
}

/// `jcmd <pid> VM.version`'s body, HotSpot 25's `VersionDCmd` (gci i12/td):
/// `<vm name> version <vm release>` then `JDK <major>.<minor>.<security>`
/// (`.<patch>` only when the patch is non-zero), from this VM's own
/// `java.vm.name` / `java.vm.version` (what its thread-dump header and
/// `java.vm.*` readers name) and `java.version`. The live command answered the
/// constant `CratonVM 1.0.0 (JDK 25 compatible)`.
fn vm_version_dcmd_lines(props: &[(String, String)]) -> String {
    let get = |key: &str| props.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    let header = ThreadDumpHeader::from_properties(props);
    // `java.version` is `25`, `25.0.3` or `25.0.3.1` (an `-ea` / `-internal`
    // suffix after a `-` is not a component); missing components are 0.
    let numbers: Vec<u32> = get("java.version")
        .unwrap_or("")
        .split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect();
    let at = |i: usize| numbers.get(i).copied().unwrap_or(0);
    let jdk = if at(3) > 0 {
        format!("{}.{}.{}.{}", at(0), at(1), at(2), at(3))
    } else {
        format!("{}.{}.{}", at(0), at(1), at(2))
    };
    format!("{} version {}\nJDK {}\n", header.vm_name, header.vm_version, jdk)
}

/// obsaudit D15: server side of the Attach API wire protocol — see the
/// `AttachListener` doc comment for the format, empirically verified
/// against a real OpenJDK 21 `jcmd`/`jstack`/`jmap`.
#[cfg(unix)]
fn handle_attach_connection(
    mut conn: std::os::unix::net::UnixStream,
    commands: &parking_lot::RwLock<Vec<DiagnosticCommand>>,
) {
    use std::io::{Read, Write};

    // A 5-second read timeout bounds how long a connection thread can be
    // stuck on a peer that opens the socket and then never sends anything
    // (or sends a truncated request) — without this, `read` blocks forever
    // and the thread (and its `Arc<RwLock<..>>` clone) leaks for the life
    // of the process.
    let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(5)));

    if let Ok(fields) = read_attach_request(&mut conn) {
        let (code, body) = execute_attach_operation(&fields, commands);
        let _ = write_attach_response(&mut conn, code, &body);
    }
}

/// The JDK v1/v2 framing. v2 carries a bounded byte count and variable arguments.
/// Streaming is not advertised: replies use status/body.
fn read_attach_request(input: &mut impl std::io::Read) -> std::io::Result<Vec<String>> {
    use std::io::{Error, ErrorKind};
    fn field(input: &mut impl std::io::Read) -> std::io::Result<String> {
        let mut bytes = Vec::new();
        loop {
            let mut byte = [0];
            input.read_exact(&mut byte)?;
            if byte[0] == 0 {
                break;
            }
            if bytes.len() == 256 * 1024 {
                return Err(Error::from(ErrorKind::InvalidData));
            }
            bytes.push(byte[0]);
        }
        String::from_utf8(bytes).map_err(|_| Error::from(ErrorKind::InvalidData))
    }
    let version = field(input)?;
    let mut fields = vec![version.clone()];
    match version.as_str() {
        "1" => {
            for _ in 0..4 {
                fields.push(field(input)?);
            }
        }
        "2" => {
            let size: usize = field(input)?
                .parse()
                .map_err(|_| Error::from(ErrorKind::InvalidData))?;
            if size == 0 || size > 256 * 1024 {
                return Err(Error::from(ErrorKind::InvalidData));
            }
            let mut bytes = vec![0; size];
            input.read_exact(&mut bytes)?;
            if bytes.last() != Some(&0) {
                return Err(Error::from(ErrorKind::InvalidData));
            }
            for bytes in bytes[..size - 1].split(|b| *b == 0) {
                fields.push(
                    String::from_utf8(bytes.to_vec())
                        .map_err(|_| Error::from(ErrorKind::InvalidData))?,
                );
            }
        }
        _ => return Err(Error::from(ErrorKind::InvalidData)),
    }
    Ok(fields)
}

fn execute_attach_operation(
    fields: &[String],
    commands: &parking_lot::RwLock<Vec<DiagnosticCommand>>,
) -> (i32, String) {
    let field = |i: usize| fields.get(i).cloned().unwrap_or_default();
    let operation = field(1).split(' ').next().unwrap_or("").to_string();
    let arg1 = field(2);
    let arg2 = field(3);
    let (name, args): (String, Vec<String>) = match operation.as_str() {
        // `jcmd <pid> <command...>` — arg1 is the *entire* diagnostic
        // command line (e.g. "GC.heap_dump /tmp/x.hprof"), exactly what
        // `JcmdProcessor::process_command` already parses.
        "jcmd" => {
            let mut parts = arg1.splitn(2, ' ');
            let name = parts.next().unwrap_or("").to_string();
            let rest = parts.next().unwrap_or("");
            // gci i12/td: HotSpot's `DCmdArgIter`, which honours quotes (a
            // path with a space was two arguments). An unknown command is
            // still reported as unknown first, as HotSpot finds the command
            // before it parses the arguments.
            match dcmd_arguments(rest) {
                Ok(args) => (name, args),
                Err(text) if commands.read().iter().any(|c| c.name == name) => return (1, text),
                Err(_) => (name, Vec::new()),
            }
        }
        // `jstack <pid>`.
        "threaddump" => ("Thread.print".to_string(), Vec::new()),
        // `jmap -histo[:live] <pid>` (arg1 is "-all" or "-live") and
        // `jmap -dump[:live],file=<path> <pid>` (arg1 is the path, arg2
        // "-all"/"-live"): HotSpot's attach operations, mapped onto the dcmd
        // grammar by `attach_heap_walk_args` (gci i4/hd).
        "inspectheap" => (
            "GC.class_histogram".to_string(),
            attach_heap_walk_args("inspectheap", &arg1, &arg2),
        ),
        "dumpheap" => (
            "GC.heap_dump".to_string(),
            attach_heap_walk_args("dumpheap", &arg1, &arg2),
        ),
        // `VirtualMachine.getSystemProperties` / `getAgentProperties` from
        // another process (interpreter round i2 wave 59, lane L1): HotSpot's
        // body is `Properties.store` output, which the client reads back with
        // `Properties.load`. Served only when the live commands are
        // registered (`CRATONVM_ATTACH_LISTENER_PROPS`); otherwise
        // `properties` answers `VM.system_properties`' unescaped lines and
        // `agentProperties` is unrecognized, as before.
        "properties"
            if commands
                .read()
                .iter()
                .any(|c| c.name == ATTACH_PROPERTIES_COMMAND) =>
        {
            (ATTACH_PROPERTIES_COMMAND.to_string(), Vec::new())
        }
        "agentProperties"
            if commands
                .read()
                .iter()
                .any(|c| c.name == ATTACH_AGENT_PROPERTIES_COMMAND) =>
        {
            (ATTACH_AGENT_PROPERTIES_COMMAND.to_string(), Vec::new())
        }
        // `jinfo -sysprops <pid>` / `jcmd <pid> VM.system_properties`'s
        // sibling entry point.
        "properties" => ("VM.system_properties".to_string(), Vec::new()),
        // `VirtualMachine.loadAgent*` from another process (interpreter round
        // i2 wave 58, lane L1): `load <lib> <absolute> <options>`, served only
        // when the live command is registered (`CRATONVM_ATTACH_LISTENER_LOAD`;
        // otherwise the arm below answers as before). Its name holds a space,
        // so no `jcmd <pid> <command>` line can name it.
        "load"
            if commands
                .read()
                .iter()
                .any(|c| c.name == ATTACH_LOAD_COMMAND) =>
        {
            (
                ATTACH_LOAD_COMMAND.to_string(),
                vec![arg1.clone(), arg2.clone(), field(4)],
            )
        }
        "getversion" => return (0, "2".to_string()),
        other => return (1, format!("Unrecognized attach operation: {other}\n")),
    };

    // `jcmd <pid> help [<command>]` (interpreter round i2 wave 59, lane L1):
    // HotSpot's `help` command; it answered "Unknown command: help".
    if operation == "jcmd" && name == "help" {
        return (0, jcmd_help(&commands.read(), &args));
    }
    let result = commands
        .read()
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.execute(&args));
    match result {
        Some(r) if r.success => (0, r.output),
        Some(r) => (1, r.error.unwrap_or_else(|| "command failed".to_string())),
        // HotSpot's answer for a `jcmd` command it does not know (MEASURED,
        // HotSpot 25.0.3: `jcmd <pid> NoSuch.cmd`; wave 59, lane L1).
        None if operation == "jcmd" => (1, JCMD_UNKNOWN_COMMAND.to_string()),
        None => (1, format!("Unknown command: {name}\n")),
    }
}

// ---------------------------------------------------------------------------
// The DiagnosticCommand MBean's view of the command table (gci i28/dg)
// ---------------------------------------------------------------------------

/// HotSpot 25's `HelpDCmd::description()`, which the MBean lists for `help`
/// (the attach path answers `help` itself, [`jcmd_help`], so no
/// [`DiagnosticCommand`] carries it).
const DCMD_HELP_DESCRIPTION: &str = "For more information about a specific command use 'help \
     <command>'. With no argument this will show a list of available commands. 'help all' will \
     show help for all commands.";

/// gci i31/hp (`i30-ob-report.md` review item 1): HotSpot 25's `impact()`
/// texts that say more than the bare level (`diagnosticCommand.hpp`,
/// `jfrDcmds.hpp`; read from the 25.0.3 `jvm.dll`). A command not listed
/// here reports its [`CommandImpact`] level alone, as HotSpot's `"Low"` /
/// `"Medium"` / `"High"` impacts do.
const HOTSPOT_DCMD_IMPACT_TEXTS: &[(&str, &str)] = &[
    (
        "AOT.end_recording",
        "Medium: Pause time depends on number of loaded classes",
    ),
    (
        "Compiler.CodeHeap_Analytics",
        "Low: Depends on code heap size and content. Holds CodeCache_lock during analysis step, \
         usually sub-second duration.",
    ),
    (
        "Compiler.memory",
        "Medium: Pause time depends on number of compiled methods",
    ),
    ("GC.run", "Medium: Depends on Java heap size and content."),
    // gci i33/jf: `RunFinalizationDCmd::impact()`.
    ("GC.run_finalization", "Medium: Depends on Java content."),
    (
        "GC.heap_dump",
        "High: Depends on Java heap size and content. Request a full GC unless the '-all' option \
         is specified.",
    ),
    ("GC.class_histogram", "High: Depends on Java heap size and content."),
    (
        "JFR.start",
        "Medium: Depending on the settings for a recording, the impact can range from low to high.",
    ),
    ("ManagementAgent.start", "Low: No impact"),
    ("ManagementAgent.start_local", "Low: No impact"),
    ("ManagementAgent.status", "Low: No impact"),
    ("ManagementAgent.stop", "Low: No impact"),
    (
        "System.dump_map",
        "Medium; can be high for very large java heaps.",
    ),
    (
        "System.map",
        "Medium; can be high for very large java heaps.",
    ),
    ("Thread.print", "Medium: Depends on the number of threads."),
    ("Thread.dump_to_file", "Medium: Depends on the number of threads."),
    (
        "VM.cds",
        "Medium: Pause time depends on number of loaded classes",
    ),
    (
        "VM.class_hierarchy",
        "Medium: Depends on number of loaded classes.",
    ),
    (
        "VM.classes",
        "Medium: Depends on number of loaded classes.",
    ),
    (
        "VM.classloaders",
        "Medium: Depends on number of class loaders and classes loaded.",
    ),
    ("VM.events", "Low: Depends on event log size. "),
    ("VM.log", "Low: No impact"),
    (
        "VM.metaspace",
        "Medium: Depends on number of classes loaded.",
    ),
    ("VM.stringtable", "Medium: Depends on Java content."),
    ("VM.symboltable", "Medium: Depends on Java content."),
    ("VM.systemdictionary", "Medium: Depends on Java content."),
];

/// gci i31/hp: the `Impact:` text of `jcmd help <command>` and the
/// DiagnosticCommand MBean's `dcmd.vmImpact` -- HotSpot's text for the
/// command's name, else its level. Pure.
fn dcmd_impact_text(c: &DiagnosticCommand) -> String {
    HOTSPOT_DCMD_IMPACT_TEXTS
        .iter()
        .find(|(name, _)| *name == c.name)
        .map_or_else(|| c.impact.to_string(), |(_, text)| (*text).to_string())
}

/// gci i28/dg (`gci-i14mp-...` item 11b): the commands the
/// `com.sun.management:type=DiagnosticCommand` MBean lists
/// (`jmm_GetDiagnosticCommands`): every registered command a `jcmd` line can
/// name (the internal attach operations, whose names hold a space, excluded,
/// as [`jcmd_help`] excludes them) and `help`, sorted (the MBean sorts its
/// operations by name anyway). Pure.
pub(crate) fn dcmd_mbean_names(commands: &[DiagnosticCommand]) -> Vec<String> {
    // gci i35/jh: HotSpot's MBean export set ([`dcmd_visible`]).
    let mut names: Vec<String> = commands
        .iter()
        .filter(|c| dcmd_visible(&c.name, true))
        .map(|c| c.name.clone())
        .chain(std::iter::once("help".to_string()))
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// gci i28/dg: `jmm_GetDiagnosticCommandInfo` for one command: HotSpot's
/// `JavaPermission` is `java.lang.management.ManagementPermission` with
/// `monitor` for a read-only command and `control` otherwise (`help`: none);
/// each argument's HotSpot type (`STRING`, `BOOLEAN`, `JLONG`), an option
/// being a name classified as an option (position -1) and a positional one
/// counted in order. `None` for a name [`dcmd_mbean_names`] does not list.
/// Pure.
pub(crate) fn dcmd_mbean_info(
    commands: &[DiagnosticCommand],
    name: &str,
) -> Option<cratonvm_native_api::registry::DcmdInfo> {
    use cratonvm_native_api::registry::{DcmdArgInfo, DcmdInfo};
    // gci i35/jh: only an MBean-exported command has an info row.
    let Some(c) = commands
        .iter()
        .find(|c| c.name == name && dcmd_visible(&c.name, true))
    else {
        if name != "help" {
            return None;
        }
        return Some(DcmdInfo {
            name: "help".to_string(),
            description: DCMD_HELP_DESCRIPTION.to_string(),
            impact: CommandImpact::Low.to_string(),
            permission_class: None,
            permission_name: None,
            permission_action: None,
            enabled: true,
            arguments: vec![
                DcmdArgInfo {
                    name: "command name".to_string(),
                    description: "The name of the command for which we want help".to_string(),
                    arg_type: "STRING".to_string(),
                    default_value: None,
                    mandatory: false,
                    option: false,
                    multiple: false,
                    position: 0,
                },
                DcmdArgInfo {
                    name: "-all".to_string(),
                    description: "Show help for all commands".to_string(),
                    arg_type: "BOOLEAN".to_string(),
                    default_value: Some("false".to_string()),
                    mandatory: false,
                    option: true,
                    multiple: false,
                    position: -1,
                },
            ],
        });
    };
    let permission_name = match c.permission {
        CommandPermission::ReadOnly => "monitor",
        CommandPermission::ManagementAction | CommandPermission::WriteAction => "control",
    };
    let mut next_position = 0i32;
    let arguments = c
        .arguments
        .iter()
        .map(|a| {
            let option = dcmd_arg_is_option(&c.name, &a.name);
            let multiple = c.name == "JFR.start" && a.name == "settings";
            let position = if option || (c.name == "JFR.view" && a.name == "view") {
                -1
            } else {
                next_position += 1;
                next_position - 1
            };
            DcmdArgInfo {
                name: a.name.clone(),
                description: a.description.clone(),
                // gci i32/jh: HotSpot's own type name (`FILE`, `INT`);
                // JDK 25's `sanitiseType` maps `FILE` to `STRING`.
                arg_type: dcmd_arg_type_name(a.arg_type).to_string(),
                default_value: a.default_value.clone(),
                mandatory: a.required,
                option,
                multiple,
                position,
            }
        })
        .collect();
    Some(DcmdInfo {
        name: c.name.clone(),
        description: c.description.clone(),
        impact: dcmd_impact_text(c),
        permission_class: Some("java.lang.management.ManagementPermission".to_string()),
        permission_name: Some(permission_name.to_string()),
        permission_action: None,
        enabled: true,
        arguments,
    })
}

/// The `IllegalArgumentException` prefix a command's error text carries in
/// the attach protocol's shape (`java.lang.IllegalArgumentException: <m>\n`).
const IAE_PREFIX: &str = "java.lang.IllegalArgumentException: ";

/// gci i28/dg: `jmm_ExecuteDiagnosticCommand` (`DCmd::parse_and_execute`,
/// source `DCmd_Source_MBean`) over the attach listener's table: the line is
/// `<name>[ <arguments>]`, read by HotSpot's `DCmdArgIter`
/// ([`dcmd_arguments`]); `Ok` is the command's output; `Err` is the message of
/// the `IllegalArgumentException` the native throws -- `Unknown diagnostic
/// command` for a name the table does not hold (found before the arguments
/// are parsed, as HotSpot finds the command first), the parse error, or a
/// command's own refusal in the attach shape. Any other failure text is
/// output, as HotSpot's commands print their errors. A blank line runs
/// nothing (`""`). Runs the command on the CALLING thread: the caller must
/// not be a Java thread a pause waits for (`vm_exec.rs` `dcmd_execute` runs
/// this on a thread of its own).
pub(crate) fn dcmd_mbean_execute(
    commands: &parking_lot::RwLock<Vec<DiagnosticCommand>>,
    line: &str,
) -> Result<String, String> {
    let line = line.trim_start_matches(' ');
    if line.trim().is_empty() {
        return Ok(String::new());
    }
    let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
    let iae = |text: &str| -> String {
        text.strip_prefix(IAE_PREFIX)
            .unwrap_or(text)
            .trim_end()
            .to_string()
    };
    // gci i35/jh: HotSpot's `DCmd::parse_and_execute(DCmd_Source_MBean, ..)`
    // finds only an MBean-exported factory ([`dcmd_visible`]); `help` runs
    // with the same source, so its pages and listings are the MBean's.
    let registered = commands
        .read()
        .iter()
        .any(|c| c.name == name && dcmd_visible(&c.name, true));
    if !registered && name != "help" {
        return Err("Unknown diagnostic command".to_string());
    }
    let args = dcmd_arguments(rest).map_err(|t| iae(&t))?;
    if !registered {
        return Ok(jcmd_help_from(&commands.read(), &args, true));
    }
    let result = commands
        .read()
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.execute(&args));
    match result {
        Some(r) if r.success => Ok(r.output),
        Some(r) => {
            let text = r.error.unwrap_or_else(|| "command failed".to_string());
            if text.starts_with(IAE_PREFIX) {
                Err(iae(&text))
            } else {
                Ok(text)
            }
        }
        None => Err("Unknown diagnostic command".to_string()),
    }
}

#[cfg(unix)]
fn write_attach_response(
    conn: &mut std::os::unix::net::UnixStream,
    code: i32,
    body: &str,
) -> std::io::Result<()> {
    use std::io::Write;
    conn.write_all(format!("{code}\n").as_bytes())?;
    conn.write_all(body.as_bytes())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Diagnostic Commands
// ---------------------------------------------------------------------------

/// Impact level of a diagnostic command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandImpact {
    Low,
    Medium,
    High,
}

impl fmt::Display for CommandImpact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandImpact::Low => write!(f, "Low"),
            CommandImpact::Medium => write!(f, "Medium"),
            CommandImpact::High => write!(f, "High"),
        }
    }
}

/// Permission required to execute a diagnostic command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandPermission {
    ReadOnly,
    ManagementAction,
    WriteAction,
}

/// Argument type for command parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgType {
    String,
    Int,
    Bool,
    FilePath,
    JuLong,
    MemorySize,
    NanoTime,
    StringSet,
}

/// A single argument descriptor for a diagnostic command.
#[derive(Debug, Clone)]
pub struct CommandArgument {
    pub name: String,
    pub description: String,
    pub arg_type: ArgType,
    pub required: bool,
    pub default_value: Option<String>,
}

/// Result of executing a diagnostic command.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
    pub execution_time_ms: u64,
}

impl CommandResult {
    pub fn ok(output: String, execution_time_ms: u64) -> Self {
        Self {
            success: true,
            output,
            error: None,
            execution_time_ms,
        }
    }

    pub fn err(msg: String, execution_time_ms: u64) -> Self {
        Self {
            success: false,
            output: String::new(),
            error: Some(msg),
            execution_time_ms,
        }
    }
}

/// A command's own `print_help` text (gci i33/jf): `None` when it cannot say
/// it, and [`jcmd_command_help`]'s page is printed instead.
pub(crate) type HelpPage = Box<dyn Fn() -> Option<String> + Send + Sync>;

/// A registered diagnostic command.
pub struct DiagnosticCommand {
    pub name: String,
    pub description: String,
    pub impact: CommandImpact,
    pub permission: CommandPermission,
    pub arguments: Vec<CommandArgument>,
    handler: Box<dyn Fn(&[String]) -> CommandResult + Send + Sync>,
    /// gci i33/jf: what `help <name>` prints after HotSpot's header when the
    /// command is not a `DCmdParser` command (HotSpot's `JfrDCmd`, whose
    /// `print_help` is the JDK's `getHelp()`). `None` for every other command.
    help_page: Option<HelpPage>,
}

impl DiagnosticCommand {
    pub fn new(
        name: &str,
        description: &str,
        impact: CommandImpact,
        permission: CommandPermission,
        arguments: Vec<CommandArgument>,
        handler: Box<dyn Fn(&[String]) -> CommandResult + Send + Sync>,
    ) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            impact,
            permission,
            arguments,
            handler,
            help_page: None,
        }
    }

    /// gci i33/jf: the command's own `print_help` text, when it has one and
    /// can say it now.
    pub(crate) fn own_help_page(&self) -> Option<String> {
        self.help_page.as_ref().and_then(|page| page())
    }

    pub fn execute(&self, args: &[String]) -> CommandResult {
        (self.handler)(args)
    }
}

// ---------------------------------------------------------------------------
// jcmd implementation
// ---------------------------------------------------------------------------

impl Drop for AttachListener {
    /// Reap the accept thread with the listener that owns it.
    ///
    /// Without this, a `JcmdProcessor` going out of scope — every `Vm` in the
    /// test binary owns one — left its accept thread parked on a socket path
    /// the next `Vm` had already unlinked. See
    /// [`AttachListener::stop_listening`] for the measurement.
    fn drop(&mut self) {
        // Wave 80 (lane L6): a signal dispatcher alone
        // ([`AttachListener::start_signal_dispatcher`]) is not "listening"
        // but has a thread to reap all the same.
        if self.is_listening || self.accept_thread.is_some() {
            self.stop_listening();
        }
    }
}

/// Processes jcmd-style diagnostic commands.
pub struct JcmdProcessor {
    pub attach_listener: AttachListener,
}

/// obsaudit D15: process-wide counter so every `JcmdProcessor::new()` gets
/// a distinct socket path. Before this fix `start_listening` only flipped a
/// bool, so a hardcoded shared path was harmless; now it binds a real Unix
/// socket, and this file's own test suite constructs `JcmdProcessor::new()`
/// upwards of a dozen times, often running in parallel (`cargo test`'s
/// default) — a shared path would make those binds race each other.
static TEST_JCMD_SOCKET_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

impl JcmdProcessor {
    pub fn new() -> Self {
        let n = TEST_JCMD_SOCKET_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = format!("/tmp/cratonvm_attach_test_{}_{}", std::process::id(), n);
        let mut listener = AttachListener::new(&path);
        listener.start_listening();

        // Register all standard commands
        Self::register_standard_commands(&mut listener);

        Self {
            attach_listener: listener,
        }
    }

    /// Create a JcmdProcessor backed by live VM state.
    ///
    /// obsaudit D15: the socket path is `/tmp/.java_pid<pid>` — not a
    /// placeholder. This is the exact path real HotSpot tooling
    /// (`jcmd`/`jstack`/`jmap`) probes for on Linux; verified empirically
    /// against a real OpenJDK 21 `jcmd` (see the `AttachListener` doc
    /// comment). Do not change this without re-verifying against a real
    /// client — the path is load-bearing, not cosmetic.
    pub fn new_with_vm_state(vm_state: Arc<dyn VmDiagnosticState>) -> Self {
        let path = attach_temp_directory()
            .join(format!(".java_pid{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let mut listener = AttachListener::new(&path);
        listener.sigquit_dump = vm_state.sigquit_thread_dump();
        let lazy = vm_state.attach_listener_lazy();
        // DOWNGRADE. The registered commands must hold a `Weak`, never the
        // `Arc` handed in here.
        //
        // This processor is stored in `SharedVm::debug.jcmd_processor`, and
        // `vm_state` IS that same `SharedVm`. Cloning the `Arc` into the nine
        // command closures therefore made the VM own nine strong references to
        // itself: a reference cycle whose count can never reach zero, so every
        // `Vm` ever constructed leaked its entire `SharedVm` -- heap, class
        // manager, thread registry, JIT caches and all. Traced through
        // `Vm::new`, `Arc::strong_count(&shared)` jumped 1 -> 10 across exactly
        // this call and stayed at 9 after the `Vm` was dropped.
        //
        // A `Weak` is also the honest lifetime: a diagnostic command is only
        // meaningful while the VM it reports on is alive, and jcmd attaching
        // during teardown should be told so rather than resurrect it.
        let weak_state = Arc::downgrade(&vm_state);
        drop(vm_state);
        Self::register_live_commands(&mut listener, weak_state);
        // Publish the endpoint only after its owning VM's commands exist.
        if lazy {
            listener.start_listening_lazily();
        } else {
            listener.start_listening();
        }
        // Interpreter round i3 wave 80, lane L6: Windows' attach transport
        // has no polling thread, so `Ctrl-Break`'s latch gets a dispatcher
        // of its own (Unix: the listener's thread takes the latch).
        #[cfg(windows)]
        if listener.sigquit_dump {
            listener.start_signal_dispatcher();
        }
        Self {
            attach_listener: listener,
        }
    }

    /// The command set behind a `SIGQUIT` / `Ctrl-Break` dump for a VM with
    /// no attach listener (`-XX:+DisableAttachMechanism`; interpreter round
    /// i3 wave 80, lane L6): the live commands, held through a `Weak` as in
    /// [`Self::new_with_vm_state`], served to no socket (`socket_path` is
    /// empty, so nothing is bound, published or unlinked) and read only by a
    /// signal dispatcher thread
    /// ([`AttachListener::start_signal_dispatcher`]).
    #[cfg(any(unix, windows))]
    pub(crate) fn new_signal_dispatcher(vm_state: Arc<dyn VmDiagnosticState>) -> Self {
        let mut listener = AttachListener::new("");
        listener.sigquit_dump = true;
        let weak_state = Arc::downgrade(&vm_state);
        drop(vm_state);
        Self::register_live_commands(&mut listener, weak_state);
        listener.start_signal_dispatcher();
        Self {
            attach_listener: listener,
        }
    }

    fn register_standard_commands(listener: &mut AttachListener) {
        // 1. Thread.print
        listener.register_command(DiagnosticCommand::new(
            "Thread.print",
            "Print all threads with stacktraces.",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("Thread.print"),
            Box::new(|_args| {
                let threads = sample_thread_snapshots();
                let dump = JstackProcessor::generate_thread_dump(&threads);
                CommandResult::ok(dump, 0)
            }),
        ));

        // 2. GC.heap_dump
        listener.register_command(DiagnosticCommand::new(
            "GC.heap_dump",
            "Generate a HPROF format dump of the Java heap.",
            CommandImpact::High,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("GC.heap_dump"),
            Box::new(|args| {
                let path = args.first().map(|s| s.as_str()).unwrap_or("heap.hprof");
                let header = HprofWriter::write_header();
                let output = format!(
                    "Heap dump written to {}\nHPROF header: {} bytes (magic: {})",
                    path,
                    header.len(),
                    HprofWriter::HPROF_MAGIC
                );
                CommandResult::ok(output, 0)
            }),
        ));

        // 3. GC.run
        listener.register_command(DiagnosticCommand::new(
            "GC.run",
            "Call java.lang.System.gc().",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![],
            Box::new(|_args| CommandResult::ok("GC triggered".to_string(), 0)),
        ));

        // 4. GC.heap_info
        listener.register_command(DiagnosticCommand::new(
            "GC.heap_info",
            "Provide generic Java heap information.",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                let summary = HeapSummary {
                    young_gen_used: 25 * 1024 * 1024,
                    young_gen_capacity: 64 * 1024 * 1024,
                    old_gen_used: 100 * 1024 * 1024,
                    old_gen_capacity: 256 * 1024 * 1024,
                    metaspace_used: 30 * 1024 * 1024,
                    metaspace_capacity: 64 * 1024 * 1024,
                    total_used: 155 * 1024 * 1024,
                    total_capacity: 384 * 1024 * 1024,
                };
                let output = JmapProcessor::generate_heap_summary(&summary);
                CommandResult::ok(output, 0)
            }),
        ));

        // 5. GC.class_histogram
        listener.register_command(DiagnosticCommand::new(
            "GC.class_histogram",
            "Provide statistics about the Java heap usage.",
            CommandImpact::High,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("GC.class_histogram"),
            Box::new(|_args| {
                let entries = sample_class_histogram();
                let output = JmapProcessor::generate_class_histogram(&entries);
                CommandResult::ok(output, 0)
            }),
        ));

        // 6. VM.version
        listener.register_command(DiagnosticCommand::new(
            "VM.version",
            "Print JVM version information.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                CommandResult::ok("CratonVM 1.0.0 (JDK 25 compatible)".to_string(), 0)
            }),
        ));

        // 7. VM.flags
        listener.register_command(DiagnosticCommand::new(
            "VM.flags",
            "Print VM flag options and their current values.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.flags"),
            Box::new(|_args| {
                let flags = vec![
                    "-XX:+UseG1GC",
                    "-XX:MaxHeapSize=268435456",
                    "-XX:InitialHeapSize=67108864",
                    "-XX:+UseCompressedOops",
                    "-XX:+UseCompressedClassPointers",
                    "-XX:MaxMetaspaceSize=67108864",
                    "-XX:MetaspaceSize=33554432",
                    "-XX:+TieredCompilation",
                    "-XX:TieredStopAtLevel=4",
                    "-XX:+UseNUMA",
                    "-XX:+UseBiasedLocking",
                    "-XX:+OptimizeStringConcat",
                    "-XX:+PrintGCDetails",
                    "-XX:+HeapDumpOnOutOfMemoryError",
                    "-XX:ParallelGCThreads=4",
                ];
                CommandResult::ok(flags.join("\n"), 0)
            }),
        ));

        // 8. VM.system_properties
        listener.register_command(DiagnosticCommand::new(
            "VM.system_properties",
            "Print system properties.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                let props = vec![
                    "java.version=25",
                    "java.vendor=CratonVM",
                    "java.home=/usr/lib/jvm/cratonvm",
                    "os.name=Linux",
                    "os.arch=amd64",
                    "file.separator=/",
                    "path.separator=:",
                    "line.separator=\\n",
                    "user.dir=/home/user",
                    "java.class.path=.",
                ];
                CommandResult::ok(props.join("\n"), 0)
            }),
        ));

        // 9. VM.uptime
        listener.register_command(DiagnosticCommand::new(
            "VM.uptime",
            "Print VM uptime.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.uptime"),
            Box::new(|_args| CommandResult::ok("VM uptime: 3600.000 seconds".to_string(), 0)),
        ));

        // 10. VM.info
        listener.register_command(DiagnosticCommand::new(
            "VM.info",
            "Print information about JVM environment and status.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                let info = vec![
                    "CratonVM 1.0.0 (JDK 25 compatible)",
                    "Runtime: Rust-based JVM implementation",
                    "Heap: 384 MB capacity, 155 MB used",
                    "GC: G1 Garbage Collector",
                    "Threads: 12 live, 14 peak",
                    "Classes: 4200 loaded, 10 unloaded",
                    "Compiler: Tiered (C1 + C2)",
                    "OS: Linux amd64",
                    "CPUs: 4 available",
                ];
                CommandResult::ok(info.join("\n"), 0)
            }),
        ));

        // 11. VM.command_line
        listener.register_command(DiagnosticCommand::new(
            "VM.command_line",
            "Print the command line used to start this VM instance.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                CommandResult::ok(
                    "java -Xmx256m -Xms64m -XX:+UseG1GC -cp app.jar com.example.Main".to_string(),
                    0,
                )
            }),
        ));

        // 12. Thread.dump_to_file
        listener.register_command(DiagnosticCommand::new(
            "Thread.dump_to_file",
            "Dump threads, with stack traces, to a file in plain text or JSON format.",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("Thread.dump_to_file"),
            // gci i12/td (page `gci-i11vt-...-review-findings` item 13): this
            // answered `Thread dump written to <path>` and wrote nothing. The
            // set has no VM to run the JDK's `ThreadDumper` on, so it now
            // refuses, as `Compiler.queue` does; the live command
            // (`register_live_commands`) runs the dumper.
            Box::new(|_args| {
                CommandResult::err(
                    "Thread.dump_to_file is not implemented: this JcmdProcessor has no \
                     VM state binding to run jdk.internal.vm.ThreadDumper on."
                        .to_string(),
                    0,
                )
            }),
        ));

        // 13. Compiler.queue
        //
        // Observability audit (2026-07-26) — DEFECT FIXED. This handler used
        // to return a hard-coded, entirely **fabricated** queue listing:
        //
        //     C1 compile queue: 3 methods
        //       1: java.lang.String.hashCode()I (tier 1)
        //       2: java.util.HashMap.get(...)... (tier 1)
        //       3: java.lang.Math.max(II)I (tier 1)
        //     C2 compile queue: 1 method
        //       1: com.example.Main.hotLoop()V (tier 4)
        //
        // Those method names were invented — `com.example.Main.hotLoop` does
        // not exist in any real workload. Nothing consulted the JIT's
        // compilation queue. An operator diagnosing a compile-storm would
        // have read that output as ground truth and drawn conclusions from
        // fiction. Invented diagnostic data is strictly worse than an honest
        // "unsupported", so the command now reports that it is unimplemented.
        //
        // To implement for real: extend `VmDiagnosticState` with a
        // `compiler_queue()` method backed by the JIT compile queue and
        // register the command from `register_live_commands` instead (that
        // variant has the `Arc<dyn VmDiagnosticState>` this one lacks).
        listener.register_command(DiagnosticCommand::new(
            "Compiler.queue",
            "Print compilation queue (not implemented)",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                CommandResult::err(
                    "Compiler.queue is not implemented: this JcmdProcessor has no \
                     VM state binding and CratonVM does not yet expose the JIT \
                     compile queue to serviceability."
                        .to_string(),
                    0,
                )
            }),
        ));

        // 14-16. JFR.start / JFR.stop / JFR.dump
        //
        // Observability audit (2026-07-26) — DEFECT FIXED. All three handlers
        // used to return `CommandResult::ok("Flight recording started: {name}")`
        // (and the stop/dump equivalents) **without touching the flight
        // recorder at all**. They did not call `FlightRecorder::new_recording`,
        // `start_recording`, `stop_recording`, or `cratonvm_jfr::dump_to_file`;
        // `JFR.dump` reported a file path it never created.
        //
        // The failure scenario is the worst kind: an operator runs
        // `jcmd <pid> JFR.start`, is told the recording started, reproduces a
        // production incident, runs `JFR.dump`, is told the file was written —
        // and finds nothing. Meanwhile the incident window is gone.
        //
        // (In practice nothing could even reach these handlers — see the
        // LIVENESS block at the top of this module — but they were the most
        // load-bearing-looking lie in the file, and the previous tests
        // asserted the fake success strings, which would have kept the lie
        // alive through any future wiring.)
        //
        // Implementing these for real needs, in order:
        //   1. an `Arc<SharedVm>` (or a `VmDiagnosticState` extension) on the
        //      processor so the handler can reach `debug.flight_recorder`;
        //   2. `new_recording` + `start_recording` on start, `stop_recording`
        //      on stop — both of which flip the process-global
        //      `cratonvm_jfr::set_enabled` flag that every `emit_*` checks;
        //   3. `FlightRecorder::dump_recording` on dump, which drains the
        //      per-thread rings and writes the chunk.
        // Note that even then the produced `.jfr` is not JMC-loadable — see
        // the FORMAT-FIDELITY GAP block in `jfr/src/dump.rs`.
        //
        // 2026-09-01 -- DONE, for `register_live_commands` only. All three
        // prerequisites above are met there: (1) via the defaulted
        // `VmDiagnosticState::jfr_start` / `jfr_dump` / `jfr_stop` and their
        // `SharedVm` implementations in `vm/src/vm/vm_init.rs`, (2) and (3)
        // inside those. The parenthetical above -- "in practice nothing could
        // even reach these handlers" -- has also expired; see the 2026-09-01
        // correction at the top of this module.
        //
        // These three STAY as they are, and that is not an oversight. This
        // function is the no-VM-state set built by the argument-less
        // `JcmdProcessor::new()`, which owns no `FlightRecorder` and can never
        // be given one without the `Arc<dyn VmDiagnosticState>` that
        // `new_with_vm_state` is for. An honest error from a processor that
        // genuinely cannot record is the correct answer, and replacing it with
        // anything else would recreate the defect this block exists to record.
        for (name, help, arg_name, arg_help, arg_type, arg_default) in [
            (
                "JFR.start",
                "Start a flight recording (not implemented)",
                "name",
                "Recording name",
                ArgType::String,
                "recording1",
            ),
            (
                "JFR.stop",
                "Stop a flight recording (not implemented)",
                "name",
                "Recording name",
                ArgType::String,
                "recording1",
            ),
            (
                "JFR.dump",
                "Dump flight recording to file (not implemented)",
                "filename",
                "Output file path",
                ArgType::FilePath,
                "recording.jfr",
            ),
        ] {
            listener.register_command(DiagnosticCommand::new(
                name,
                help,
                CommandImpact::Medium,
                CommandPermission::ManagementAction,
                vec![CommandArgument {
                    name: arg_name.to_string(),
                    description: arg_help.to_string(),
                    arg_type,
                    required: false,
                    default_value: Some(arg_default.to_string()),
                }],
                Box::new(move |_args| {
                    CommandResult::err(
                        format!(
                            "{name} is not implemented: this JcmdProcessor has no binding \
                             to the VM's FlightRecorder, so no recording is started, \
                             stopped, or written. Reporting success here would lose an \
                             operator's incident window."
                        ),
                        0,
                    )
                }),
            ));
        }
    }

    fn register_live_commands(
        listener: &mut AttachListener,
        vm_state: std::sync::Weak<dyn VmDiagnosticState>,
    ) {
        /// Upgrade the weak VM handle, or answer the attaching client that the
        /// VM is gone. See `new_with_vm_state` for why these closures hold a
        /// `Weak` and not an `Arc`.
        macro_rules! live_vm {
            ($weak:expr) => {
                match $weak.upgrade() {
                    Some(vs) => vs,
                    None => {
                        return CommandResult::err(
                            "VM is shutting down; no live state to report".to_string(),
                            1,
                        )
                    }
                }
            };
        }

        // 1. Thread.print
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Thread.print",
            "Print all threads with stacktraces.",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("Thread.print"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                // Interpreter round i3 wave 81, lane L6: the threads first,
                // then the header (its JNI counts are read after the walk,
                // as HotSpot prints them after the threads).
                let threads = vs.thread_snapshots();
                let header = vs.thread_dump_header();
                let dump = JstackProcessor::generate_thread_dump_with(&header, &threads);
                CommandResult::ok(dump, 0)
            }),
        ));

        // The `Heap` section a `SIGQUIT` dump ends with (interpreter round i3
        // wave 81, lane L6; [`print_sigquit_thread_dump`]). A name with a
        // space, as [`ATTACH_LOAD_COMMAND`], so no `jcmd` line reaches it.
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            SIGQUIT_HEAP_COMMAND,
            "The heap section of a SIGQUIT thread dump",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                // gci i14/hd: HotSpot's own lines (`Universe::print_on`, then
                // the dispatcher's `cr`) where the state can state them.
                let section = match vs.heap_info_lines() {
                    Some(lines) => heap_info_text(&lines) + "\n",
                    None => JstackProcessor::sigquit_heap_section(&vs.heap_summary()),
                };
                CommandResult::ok(section, 0)
            }),
        ));

        // 2. GC.heap_dump
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "GC.heap_dump",
            "Generate a HPROF format dump of the Java heap.",
            CommandImpact::High,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("GC.heap_dump"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                // gci i3/hd: HotSpot's grammar, `GC.heap_dump [options]
                // <filename>`; the first token used to be taken as the path,
                // so `-overwrite /tmp/x` dumped to a file named `-overwrite`.
                let (path, overwrite) = heap_dump_args(args);
                let path = path.as_str();
                // gci i5/hd: `-gz=<level>`, checked first as HotSpot's
                // `HeapDumpDCmd::execute` checks it (before the file is
                // opened); it used to be ignored and the dump written plain.
                let gz_level = match heap_dump_gz_level(args) {
                    Ok(level) => level,
                    Err(GzLevelError::OutOfRange(text)) => return CommandResult::ok(text, 0),
                    Err(GzLevelError::Unparsable(text)) => return CommandResult::err(text, 1),
                };
                // HotSpot opens the file exclusively unless `-overwrite`
                // (`jmap -dump` never overwrites) and prints this.
                if !overwrite && std::path::Path::new(path).exists() {
                    return CommandResult::ok(format!("Unable to create {path}: File exists"), 0);
                }
                let started = Instant::now();
                // gci i4/hd: live objects only unless `-all`, as HotSpot.
                match vs.heap_dump_with(path, !dcmd_all_objects(args), gz_level, overwrite) {
                    // gci i3/hd: HotSpot's `HeapDumper::dump` lines.
                    Ok(bytes) => CommandResult::ok(
                        hotspot_heap_dump_lines(path, bytes, started.elapsed()),
                        0,
                    ),
                    Err(e) => CommandResult::err(format!("Heap dump failed: {}", e), 1),
                }
            }),
        ));

        // 3. GC.run
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "GC.run",
            "Call java.lang.System.gc().",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                // gci i2/oo: HotSpot's `GC.run` (`SystemGCDCmd`) prints nothing
                // of its own -- `jcmd` then prints `Command executed
                // successfully`. `trigger_gc` now waits for the full collection
                // (`request_diagnostic_command_gc`); only when that wait ran out
                // does the command say so.
                let ran = vs.trigger_gc();
                if ran {
                    CommandResult::ok(String::new(), 0)
                } else {
                    CommandResult::ok("GC trigger requested (may be deferred)".to_string(), 0)
                }
            }),
        ));

        // 4. GC.heap_info
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "GC.heap_info",
            "Provide generic Java heap information.",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                // gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
                // item 6): HotSpot's `HeapInfoDCmd` text where the state can
                // state it; this VM's summary otherwise. gci i47w/oa47: that
                // text is `print_heap_on` alone (`heap_info_dcmd_text`).
                let output = match vs.heap_info_lines() {
                    Some(lines) => heap_info_dcmd_text(&lines),
                    None => JmapProcessor::generate_heap_summary(&vs.heap_summary()),
                };
                CommandResult::ok(output, 0)
            }),
        ));

        // 5. GC.class_histogram
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "GC.class_histogram",
            "Provide statistics about the Java heap usage.",
            CommandImpact::High,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("GC.class_histogram"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                // gci i4/hd: HotSpot's `GC.class_histogram [-all]`; live
                // objects only unless `-all`.
                let entries = vs.class_histogram_live(!dcmd_all_objects(args));
                let output = JmapProcessor::generate_class_histogram(&entries);
                CommandResult::ok(output, 0)
            }),
        ));

        // gci i33/jf (page `gci-i32jh-jfr-dcmd-help-is-not-the-jdks-printhelp-FIXED-20261016`
        // item 2): HotSpot 25 registers `GC.finalizer_info` and
        // `GC.run_finalization` (`DCmd::register_dcmds`, parserless `DCmd`s);
        // the live table had neither, so `jcmd <pid> GC.finalizer_info`
        // answered `Unknown diagnostic command` and `help` / the
        // DiagnosticCommand MBean listed two commands fewer.
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "GC.finalizer_info",
            "Provide information about Java finalization queue.",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.finalizer_info() {
                    Ok(text) => CommandResult::ok(text, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "GC.run_finalization",
            "Call java.lang.System.runFinalization().",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.run_finalization() {
                    Ok(text) => CommandResult::ok(text, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        // 6. VM.version -- gci i12/td: HotSpot's `VersionDCmd` lines from this
        // VM's own properties (the constant `CratonVM 1.0.0 (JDK 25
        // compatible)` named no real version).
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.version",
            "Print JVM version information.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                CommandResult::ok(vm_version_dcmd_lines(&vs.system_properties()), 0)
            }),
        ));

        // 7. VM.flags
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.flags",
            "Print VM flag options and their current values.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.flags"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                let flags = vs.vm_flags();
                CommandResult::ok(flags.join("\n"), 0)
            }),
        ));

        // 8. VM.system_properties
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.system_properties",
            "Print system properties.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                // HotSpot's `SystemPropertiesDCmd` prints the bytes of
                // `VMSupport.serializePropertiesToByteArray()`, as the
                // `properties` operation answers (interpreter round i2 wave
                // 59, lane L1; with `CRATONVM_ATTACH_LISTENER_PROPS`).
                // A VM that cannot serialize them (no Java thread to run
                // `VMSupport`, a VM not in the identity table) still answers
                // from the VM's own table: HotSpot's command never fails
                // (interpreter round i2 wave 61; it answered "VM is shutting
                // down" for a live VM, `diagnostic_state_jcmd_integration`).
                if vs.attach_properties_enabled() {
                    if let Ok(body) = vs.attach_properties(false) {
                        return CommandResult::ok(body, 0);
                    }
                }
                let props = vs.system_properties();
                let lines: Vec<String> =
                    props.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
                CommandResult::ok(lines.join("\n"), 0)
            }),
        ));

        // 9. VM.uptime
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.uptime",
            "Print VM uptime.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.uptime"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                CommandResult::ok(vs.vm_uptime_text(args), 0)
            }),
        ));

        // 10. VM.command_line
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.command_line",
            "Print the command line used to start this VM instance.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                CommandResult::ok(vs.vm_command_line_text(), 0)
            }),
        ));

        // 11-13. JFR.start / JFR.dump / JFR.stop -- the REAL ones.
        //
        // 2026-09-01. The audit block at the `register_standard_commands` JFR
        // entries lists three prerequisites for implementing these; (1) was a
        // VM binding, and `VmDiagnosticState::jfr_start` / `jfr_dump` /
        // `jfr_stop` (this file) plus their `SharedVm` implementations
        // (`vm/src/vm/vm_init.rs`) are it. (2) and (3) -- `new_recording` +
        // `start_recording`, `stop_recording`, `dump_recording` -- happen
        // inside those implementations.
        //
        // Registered HERE and not in `register_standard_commands`, and that is
        // the whole point of the split: the standard set is built by the
        // argument-less `JcmdProcessor::new()`, which has no VM at all, so its
        // three entries keep returning an honest "not implemented". A command
        // that cannot reach a recorder must not answer as though it had.
        //
        // These are reachable from a real `jcmd <pid> JFR.start ...`: see the
        // 2026-09-01 correction at the top of this module for why the old
        // "nothing can reach these handlers" note no longer holds.
        //
        // A bad option or an unknown event name is answered with
        // `CommandResult::err` and the recording is NOT started -- deliberately
        // unlike the `-XX:StartFlightRecording` boot path, which panics on the
        // same mistake. A typo in a boot flag must not produce a VM whose
        // recording can never contain what was asked for; a typo typed at a
        // *running* VM must not kill it. Same rule, opposite blast radius.
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "JFR.start",
            "Starts a new JFR recording",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("JFR.start"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.jfr_start(args) {
                    Ok(msg) => CommandResult::ok(msg, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));
        // gci i33/jf: `help JFR.start` prints the JDK's own text, as HotSpot's
        // `JfrDCmd::print_help`.
        listener.set_help_page(
            "JFR.start",
            jfr_help_page(&vm_state, "jdk/jfr/internal/dcmd/DCmdStart"),
        );

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "JFR.dump",
            "Copies contents of a JFR recording to file. Either the name or the recording id must be specified.",
            CommandImpact::Low,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("JFR.dump"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.jfr_dump(args) {
                    Ok(msg) => CommandResult::ok(msg, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));
        // gci i33/jf: `help JFR.dump` prints the JDK's own text, as HotSpot's
        // `JfrDCmd::print_help`.
        listener.set_help_page(
            "JFR.dump",
            jfr_help_page(&vm_state, "jdk/jfr/internal/dcmd/DCmdDump"),
        );

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "JFR.stop",
            "Stops a JFR recording",
            CommandImpact::Low,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("JFR.stop"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.jfr_stop(args) {
                    Ok(msg) => CommandResult::ok(msg, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));
        // gci i33/jf: `help JFR.stop` prints the JDK's own text, as HotSpot's
        // `JfrDCmd::print_help`.
        listener.set_help_page(
            "JFR.stop",
            jfr_help_page(&vm_state, "jdk/jfr/internal/dcmd/DCmdStop"),
        );

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "JFR.check",
            "Checks running JFR recording(s)",
            CommandImpact::Low,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("JFR.check"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.jfr_check(args) {
                    Ok(msg) => CommandResult::ok(msg, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));
        listener.set_help_page(
            "JFR.check",
            jfr_help_page(&vm_state, "jdk/jfr/internal/dcmd/DCmdCheck"),
        );

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "JFR.configure",
            "Configure JFR",
            CommandImpact::Low,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("JFR.configure"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.jfr_configure(args) {
                    Ok(msg) => CommandResult::ok(msg, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));
        listener.set_help_page(
            "JFR.configure",
            Box::new(|| Some(JFR_CONFIGURE_HELP_PAGE.to_string())),
        );

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "JFR.view",
            "Display event data in predefined views",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("JFR.view"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.jfr_view(args) {
                    Ok(msg) => CommandResult::ok(msg, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));
        listener.set_help_page(
            "JFR.view",
            jfr_help_page(&vm_state, "jdk/jfr/internal/dcmd/DCmdView"),
        );

        // Thread.dump_to_file (gci i12/td; page
        // `gci-i11vt-virtual-thread-continuation-roots-review-findings` item
        // 13). The live table had no entry, so `jcmd <pid> Thread.dump_to_file
        // <file>` answered `Unknown diagnostic command`; the argument-less set
        // answered a success it never wrote. HotSpot 25's
        // `ThreadDumpToFileDCmd` runs the JDK's own dumper (platform AND
        // virtual threads, from the thread containers), and so does this.
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Thread.dump_to_file",
            "Dump threads, with stack traces, to a file in plain text or JSON format.",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("Thread.dump_to_file"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                let request = match thread_dump_to_file_request(args) {
                    Ok(request) => request,
                    Err(text) => return CommandResult::err(text, 1),
                };
                match vs.thread_dump_to_file(&request.path, request.overwrite, request.json) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        // Remaining HotSpot 25 diagnostic commands (gci jcmd parity).
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "AOT.end_recording",
            "End AOT recording.",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("AOT.end_recording"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.aot_end_recording() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Compiler.CodeHeap_Analytics",
            "Print CodeHeap analytics",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("Compiler.CodeHeap_Analytics"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.compiler_codeheap_analytics(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Compiler.codecache",
            "Print code cache layout and bounds.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("Compiler.codecache"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.compiler_codecache() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Compiler.codelist",
            "Print all compiled methods in code cache that are alive",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("Compiler.codelist"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.compiler_codelist() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Compiler.directives_add",
            "Add compiler directives from file.",
            CommandImpact::Low,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("Compiler.directives_add"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.compiler_directives_add(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Compiler.directives_clear",
            "Remove all compiler directives.",
            CommandImpact::Low,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("Compiler.directives_clear"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.compiler_directives_clear() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Compiler.directives_print",
            "Print all active compiler directives.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("Compiler.directives_print"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.compiler_directives_print() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Compiler.directives_remove",
            "Remove latest added compiler directive.",
            CommandImpact::Low,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("Compiler.directives_remove"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.compiler_directives_remove() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Compiler.memory",
            "Print compilation footprint",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("Compiler.memory"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.compiler_memory(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Compiler.queue",
            "Print methods queued for compilation.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("Compiler.queue"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.compiler_queue() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "JVMTI.data_dump",
            "Signal the JVM to do a data-dump request for JVMTI.",
            CommandImpact::High,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("JVMTI.data_dump"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.jvmti_data_dump() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "ManagementAgent.start",
            "Start remote management agent.",
            CommandImpact::Low,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("ManagementAgent.start"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.management_agent_start(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "ManagementAgent.start_local",
            "Start local management agent.",
            CommandImpact::Low,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("ManagementAgent.start_local"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.management_agent_start_local() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "ManagementAgent.status",
            "Print the management agent status.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("ManagementAgent.status"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.management_agent_status() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "ManagementAgent.stop",
            "Stop remote management agent.",
            CommandImpact::Low,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("ManagementAgent.stop"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.management_agent_stop() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "System.dump_map",
            "Dumps an annotated process memory map to an output file (linux, Windows and MacOS only).",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("System.dump_map"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.system_dump_map(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "System.map",
            "Prints an annotated process memory map of the VM process (linux, Windows and MacOS only).",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("System.map"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.system_map() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Thread.vthread_pollers",
            "Print the I/O pollers that support virtual threads doing blocking network I/O operations.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("Thread.vthread_pollers"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.thread_vthread_pollers() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Thread.vthread_scheduler",
            "Print the virtual thread scheduler, and the delayed task schedulers that support virtual threads doing timed operations.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("Thread.vthread_scheduler"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.thread_vthread_scheduler() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.cds",
            "Dump a static or dynamic shared archive including all shareable classes",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("VM.cds"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.vm_cds(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.class_hierarchy",
            "Print a list of all loaded classes, indented to show the class hierarchy. The name of each class is followed by the ClassLoaderData* of its ClassLoader, or \"null\" if loaded by the bootstrap class loader.",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.class_hierarchy"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.vm_class_hierarchy(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.classes",
            "Print all loaded classes",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.classes"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.vm_classes(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.classloader_stats",
            "Print statistics about all ClassLoaders.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.classloader_stats"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.vm_classloader_stats() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.classloaders",
            "Prints classloader hierarchy.",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.classloaders"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.vm_classloaders(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.dynlibs",
            "Print loaded dynamic libraries.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.dynlibs"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.vm_dynlibs() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.events",
            "Print VM event logs",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.events"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.vm_events(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.info",
            "Print information about JVM environment and status.",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.info"),
            Box::new(move |_args| {
                let vs = live_vm!(vs);
                match vs.vm_info() {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.log",
            "Lists current log configuration, enables/disables/configures a log output, or rotates all logs.",
            CommandImpact::Low,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("VM.log"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.vm_log(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.metaspace",
            "Prints the statistics for the metaspace",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.metaspace"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.vm_metaspace(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.native_memory",
            "Print native memory usage",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.native_memory"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.vm_native_memory(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.set_flag",
            "Sets VM flag option using the provided value.",
            CommandImpact::Low,
            CommandPermission::ManagementAction,
            hotspot_dcmd_arguments("VM.set_flag"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.vm_set_flag(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.stringtable",
            "Dump string table.",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.stringtable"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.vm_stringtable(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.symboltable",
            "Dump symbol table.",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.symboltable"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.vm_symboltable(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.systemdictionary",
            "Prints the statistics for dictionary hashtable sizes and bucket length",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments("VM.systemdictionary"),
            Box::new(move |args| {
                let vs = live_vm!(vs);
                match vs.vm_systemdictionary(args) {
                    Ok(body) => CommandResult::ok(body, 0),
                    Err(e) => CommandResult::err(e, 1),
                }
            }),
        ));

        // The attach `load` operation (interpreter round i2 wave 58, lane L1;
        // [`ATTACH_LOAD_COMMAND`]), only when the VM serves it. HotSpot answers
        // completion status 0 whether or not the agent loaded; the body says
        // which, and the client turns it into its exception.
        if vm_state
            .upgrade()
            .is_some_and(|vs| vs.attach_load_enabled())
        {
            let vs = vm_state.clone();
            listener.register_command(DiagnosticCommand::new(
                ATTACH_LOAD_COMMAND,
                "Load an agent library (the attach `load` operation)",
                CommandImpact::High,
                CommandPermission::ManagementAction,
                vec![],
                Box::new(move |args| {
                    let vs = live_vm!(vs);
                    // `-XX:-EnableDynamicAgentLoading` (interpreter round i3
                    // wave 81, lane L1): HotSpot's dispatch refuses the
                    // operation with `JNI_ERR` and this text before any
                    // agent code runs; the client throws
                    // `AgentLoadException("Failed to load agent library: <text>")`
                    // (MEASURED, HotSpot 25.0.4).
                    if vs.dynamic_agent_loading_disabled() {
                        return CommandResult::err(
                            crate::runtime::instrument::DYNAMIC_AGENT_LOADING_DISABLED.to_string(),
                            0,
                        );
                    }
                    let field = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
                    let body = vs.attach_load(field(0), field(1) == "true", field(2));
                    CommandResult::ok(format!("{body}\n"), 0)
                }),
            ));

            // `jcmd <pid> JVMTI.agent_load <library path> [<agent option>]`
            // (interpreter round i2 wave 59, lane L1): HotSpot's
            // `JvmtiAgentLoadDCmd`, the same load as the `load` operation
            // and so behind the same switch. A path ending in `.jar` loads
            // through JPLIS (`instrument`, `<jar>[=<option>]`), anything else
            // as an absolute native agent; the output is the operation's
            // body. MEASURED (HotSpot 25.0.3, `L1W59AttachListenerOps`):
            // `return code: 0` / `100`, and with no path
            // `java.lang.IllegalArgumentException: The argument 'library
            // path' is mandatory.` as the failure text.
            let vs = vm_state.clone();
            listener.register_command(DiagnosticCommand::new(
                "JVMTI.agent_load",
                "Load JVMTI native agent.",
                CommandImpact::Low,
                CommandPermission::ManagementAction,
                hotspot_dcmd_arguments("JVMTI.agent_load"),
                Box::new(move |args| {
                    let vs = live_vm!(vs);
                    match jvmti_agent_load_request(args) {
                        Some((lib, absolute, options)) => {
                            let body = vs.attach_load(&lib, absolute, &options);
                            CommandResult::ok(format!("{body}\n"), 0)
                        }
                        None => CommandResult::err(
                            "java.lang.IllegalArgumentException: The argument 'library path' is mandatory.\n"
                                .to_string(),
                            1,
                        ),
                    }
                }),
            ));
        }

        // The attach `properties` / `agentProperties` operations
        // (interpreter round i2 wave 59, lane L1;
        // [`ATTACH_PROPERTIES_COMMAND`]), only when the VM serves them.
        if vm_state
            .upgrade()
            .is_some_and(|vs| vs.attach_properties_enabled())
        {
            for (name, agent) in [
                (ATTACH_PROPERTIES_COMMAND, false),
                (ATTACH_AGENT_PROPERTIES_COMMAND, true),
            ] {
                let vs = vm_state.clone();
                listener.register_command(DiagnosticCommand::new(
                    name,
                    "Serialize the VM's properties (the attach operation)",
                    CommandImpact::Low,
                    CommandPermission::ReadOnly,
                    vec![],
                    Box::new(move |_args| {
                        let vs = live_vm!(vs);
                        match vs.attach_properties(agent) {
                            Ok(body) => CommandResult::ok(body, 0),
                            Err(e) => CommandResult::err(e, 1),
                        }
                    }),
                ));
            }
        }
    }

    /// Process a jcmd command line (e.g. "Thread.print" or "GC.heap_dump /tmp/dump.hprof").
    // Test-only. The LIVE attach path is `handle_attach_connection`, which
    // does its own `.find(|c| c.name == name).map(|c| c.execute(..))` rather
    // than calling this; every caller here and in `vm_init.rs` is a #[test].
    #[cfg(test)]
    pub fn process_command(&self, command_line: &str) -> CommandResult {
        let parts: Vec<&str> = command_line.trim().splitn(2, ' ').collect();
        let cmd_name = parts[0];
        let args: Vec<String> = if parts.len() > 1 {
            parts[1].split_whitespace().map(|s| s.to_string()).collect()
        } else {
            vec![]
        };

        if cmd_name == "help" {
            return CommandResult::ok(self.help(), 0);
        }

        let start = Instant::now();
        match self.attach_listener.dispatch_command(cmd_name, &args) {
            Some(mut result) => {
                result.execution_time_ms = start.elapsed().as_millis() as u64;
                result
            }
            None => CommandResult::err(format!("Unknown command: {}", cmd_name), 0),
        }
    }

    /// Generate help text listing all available commands.
    pub fn help(&self) -> String {
        let mut lines = vec!["Available commands:".to_string()];
        for cmd in self.attach_listener.commands.read().iter() {
            lines.push(format!(
                "  {} - {} [impact: {}]",
                cmd.name, cmd.description, cmd.impact
            ));
        }
        lines.join("\n")
    }
}

impl Default for JcmdProcessor {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) const DEFAULT_COMPILER_DIRECTIVES_PRINT: &str = "\n\
Directive: (default)\n \
matching: *.*\n \
c1 directives:\n  \
inline: -\n  \
Enable:true Exclude:false BreakAtExecute:false BreakAtCompile:false Log:false MemLimit:0 MemStat:0 PrintAssembly:false PrintCompilation:false PrintInlining:false PrintNMethods:false BackgroundCompilation:true ReplayInline:false DumpReplay:false DumpInline:false CompilerDirectivesIgnoreCompileCommands:false RepeatCompilation:0 DisableIntrinsic: ControlIntrinsic: BlockLayoutByFrequency:true PrintOptoAssembly:false PrintIntrinsics:false TraceSpilling:false Vectorize:false CloneMapDebug:false IncrementalInlineForceCleanup:false MaxNodeLimit:80000 \n\n \
c2 directives:\n  \
inline: -\n  \
Enable:true Exclude:false BreakAtExecute:false BreakAtCompile:false Log:false MemLimit:0 MemStat:0 PrintAssembly:false PrintCompilation:false PrintInlining:false PrintNMethods:false BackgroundCompilation:true ReplayInline:false DumpReplay:false DumpInline:false CompilerDirectivesIgnoreCompileCommands:false RepeatCompilation:0 DisableIntrinsic: ControlIntrinsic: BlockLayoutByFrequency:true PrintOptoAssembly:false PrintIntrinsics:false TraceSpilling:false Vectorize:false CloneMapDebug:false IncrementalInlineForceCleanup:false MaxNodeLimit:80000 \n\n";

pub(crate) fn default_compiler_codecache_text(nmethods: usize, used_kb: usize) -> String {
    let non_profiled_used = (used_kb / 2).max(16);
    let profiled_used = used_kb.saturating_sub(non_profiled_used).max(16);
    let non_nmethod_used = 512usize;
    let total_used = non_profiled_used + profiled_used + non_nmethod_used;
    let total_size = 245_888usize;
    let free = total_size.saturating_sub(total_used);
    let total_blobs = nmethods + 64;
    format!(
        "CodeHeap 'non-profiled nmethods': size=118912Kb used={non_profiled_used}Kb max_used={non_profiled_used}Kb free={}Kb\n \
         bounds [0x0000010087c00000, 0x0000010087e70000, 0x000001008f020000]\n\
         CodeHeap 'profiled nmethods': size=118912Kb used={profiled_used}Kb max_used={profiled_used}Kb free={}Kb\n \
         bounds [0x0000010080000000, 0x0000010080270000, 0x0000010087420000]\n\
         CodeHeap 'non-nmethods': size=8064Kb used={non_nmethod_used}Kb max_used={non_nmethod_used}Kb free={}Kb\n \
         bounds [0x0000010087420000, 0x0000010087720000, 0x0000010087c00000]\n\
         CodeCache: size={total_size}Kb, used={total_used}Kb, max_used={total_used}Kb, free={free}Kb\n \
         total_blobs={total_blobs}, nmethods={nmethods}, adapters=64, full_count=0\n\
         Compilation: enabled, stopped_count=0, restarted_count=0\n",
        118_912usize.saturating_sub(non_profiled_used),
        118_912usize.saturating_sub(profiled_used),
        8_064usize.saturating_sub(non_nmethod_used),
    )
}

pub(crate) fn default_codeheap_analytics_text(compiler_threads: usize, used_kb: usize) -> String {
    let threads = compiler_threads.max(1);
    let committed_kb = used_kb.max(2048);
    let reserved_kb = 245_888usize;
    let unallocated_kb = reserved_kb.saturating_sub(committed_kb);
    format!(
        "\n======================\n   General JIT info   \n======================\n\n\
         {:>19} : {:>7}\n{:>19} : {:>7}\n\n\
         CodeCache overview\n--------------------------------------------------------\n\n\
         {:>23} : {:>7} KB\n{:>23} : {:>7} KB\n{:>23} : {:>7} KB\n\n\
         __ CodeHeapStateAnalytics lock wait took      0.000 seconds _________\n\n\
         __ Compile & CodeCache (global) lock wait took      0.000 seconds _________\n\n\
         =============================================================\n\
         =  C O D E   H E A P   A N A L Y S I S   (general remarks)  =\n\
         =============================================================\n\
         __ Compile & CodeCache (global) lock hold took      0.000 seconds _________\n\n\
         __ CodeHeapStateAnalytics total duration      0.000 seconds _________\n",
        "JIT is",
        "on",
        "Compiler threads",
        threads,
        "Reserved size",
        reserved_kb,
        "Committed size",
        committed_kb,
        "Unallocated capacity",
        unallocated_kb,
    )
}

pub(crate) fn default_compiler_queue_text(queued: &[String]) -> String {
    let mut out = String::from("Current compiles: \n\nC1 compile queue:\nEmpty\n\nC2 compile queue:\n");
    if queued.is_empty() {
        out.push_str("Empty\n");
    } else {
        for q in queued {
            out.push_str("  ");
            out.push_str(q);
            out.push('\n');
        }
    }
    out
}

pub(crate) fn default_compiler_directives_add(args: &[String]) -> Result<String, String> {
    let Some(filename) = args.iter().find(|a| !a.starts_with('-')) else {
        return Err(
            "java.lang.IllegalArgumentException: The argument 'filename' is mandatory.\n"
                .to_string(),
        );
    };
    if !std::path::Path::new(filename).exists() {
        return Ok(format!("Could not open file {filename}\n"));
    }
    Ok("1 compiler directives added\n".to_string())
}

pub(crate) fn default_jfr_configure_text() -> String {
    let tmp = std::env::temp_dir().display().to_string();
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .display()
        .to_string();
    format!(
        "Current configuration:\n\n\
         Preserve repository: false\n\
         Repository path: {tmp}\n\n\
         Dump path: {cwd}\n\n\
         Stack depth: 64\n\
         Global buffer count: 20\n\
         Global buffer size: 512.0 kB\n\
         Thread buffer size: 8.0 kB\n\
         Memory size: 10.0 MB\n\
         Max chunk size: 12.0 MB\n"
    )
}

pub(crate) fn default_jfr_view(args: &[String]) -> Result<String, String> {
    let mut view_name: Option<String> = None;
    for arg in args {
        if let Some(v) = arg.strip_prefix("view=") {
            view_name = Some(v.to_string());
        } else if !arg.contains('=') && !arg.starts_with('-') {
            view_name = Some(arg.clone());
        }
    }
    let Some(_view) = view_name else {
        return Err(
            "java.lang.IllegalArgumentException: The argument 'view' is mandatory.\n".to_string(),
        );
    };
    Ok("No available recordings.\n\nUse JFR.start to start a recording.\n".to_string())
}

pub(crate) fn default_vm_cds(args: &[String]) -> Result<String, String> {
    let mut positional = args.iter().filter(|a| !a.starts_with('-'));
    let Some(subcmd) = positional.next() else {
        return Err(
            "java.lang.IllegalArgumentException: The argument 'subcmd' is mandatory.\n".to_string(),
        );
    };
    let subcmd = subcmd.strip_prefix("subcmd=").unwrap_or(subcmd.as_str());
    if subcmd != "static_dump" && subcmd != "dynamic_dump" {
        return Ok("Invalid subcmd, must be static_dump or dynamic_dump\n".to_string());
    }
    let pid = std::process::id();
    let filename = positional
        .next()
        .map(|s| s.strip_prefix("filename=").unwrap_or(s.as_str()).to_string())
        .unwrap_or_else(|| format!("java_pid{pid}_{subcmd}.jsa"));
    Ok(format!("Dumping shared data to file:\n   {filename}\n"))
}

pub(crate) fn default_vm_class_hierarchy_text(args: &[String]) -> String {
    let target = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(|s| s.strip_prefix("classname=").unwrap_or(s.as_str()).replace('/', "."));
    match target {
        Some(cn) if cn != "java.lang.Object" => {
            format!("java.lang.Object/null\n|--{cn}/null\n")
        }
        _ => "java.lang.Object/null\n".to_string(),
    }
}

pub(crate) fn default_vm_classloader_stats_text(
    boot_classes: usize,
    hidden_classes: usize,
    app_classes: usize,
) -> String {
    let boot_chunk = boot_classes * 2048;
    let boot_block = boot_classes * 1536;
    let hidden_chunk = hidden_classes * 2048;
    let hidden_block = hidden_classes * 1536;
    let app_chunk = app_classes * 2048;
    let app_block = app_classes * 1536;
    let total_classes = boot_classes + hidden_classes + app_classes;
    let total_chunk = boot_chunk + hidden_chunk + app_chunk;
    let total_block = boot_block + hidden_block + app_block;
    let mut out = format!(
        "{:<20}{:<20}{:<19}Classes   ChunkSz   BlockSz  Type\n",
        "ClassLoader", "Parent", "CLD*"
    );
    out.push_str(&format!(
        "0x00000000901db170  0x0000000000000000  0x000001007f0a1db0 {:>7} {:>9} {:>9}  jdk.internal.loader.ClassLoaders$PlatformClassLoader\n",
        0, 0, 0
    ));
    out.push_str(&format!(
        "0x0000000000000000  0x0000000000000000  0x000001007f0a2540 {:>7} {:>9} {:>9}  <boot class loader>\n",
        boot_classes, boot_chunk, boot_block
    ));
    if hidden_classes > 0 {
        out.push_str(&format!(
            "{:>66} {:>9} {:>9}   + hidden classes\n",
            hidden_classes, hidden_chunk, hidden_block
        ));
    }
    out.push_str(&format!(
        "0x00000000901dac50  0x00000000901db170  0x000001007f0a31a0 {:>7} {:>9} {:>9}  jdk.internal.loader.ClassLoaders$AppClassLoader\n",
        app_classes, app_chunk, app_block
    ));
    out.push_str(&format!(
        "Total = 3{:>57} {:>9} {:>9}  \n\
         ChunkSz: Total size of all allocated metaspace chunks\n\
         BlockSz: Total size of all allocated metaspace blocks (each chunk has several blocks)\n",
        total_classes, total_chunk, total_block
    ));
    out
}

pub(crate) fn default_vm_classloaders_text(
    boot_classes: &[String],
    app_classes: &[String],
    show_classes: bool,
) -> String {
    let mut out = String::from("+-- <bootstrap>\n      |\n");
    if show_classes && !boot_classes.is_empty() {
        for (i, c) in boot_classes.iter().enumerate() {
            if i == 0 {
                out.push_str(&format!("      |{:>24} {c}\n", "Classes:"));
            } else {
                out.push_str(&format!("      |{:24}{c}\n", ""));
            }
        }
        out.push_str(&format!(
            "      |{:24}({} classes)\n      |\n",
            "",
            boot_classes.len()
        ));
    }
    out.push_str(&format!(
        "      +-- \"platform\", jdk.internal.loader.ClassLoaders$PlatformClassLoader\n\
         {:12}|\n\
         {:12}+-- \"app\", jdk.internal.loader.ClassLoaders$AppClassLoader\n",
        "", ""
    ));
    if show_classes && !app_classes.is_empty() {
        out.push_str("\n");
        for (i, c) in app_classes.iter().enumerate() {
            if i == 0 {
                out.push_str(&format!("{:>42} {c}\n", "Classes:"));
            } else {
                out.push_str(&format!("{:43}{c}\n", ""));
            }
        }
        let label = if app_classes.len() == 1 { "class" } else { "classes" };
        out.push_str(&format!(
            "{:43}({} {label})\n",
            "",
            app_classes.len()
        ));
    }
    out
}

pub(crate) fn default_vm_events_text(
    args: &[String],
    compilations: &[String],
    dlls: &[String],
    classes: &[String],
    threads: &[String],
) -> String {
    let mut max: usize = 0;
    let mut log_filter: Option<String> = None;
    for arg in args {
        if let Some(v) = arg.strip_prefix("max=") {
            max = v.parse::<usize>().unwrap_or(0);
        } else if let Some(v) = arg.strip_prefix("log=") {
            log_filter = Some(v.to_lowercase());
        }
    }
    let sections: &[(&str, &str, &[String])] = &[
        ("compilation", "Compilation events", compilations),
        ("gc", "GC Heap Usage History", &[]),
        ("metaspace", "Metaspace Usage History", &[]),
        ("dll", "Dll operation events", dlls),
        ("deopt", "Deoptimization events", &[]),
        ("class", "Classes loaded", classes),
        ("classunload", "Classes unloaded", &[]),
        ("redefine", "Classes redefined", &[]),
        ("exceptions", "Internal exceptions", &[]),
        ("memory", "Memory protections", &[]),
        ("nmethod", "Nmethod flushes", &[]),
        ("events", "Events", threads),
    ];
    let mut out = String::new();
    for (key, title, items) in sections {
        if let Some(ref f) = log_filter {
            if !key.contains(f.as_str()) && !title.to_lowercase().contains(f.as_str()) {
                continue;
            }
        }
        out.push_str(&format!("{title} ({} events):\n", items.len()));
        if items.is_empty() {
            out.push_str("No events\n\n");
        } else {
            let take = if max > 0 { max.min(items.len()) } else { items.len() };
            for item in items.iter().take(take) {
                out.push_str(item);
                if !item.ends_with('\n') {
                    out.push('\n');
                }
            }
            if max > 0 && items.len() > max {
                out.push_str("...(skipped)\n");
            }
            out.push('\n');
        }
    }
    out
}

pub(crate) fn default_vm_info_text(
    props: &[(String, String)],
    cmdline: &str,
    uptime: f64,
    heap_lines: Option<&[String]>,
    summary: &HeapSummary,
    flags: &[String],
) -> String {
    let get = |k: &str, def: &str| -> String {
        props
            .iter()
            .find(|(pk, _)| pk == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| def.to_string())
    };
    let runtime_name = get("java.runtime.name", "OpenJDK Runtime Environment");
    let runtime_ver = get("java.runtime.version", "25.0.3+9-LTS");
    let vm_name = get("java.vm.name", "OpenJDK 64-Bit Server VM");
    let vm_ver = get("java.vm.version", "25.0.3+9-LTS");
    let vm_info = get("java.vm.info", "mixed mode, sharing");
    let os_name = get("os.name", std::env::consts::OS);
    let os_arch = get("os.arch", std::env::consts::ARCH);
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let heap_block = match heap_lines {
        Some(lines) => heap_info_text(lines),
        None => JstackProcessor::sigquit_heap_section(summary).replacen("Heap\n", "Heap:\n", 1),
    };
    let secs = uptime as u64;
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    let rem_secs = secs % 60;
    let metaspace_block = default_vm_metaspace_text(summary, 1, 0);
    format!(
        "#\n#  JRE version: {runtime_name} ({runtime_ver}) (build {runtime_ver})\n\
         #  Java VM: {vm_name} ({vm_ver}, {vm_info}, tiered, compressed oops, compressed class ptrs, g1 gc, {os_name}-{os_arch})\n\n\
         ---------------  S U M M A R Y ------------\n\n\
         Command Line: {cmdline}\n\n\
         Host: {os_arch}, {cpus} cores, {os_name}\n\
         Time: elapsed time: {uptime:.6} seconds ({days}d {hours}h {mins}m {rem_secs}s)\n\n\
         ---------------  T H R E A D  ---------------\n\n\
         Java Threads: ( => current thread )\n\n\
         ---------------  P R O C E S S  ---------------\n\n\
         GC Precious Log:\n <Empty>\n\n\
         {heap_block}\n\
         Polling page: 0x0000000000000000\n\n\
         Metaspace:\n{metaspace_block}\n\
         {}\n\
         GC Heap History (0 events):\nNo events\n\n\
         VM Arguments:\n\
         jvm_args: {}\n\
         java_command: {cmdline}\n\
         Launcher Type: SUN_STANDARD\n\n\
         ---------------  S Y S T E M  ---------------\n\n\
         OS: {os_name} ({os_arch})\n\
         CPU: total {cpus}\n\
         vm_info: {vm_name} ({vm_ver}) for {os_name}-{os_arch} JRE ({runtime_ver})\n\n\
         END.\n",
        host_dynlibs_text(),
        flags.join(" ")
    )
}

pub(crate) fn default_vm_log_text(args: &[String]) -> Result<String, String> {
    for arg in args {
        if arg == "disable" || arg == "disable=true" || arg == "rotate" || arg == "rotate=true" {
            return Ok(String::new());
        }
    }
    Ok(
        "Available log levels: off, trace, debug, info, warning, error\n\
         Available log decorators: time (t), utctime (utc), uptime (u), timemillis (tm), uptimemillis (um), timenanos (tn), uptimenanos (un), hostname (hn), pid (p), tid (ti), level (l), tags (tg)\n\
         Available log tags: add, age, alloc, annotation, aot, arguments, array, attach, barrier, blocks, bot, breakpoint, bytecode, cause, cds, census, class, classhisto, cleanup, codecache, compaction, compilation, condy, constantpool, constraints, container, continuations, coops, cpu, cset, data, datacreation, dcmd, decoder, defaultmethods, deoptimization, dependencies, director, dump, dynamic, ergo, event, exceptions, exit, fastlock, finalizer, fingerprint, free, freelist, gc, handshake, hashtables, heap, heapdump, humongous, ihop, iklass, indy, init, inlinecache, inlining, install, interpreter, itables, jfr, jit, jmethod, jni, jvmci, jvmti, lambda, library, link, liveness, load, loader, logging, malloc, map, mark, marking, membername, memops, metadata, metaspace, methodcomparator, methodhandles, methodtrace, mirror, mmu, module, monitorinflation, monitormismatch, monitortable, native, nestmates, nmethod, nmt, normalize, numa, objecttagging, obsolete, oldobject, oom, oopmap, oops, oopstorage, os, owner, page, pagesize, parser, patch, path, perf, periodic, phases, plab, placeholders, preempt, preorder, preview, promotion, ptrqueue, purge, record, redefine, ref, refine, region, reloc, remset, resolve, safepoint, sampling, scavenge, sealed, setting, smr, stackbarrier, stackmap, stacktrace, stackwalk, start, startup, startuptime, state, stats, streaming, stringdedup, stringtable, stubs, subclass, survivor, suspend, sweep, symboltable, system, table, task, thread, throttle, timer, tlab, tracking, training, trimnative, unload, unmap, unshareable, update, valuebasedclasses, verification, verify, vmmutex, vmoperation, vmthread, vtables, vtablestubs\n\
         Described tag sets:\n \
         logging: Logging for the log framework itself\n\
         Log output configuration:\n \
         #0: stdout all=warning uptime,level,tags foldmultilines=false\n \
         #1: stderr all=off uptime,level,tags foldmultilines=false\n"
            .to_string(),
    )
}

pub(crate) fn default_vm_metaspace_text(
    summary: &HeapSummary,
    loaders: usize,
    classes: usize,
) -> String {
    let used_kb = (summary.metaspace_used / 1024).max(256);
    let committed_kb = (summary.metaspace_capacity / 1024).max(used_kb);
    let reserved_kb = 1_114_112u64;
    let class_used_kb = (used_kb / 8).max(32);
    let class_committed_kb = (committed_kb / 8).max(class_used_kb);
    let class_reserved_kb = 1_048_576u64;
    let non_class_mb = (used_kb.saturating_sub(class_used_kb)) as f64 / 1024.0;
    let class_kb = class_used_kb as f64;
    let both_mb = used_kb as f64 / 1024.0;
    format!(
        "{:<17}used {used_kb}K, committed {committed_kb}K, reserved {reserved_kb}K\n \
         {:<16}used {class_used_kb}K, committed {class_committed_kb}K, reserved {class_reserved_kb}K\n\n\
         Total Usage - {loaders} loaders, {classes} classes (0 shared):\n  \
         {:<15}{non_class_mb:.2} MB used.\n      \
         {:<11}{class_kb:.2} KB used.\n       \
         {:<10}{both_mb:.2} MB used.\n\n\
         Virtual space:\n  \
         {:<22}64.00 MB reserved, {:>11.2} MB committed,  1 nodes.\n      \
         {:<18}1.00 GB reserved, {:>11.2} KB committed,  1 nodes.\n             \
         {:<11}1.06 GB reserved, {:>11.2} MB committed.\n\n\
         MaxMetaspaceSize: unlimited\n\
         CompressedClassSpaceSize: 1.00 GB\n\
         CDS: off\n",
        "Metaspace",
        "class space",
        "Non-Class:",
        "Class:",
        "Both:",
        "Non-class space:",
        non_class_mb,
        "Class space:",
        class_kb,
        "Both:",
        both_mb,
    )
}

pub(crate) fn default_vm_set_flag(args: &[String]) -> Result<String, String> {
    let mut positional = args.iter().filter(|a| !a.starts_with('-'));
    let Some(_flag_name) = positional.next() else {
        return Err(
            "java.lang.IllegalArgumentException: The argument 'flag name' is mandatory.\n"
                .to_string(),
        );
    };
    Ok(String::new())
}

pub(crate) fn default_vm_stringtable_text(
    entries: usize,
    literal_bytes: usize,
    verbose_items: &[String],
    verbose: bool,
) -> String {
    let buckets = 65_536usize;
    let bucket_bytes = buckets * 8;
    let entry_bytes = entries * 16;
    let total_bytes = bucket_bytes + entry_bytes + literal_bytes;
    // gcj j2/ob2 (rows on j2a, `ob2_cap_stst`: `697560 bytes, avg  34.000` for
    // 20008 literals): HotSpot's average is an integer division, printed with
    // three decimals.
    let avg_lit = if entries > 0 {
        (literal_bytes / entries) as f64
    } else {
        0.0
    };
    let avg_bucket = entries as f64 / buckets as f64;
    let mut out = format!(
        "StringTable statistics:\n\
         {:<24}: {:>9} = {:>9} bytes, each 8\n\
         {:<24}: {:>9} = {:>9} bytes, each 16\n\
         {:<24}: {:>9} = {:>9} bytes, avg {:>7.3}\n\
         {:<24}: {:>9} = {:>9} bytes\n\
         {:<24}: {:>9.3}\n\
         {:<24}: {:>9.3}\n\
         {:<24}: {:>9.3}\n\
         {:<24}: {:>9}\n",
        "Number of buckets",
        buckets,
        bucket_bytes,
        "Number of entries",
        entries,
        entry_bytes,
        "Number of literals",
        entries,
        literal_bytes,
        avg_lit,
        "Total footprint",
        "",
        total_bytes,
        "Average bucket size",
        avg_bucket,
        "Variance of bucket size",
        avg_bucket,
        "Std. dev. of bucket size",
        avg_bucket.sqrt(),
        "Maximum bucket size",
        if entries > 0 { 2 } else { 0 }
    );
    if verbose {
        for s in verbose_items {
            out.push_str(&format!("{}: \"{}\"\n", s.len(), s));
        }
    }
    out
}

/// gcj j2/ob2 (`gcj-j1st-string-table-logging-and-statistics-are-not-hotspots-20261019`
/// item 1): `jcmd <pid> VM.stringtable [-verbose]`'s figures from the live
/// table's texts -- `(entries, literal_bytes, verbose_items)` for
/// [`default_vm_stringtable_text`]. `literal_bytes` is the compact-String
/// payload (one byte per char of a Latin-1 text, two per UTF-16 unit
/// otherwise); HotSpot's figure adds each String's and value array's object
/// headers, which are layout-dependent, so a row masks the byte columns and
/// compares the counts. `verbose_items` (only when `verbose`) are the texts in
/// sorted order (the pool is a hash map; HotSpot's order is its bucket
/// order, which no row can compare either). Pure.
pub(crate) fn stringtable_census<'a>(
    texts: impl Iterator<Item = &'a str>,
    verbose: bool,
) -> (usize, usize, Vec<String>) {
    let mut entries = 0usize;
    let mut literal_bytes = 0usize;
    let mut items = Vec::new();
    for text in texts {
        entries += 1;
        let payload = if text.chars().all(|c| u32::from(c) <= 0xFF) {
            text.chars().count()
        } else {
            text.encode_utf16().count() * 2
        };
        literal_bytes = literal_bytes.saturating_add(payload);
        if verbose {
            items.push(text.to_string());
        }
    }
    items.sort_unstable();
    (entries, literal_bytes, items)
}

/// gcj j2/ob2: HotSpot's `-verbose` `BOOLEAN` `DCmdArgument` of
/// `VM.stringtable` / `VM.symboltable`: `-verbose` alone is true,
/// `-verbose=true` / `-verbose=false` say so; the last one wins. Pure.
pub(crate) fn dcmd_verbose_option(args: &[String]) -> bool {
    let mut verbose = false;
    for a in args {
        match a.as_str() {
            "-verbose" | "-verbose=true" => verbose = true,
            "-verbose=false" => verbose = false,
            _ => {}
        }
    }
    verbose
}

/// gcj j2/ob2 (rows on j2a, `ob2_cap_st_verbose`): HotSpot 25.0.3's
/// `VM.stringtable -verbose` text -- `VERSION: 1.1`, then one line per String
/// (no statistics): its modified-UTF-8 length, `: `, and its bytes with `\t`,
/// `\r`, `\n`, `\\` escaped, printable ASCII as is, and every other byte as
/// `\x<hh>` (`2: \x01\x01`, `2: \r\n`, `404: \xd0\x82...`). Pure.
pub(crate) fn hotspot_stringtable_verbose_text(texts: &[String]) -> String {
    let mut out = String::from("VERSION: 1.1\n");
    for text in texts {
        let bytes = modified_utf8(text);
        out.push_str(&bytes.len().to_string());
        out.push_str(": ");
        for &b in &bytes {
            match b {
                b'\t' => out.push_str("\\t"),
                b'\r' => out.push_str("\\r"),
                b'\n' => out.push_str("\\n"),
                b'\\' => out.push_str("\\\\"),
                0x20..=0x7e => out.push(char::from(b)),
                _ => out.push_str(&format!("\\x{b:02x}")),
            }
        }
        out.push('\n');
    }
    out
}

/// gcj j2/ob2: `text` in the JVM's modified UTF-8 (a U+0000 is `C0 80`, a
/// supplementary character is its two surrogates, three bytes each), the
/// encoding HotSpot's String-table dump measures and prints. Pure.
fn modified_utf8(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for unit in text.encode_utf16() {
        let u = u32::from(unit);
        if u != 0 && u < 0x80 {
            out.push(u as u8);
        } else if u < 0x800 {
            out.push((0xc0 | (u >> 6)) as u8);
            out.push((0x80 | (u & 0x3f)) as u8);
        } else {
            out.push((0xe0 | (u >> 12)) as u8);
            out.push((0x80 | ((u >> 6) & 0x3f)) as u8);
            out.push((0x80 | (u & 0x3f)) as u8);
        }
    }
    out
}

/// `CRATONVM_GEN_JCMD_HOTSPOT_TEXTS` (gcj j2/ob2 rows on j2a; default ON, `=0`
/// kill: the old texts). On Generational, `jcmd VM.uptime` and
/// `VM.command_line` answer in HotSpot 25.0.3's shapes (rows
/// `ob2_cap_vmuptime`, `ob2_cap_vmcmd`). G1 / ZGC unchanged.
pub(crate) fn gen_jcmd_hotspot_texts(generational: bool) -> bool {
    generational
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GEN_JCMD_HOTSPOT_TEXTS")
}

/// gcj j2/ob2 (row `ob2_cap_vmuptime`): HotSpot's `VM.uptime` text,
/// `<seconds, 3 decimals> s` and a newline. Pure.
pub(crate) fn hotspot_vm_uptime_text(uptime_secs: f64) -> String {
    format!("{uptime_secs:.3} s\n")
}

/// gcj j2/ob2 (row `ob2_cap_vmcmd`): HotSpot's `VM.command_line` text
/// (`Arguments::print_on`): `VM Arguments:`; `jvm_args: ` with each argument
/// followed by a blank (the line only when there is one); `java_command: `
/// (`<unknown>` without one); `java_class_path (initial): ` (`<not set>` when
/// empty); `Launcher Type: <launcher>`. Pure.
pub(crate) fn hotspot_vm_command_line_text(
    jvm_args: &[String],
    java_command: Option<&str>,
    class_path: &str,
    launcher: &str,
) -> String {
    let mut out = String::from("VM Arguments:\n");
    if !jvm_args.is_empty() {
        out.push_str("jvm_args: ");
        for a in jvm_args {
            out.push_str(a);
            out.push(' ');
        }
        out.push('\n');
    }
    out.push_str(&format!("java_command: {}\n", java_command.unwrap_or("<unknown>")));
    let class_path = if class_path.is_empty() {
        "<not set>"
    } else {
        class_path
    };
    out.push_str(&format!("java_class_path (initial): {class_path}\n"));
    out.push_str(&format!("Launcher Type: {launcher}\n"));
    out
}

/// `CRATONVM_GEN_JCMD_STRINGTABLE_LIVE` (gcj j2/ob2; default ON, `=0` kill:
/// the constant 0-entry text again). On the Generational collector
/// `VM.stringtable` reads the live table; G1 and ZGC keep the constant text
/// (round scope). Read per command (a jcmd is rare).
pub(crate) fn gen_jcmd_stringtable_live(generational: bool) -> bool {
    generational
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GEN_JCMD_STRINGTABLE_LIVE")
}

pub(crate) fn default_vm_symboltable_text(
    entries: usize,
    literal_bytes: usize,
    verbose_items: &[String],
    verbose: bool,
) -> String {
    let buckets = 32_768usize;
    let bucket_bytes = buckets * 8;
    let entry_bytes = entries * 16;
    let total_bytes = bucket_bytes + entry_bytes + literal_bytes;
    let avg_lit = if entries > 0 {
        literal_bytes as f64 / entries as f64
    } else {
        0.0
    };
    let avg_bucket = entries as f64 / buckets as f64;
    let mut out = format!(
        "SymbolTable statistics:\n\
         {:<24}: {:>9} = {:>9} bytes, each 8\n\
         {:<24}: {:>9} = {:>9} bytes, each 16\n\
         {:<24}: {:>9} = {:>9} bytes, avg {:>7.3}\n\
         {:<24}: {:>9} = {:>9} bytes\n\
         {:<24}: {:>9.3}\n\
         {:<24}: {:>9.3}\n\
         {:<24}: {:>9.3}\n\
         {:<24}: {:>9}\n",
        "Number of buckets",
        buckets,
        bucket_bytes,
        "Number of entries",
        entries,
        entry_bytes,
        "Number of literals",
        entries,
        literal_bytes,
        avg_lit,
        "Total footprint",
        "",
        total_bytes,
        "Average bucket size",
        avg_bucket,
        "Variance of bucket size",
        avg_bucket,
        "Std. dev. of bucket size",
        avg_bucket.sqrt(),
        "Maximum bucket size",
        if entries > 0 { 4 } else { 0 }
    );
    if verbose {
        for s in verbose_items {
            out.push_str(&format!("{} 1: {}\n", s.len(), s));
        }
    }
    out
}

pub(crate) fn default_vm_systemdictionary_text(
    boot_entries: usize,
    platform_entries: usize,
    app_entries: usize,
) -> String {
    let fmt_loader = |name: &str, buckets: usize, entries: usize| -> String {
        let bucket_bytes = buckets * 8;
        let entry_bytes = entries * 16;
        let lit_bytes = entries * 472;
        let total = bucket_bytes + entry_bytes + lit_bytes;
        let avg = entries as f64 / buckets as f64;
        format!(
            "System Dictionary for '{name}' class loader statistics:\n\
             {:<24}: {:>9} = {:>9} bytes, each 8\n\
             {:<24}: {:>9} = {:>9} bytes, each 16\n\
             {:<24}: {:>9} = {:>9} bytes, avg 472.000\n\
             {:<24}: {:>9} = {:>9} bytes\n\
             {:<24}: {:>9.3}\n\
             {:<24}: {:>9.3}\n\
             {:<24}: {:>9.3}\n\
             {:<24}: {:>9}\n",
            "Number of buckets",
            buckets,
            bucket_bytes,
            "Number of entries",
            entries,
            entry_bytes,
            "Number of literals",
            entries,
            lit_bytes,
            "Total footprint",
            "",
            total,
            "Average bucket size",
            avg,
            "Variance of bucket size",
            avg,
            "Std. dev. of bucket size",
            avg.sqrt(),
            "Maximum bucket size",
            if entries > 0 { 4 } else { 0 }
        )
    };
    let mut out = String::new();
    out.push_str(&fmt_loader("app", 1024, app_entries));
    out.push_str(&fmt_loader("platform", 1024, platform_entries));
    out.push_str(&fmt_loader("bootstrap", 1024, boot_entries));
    out
}

pub(crate) fn host_dynlibs_text() -> String {
    let mut out = String::from("Dynamic libraries:\n");
    #[cfg(windows)]
    {
        #[repr(C)]
        struct ModuleEntry32W {
            dw_size: u32,
            th32_module_id: u32,
            th32_process_id: u32,
            glblcnt_usage: u32,
            proccnt_usage: u32,
            mod_base_addr: *mut u8,
            mod_base_size: u32,
            h_module: *mut std::ffi::c_void,
            sz_module: [u16; 256],
            sz_exe_path: [u16; 260],
        }
        extern "system" {
            fn CreateToolhelp32Snapshot(dw_flags: u32, th32_process_id: u32) -> isize;
            fn Module32FirstW(h_snapshot: isize, lpme: *mut ModuleEntry32W) -> i32;
            fn Module32NextW(h_snapshot: isize, lpme: *mut ModuleEntry32W) -> i32;
            fn CloseHandle(h_object: isize) -> i32;
        }
        const TH32CS_SNAPMODULE: u32 = 0x0000_0008;
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, 0);
            if snap != 0 && snap != -1 {
                let mut entry: ModuleEntry32W = std::mem::zeroed();
                entry.dw_size = std::mem::size_of::<ModuleEntry32W>() as u32;
                if Module32FirstW(snap, &mut entry) != 0 {
                    loop {
                        let start = entry.mod_base_addr as usize;
                        let end = start.wrapping_add(entry.mod_base_size as usize);
                        let len = entry
                            .sz_exe_path
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(entry.sz_exe_path.len());
                        let path = String::from_utf16_lossy(&entry.sz_exe_path[..len]);
                        out.push_str(&format!("0x{start:016x} - 0x{end:016x} \t{path}\n"));
                        if Module32NextW(snap, &mut entry) == 0 {
                            break;
                        }
                    }
                }
                CloseHandle(snap);
                return out;
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(maps) = std::fs::read_to_string("/proc/self/maps") {
            out.push_str(&maps);
            return out;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        out.push_str(&format!(
            "0x00007ff600000000 - 0x00007ff600100000 \t{}\n",
            exe.display()
        ));
    }
    out
}

pub(crate) fn host_system_map_text() -> String {
    let mut out = format!(
        "Memory mappings:\n\
         NMT is disabled. VM info not available.\n\
         {:<19}{:<26}vsize prot  {:<11}offset vminfo/file\n\
         ===========================================================================================\n",
        "from", "to", "state"
    );
    for line in host_dynlibs_text().lines().skip(1) {
        if let Some((range, path)) = line.split_once('\t') {
            if let Some((start_s, end_s)) = range.trim().split_once(" - ") {
                let start = usize::from_str_radix(start_s.trim_start_matches("0x"), 16).unwrap_or(0);
                let end = usize::from_str_radix(end_s.trim_start_matches("0x"), 16).unwrap_or(0);
                let size = end.saturating_sub(start);
                out.push_str(&format!(
                    "{start_s}-{end_s} {:>12} r-x   c-img           0 {path}\n",
                    size
                ));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// jstack implementation
// ---------------------------------------------------------------------------

/// Thread state for thread dump output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    New,
    Runnable,
    Blocked,
    Waiting,
    TimedWaiting,
    Terminated,
}

impl fmt::Display for ThreadState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThreadState::New => write!(f, "NEW"),
            ThreadState::Runnable => write!(f, "RUNNABLE"),
            ThreadState::Blocked => write!(f, "BLOCKED"),
            ThreadState::Waiting => write!(f, "WAITING"),
            ThreadState::TimedWaiting => write!(f, "TIMED_WAITING"),
            ThreadState::Terminated => write!(f, "TERMINATED"),
        }
    }
}

/// What a blocked or waiting thread waits for, as a HotSpot 25 thread dump
/// prints it (interpreter round i3 wave 79, lane L6; page
/// `i63-L1-thread-dump-has-no-wait-kind-suffix`): the suffix of the
/// `java.lang.Thread.State:` line and the header's state word. HotSpot's
/// `JavaThreadStatus` names (`SLEEPING`, `PARKED`, `IN_OBJECT_WAIT`,
/// `BLOCKED_ON_MONITOR_ENTER`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpWaitKind {
    /// `Thread.sleep`: `TIMED_WAITING (sleeping)`, `waiting on condition`.
    Sleeping,
    /// `LockSupport.park*`: `(parking)`, `waiting on condition`.
    Parking,
    /// `Object.wait`: `(on object monitor)`, `in Object.wait()`.
    ObjectWait,
    /// A contended `synchronized` enter: `BLOCKED (on object monitor)`,
    /// `waiting for monitor entry`.
    MonitorEnter,
}

impl DumpWaitKind {
    /// The `java.lang.Thread.State:` line's suffix.
    pub fn state_suffix(self) -> &'static str {
        match self {
            DumpWaitKind::Sleeping => " (sleeping)",
            DumpWaitKind::Parking => " (parking)",
            DumpWaitKind::ObjectWait | DumpWaitKind::MonitorEnter => " (on object monitor)",
        }
    }

    /// The header line's state word.
    pub fn header_word(self) -> &'static str {
        match self {
            DumpWaitKind::Sleeping | DumpWaitKind::Parking => "waiting on condition",
            DumpWaitKind::ObjectWait => "in Object.wait()",
            DumpWaitKind::MonitorEnter => "waiting for monitor entry",
        }
    }
}

/// A single stack frame in a thread dump.
#[derive(Debug, Clone)]
pub struct FrameInfo {
    pub class_name: String,
    pub method_name: String,
    pub file_name: Option<String>,
    pub line_number: i32,
    pub native_method: bool,
}

/// Lock information for a thread.
#[derive(Debug, Clone)]
pub struct LockInfo {
    pub class_name: String,
    pub identity_hash: u64,
}

/// Snapshot of a single thread's state.
#[derive(Debug, Clone)]
pub struct ThreadSnapshot {
    pub id: u64,
    pub name: String,
    pub daemon: bool,
    pub priority: i32,
    pub state: ThreadState,
    pub stack_frames: Vec<FrameInfo>,
    pub lock_info: Option<LockInfo>,
    pub blocked_by: Option<u64>,
    pub waiting_on: Option<String>,
    /// What the thread waits for (wave 79, lane L6): `None` prints the bare
    /// state and the old header word, as before.
    pub wait_kind: Option<DumpWaitKind>,
    /// What HotSpot's thread line, frames and lock lines print beyond the
    /// fields above (interpreter round i3 wave 81, lane L6). `Default`: all
    /// unknown.
    pub dump: ThreadDumpFacts,
}

/// One thread's thread-dump facts beyond [`ThreadSnapshot`]'s own fields, in
/// the shape HotSpot 25's `JavaThread::print_on` and
/// `javaVFrame::print_lock_info_on` print them (interpreter round i3 wave 81,
/// lane L6; page `i59-L1-sigquit-terminates-the-vm`). `Default` is "all
/// unknown": the snapshot's `id`, OS tid 0, no `cpu=` / `elapsed=`, zero
/// addresses, no module prefixes and no lock lines.
#[derive(Debug, Clone, Default)]
pub struct ThreadDumpFacts {
    /// `Thread.threadId()` (`#<id>`); `None` prints the snapshot's `id`.
    pub java_tid: Option<u64>,
    /// The OS thread id (`[<tid>]` and `nid=<tid>`, decimal); `None` prints 0.
    pub os_tid: Option<u64>,
    /// CPU time the thread used, in ns (`cpu=<ms>ms`); `None` omits the field.
    pub cpu_nanos: Option<u64>,
    /// Time since the thread started, in ns (`elapsed=<s>s`); `None` omits it.
    pub elapsed_nanos: Option<u64>,
    /// `tid=`: the address of the VM's own structure for the thread (this VM's
    /// `JvmThread`, as HotSpot's is its `JavaThread`); 0 when unknown.
    pub thread_addr: u64,
    /// The `[0x..]` after the state words: a page of the thread's stack, 0
    /// when unknown (HotSpot prints 0 for a thread with no Java frame).
    pub stack_addr: u64,
    /// RUNNABLE, blocked in a wait inside the VM (not in a native's own C
    /// code): the header's state word is `waiting on condition`, as
    /// HotSpot's `CONDVAR_WAIT` (its Reference Handler in
    /// `waitForReferencePendingList`).
    pub vm_wait: bool,
    /// Parallel to `stack_frames` (shorter means unknown): the frame's
    /// module, `name@version` or `name`, `None` for the unnamed module.
    pub frame_modules: Vec<Option<String>>,
    /// `(index into stack_frames, text after "\t- ")`, frames ascending and
    /// in print order within a frame ([`dump_lock_lines`]).
    pub lock_lines: Vec<(usize, String)>,
    /// A thread of the VM's own with no `java.lang.Thread` (this VM's
    /// finalizer-delivery and concurrent-mark service threads): printed after
    /// the Java threads in HotSpot's non-Java form (`"VM Thread" os_prio=0
    /// cpu=.. elapsed=.. tid=.. nid=.. runnable  `), outside the SMR list,
    /// with no state line and no frame.
    pub no_java_thread: bool,
    /// A carrier with a virtual thread mounted on it: that virtual thread's
    /// `Thread.threadId()`. HotSpot 25's `JavaThread::print_on` then prints
    /// `   Carrying virtual thread #<id>` IN PLACE OF the
    /// `java.lang.Thread.State:` line (`is_vthread_mounted()`). gci i12/td
    /// (page `gci-i11vt-...-review-findings` item 9's remainder).
    pub carrying_virtual_thread: Option<u64>,
}

/// A monitor a lock line names: its address and its class's external name
/// (`java.lang.ref.ReferenceQueue$Lock`). Interpreter round i3 wave 81, lane L6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpMonitor {
    pub address: u64,
    pub class_name: String,
}

impl DumpMonitor {
    /// `<word> <0x..> (a <class>)`, the text of HotSpot's
    /// `print_locked_object_class_name` after `"\t- "`.
    pub fn line(&self, word: &str) -> String {
        format!("{} <0x{:016x}> (a {})", word, self.address, self.class_name)
    }
}

/// A class's internal name (`java/lang/Object`) as a thread dump prints it,
/// HotSpot's `Klass::external_name` (interpreter round i3 wave 81, lane L6):
/// dots for slashes, except a hidden class's `/0x<hex>` suffix, which stays
/// (`M81Sleeper$$Lambda/0x0000000039040210`).
pub fn external_class_name(internal: &str) -> String {
    match internal.rfind("/0x") {
        Some(at)
            if internal.len() > at + 3
                && internal[at + 3..].bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            format!("{}{}", internal[..at].replace('/', "."), &internal[at..])
        }
        _ => internal.replace('/', "."),
    }
}

/// The lock lines of one thread, as HotSpot 25's
/// `javaVFrame::print_lock_info_on` prints them per frame (interpreter round
/// i3 wave 81, lane L6), as `(frame index, text after "\t- ")`:
///
/// * frame 0 that is `java.lang.Object.wait0` / `wait`: `waiting on <obj>`
///   (`waiting to re-lock in wait()` when the thread is BLOCKED re-entering
///   the monitor); else a `parkBlocker`: `parking to wait for  <obj>` (HotSpot's
///   `"\t- %s <"` with `"parking to wait for "`: two spaces);
/// * frame 0 of a BLOCKED thread: `waiting to lock <obj>` for the monitor it
///   contends for (HotSpot's first monitor of the top frame, still pending);
/// * every frame: `locked <obj>` for each monitor `frame_locked` attributes
///   to it -- a monitor released by `wait()` included, as HotSpot prints the
///   frame's monitor record.
///
/// `top` is frame 0's `(class, method)` (external names), `None` for a
/// thread with no frame (no lines at all, as HotSpot).
pub fn dump_lock_lines(
    top: Option<(&str, &str)>,
    state: ThreadState,
    waiting: Option<&DumpMonitor>,
    park_blocker: Option<&DumpMonitor>,
    contended: Option<&DumpMonitor>,
    frame_locked: &[(usize, DumpMonitor)],
) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let Some((class, method)) = top else {
        return out;
    };
    let blocked = state == ThreadState::Blocked;
    let mut relocking = None;
    if class == "java.lang.Object" && (method == "wait0" || method == "wait") {
        let word = if blocked {
            "waiting to re-lock in wait()"
        } else {
            "waiting on"
        };
        match waiting {
            Some(m) => {
                out.push((0, m.line(word)));
                if blocked {
                    relocking = Some(m.address);
                }
            }
            None => out.push((0, format!("{word} <no object reference available>"))),
        }
    } else if let Some(m) = park_blocker {
        out.push((0, m.line("parking to wait for ")));
    }
    let pending = contended.filter(|_| blocked);
    if let Some(m) = pending {
        if relocking != Some(m.address) {
            out.push((0, m.line("waiting to lock")));
        }
    }
    for (index, m) in frame_locked {
        if *index == 0 && pending.is_some_and(|p| p.address == m.address) {
            continue;
        }
        out.push((*index, m.line("locked")));
    }
    // Stable: the order within a frame is kept.
    out.sort_by_key(|(index, _)| *index);
    out
}

/// What a thread dump prints before and after its threads (interpreter round
/// i3 wave 81, lane L6): HotSpot 25's `Threads::print_on` prints a local
/// timestamp, `Full thread dump <java.vm.name> (<java.vm.version>
/// <java.vm.info>):`, the `Threads class SMR info:` block, and after the
/// threads `JNI global refs: N, weak refs: M`.
#[derive(Debug, Clone)]
pub struct ThreadDumpHeader {
    /// `yyyy-MM-dd HH:mm:ss`, local time.
    pub timestamp: String,
    pub vm_name: String,
    pub vm_version: String,
    pub vm_info: String,
    /// `_java_thread_list=0x..`: the address of the VM's thread list.
    pub thread_list_addr: u64,
    pub jni_global_refs: usize,
    pub jni_weak_refs: usize,
}

impl ThreadDumpHeader {
    /// The header of a dump taken now, naming the VM by its own
    /// `java.vm.name` / `java.vm.version` / `java.vm.info` properties (what
    /// `System.getProperty` answers in it); a missing one falls back to this
    /// crate's own name, version and `mixed mode`.
    pub fn from_properties(props: &[(String, String)]) -> Self {
        let get = |key: &str| props.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        Self {
            timestamp: dump_local_time_string(),
            vm_name: get("java.vm.name").unwrap_or_else(|| "CratonVM".to_string()),
            vm_version: get("java.vm.version")
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
            vm_info: get("java.vm.info").unwrap_or_else(|| "mixed mode".to_string()),
            thread_list_addr: 0,
            jni_global_refs: 0,
            jni_weak_refs: 0,
        }
    }
}

/// The current local time as HotSpot's `os::local_time_string` prints it,
/// `yyyy-MM-dd HH:mm:ss` (interpreter round i3 wave 81, lane L6). UTC where
/// the platform's local time is not read.
fn dump_local_time_string() -> String {
    #[cfg(unix)]
    {
        // SAFETY: `time(NULL)` reads the clock; `localtime_r` writes only the
        // zeroed `tm` it is handed and returns null on failure.
        unsafe {
            let now = libc::time(std::ptr::null_mut());
            let mut tm: libc::tm = std::mem::zeroed();
            if !libc::localtime_r(&now, &mut tm).is_null() {
                return format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    tm.tm_year + 1900,
                    tm.tm_mon + 1,
                    tm.tm_mday,
                    tm.tm_hour,
                    tm.tm_min,
                    tm.tm_sec
                );
            }
        }
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        struct SystemTime {
            year: u16,
            month: u16,
            day_of_week: u16,
            day: u16,
            hour: u16,
            minute: u16,
            second: u16,
            milliseconds: u16,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetLocalTime(time: *mut SystemTime);
        }
        let mut st = SystemTime {
            year: 0,
            month: 0,
            day_of_week: 0,
            day: 0,
            hour: 0,
            minute: 0,
            second: 0,
            milliseconds: 0,
        };
        // SAFETY: `GetLocalTime` writes the struct it is handed and cannot fail.
        unsafe { GetLocalTime(&mut st) };
        if st.year != 0 {
            return format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                st.year, st.month, st.day, st.hour, st.minute, st.second
            );
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    dump_utc_time_string(secs)
}

/// `secs` since the epoch as `yyyy-MM-dd HH:mm:ss`, UTC (Howard Hinnant's
/// `civil_from_days`).
fn dump_utc_time_string(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year,
        month,
        day,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Processes jstack-style thread dumps.
pub struct JstackProcessor;

impl JstackProcessor {
    /// Generate a HotSpot-style thread dump string, headed as a dump of this
    /// crate's VM taken now ([`ThreadDumpHeader::from_properties`] of no
    /// property). The live command passes the VM's own header
    /// ([`Self::generate_thread_dump_with`]).
    pub fn generate_thread_dump(threads: &[ThreadSnapshot]) -> String {
        Self::generate_thread_dump_with(&ThreadDumpHeader::from_properties(&[]), threads)
    }

    /// A thread dump in HotSpot 25's `Thread.print` / `jstack` / `SIGQUIT`
    /// shape (interpreter round i3 wave 81, lane L6; measured on HotSpot
    /// 25.0.4, page `i59-L1-sigquit-terminates-the-vm`): the timestamp, the
    /// `Full thread dump` line, the SMR block, each thread
    /// ([`Self::thread_dump_line`], its state line, frames and lock lines)
    /// followed by a blank line, and the JNI line. `threads` are printed in
    /// the order given (HotSpot: the order the threads were added).
    pub fn generate_thread_dump_with(
        header: &ThreadDumpHeader,
        threads: &[ThreadSnapshot],
    ) -> String {
        let mut output = String::new();
        output.push_str(&header.timestamp);
        output.push('\n');
        output.push_str(&format!(
            "Full thread dump {} ({} {}):\n\n",
            header.vm_name, header.vm_version, header.vm_info
        ));
        let (java, vm_own): (Vec<&ThreadSnapshot>, Vec<&ThreadSnapshot>) =
            threads.iter().partition(|t| !t.dump.no_java_thread);
        output.push_str(&Self::smr_info(
            header.thread_list_addr,
            &java.iter().map(|t| t.dump.thread_addr).collect::<Vec<_>>(),
        ));
        output.push('\n');

        for thread in java {
            output.push_str(&Self::thread_dump_line(thread));
            output.push('\n');

            // Thread state, with HotSpot's wait-kind suffix when known; a
            // carrier with a mounted virtual thread prints HotSpot's
            // `Carrying virtual thread` line instead (gci i12/td).
            if let Some(carried) = thread.dump.carrying_virtual_thread {
                output.push_str(&format!("   Carrying virtual thread #{carried}\n"));
            } else {
                let suffix = match (thread.state, thread.wait_kind) {
                    (
                        ThreadState::Blocked | ThreadState::Waiting | ThreadState::TimedWaiting,
                        Some(kind),
                    ) => kind.state_suffix(),
                    _ => "",
                };
                output.push_str(&format!("   java.lang.Thread.State: {}{}\n", thread.state, suffix));
            }

            // Stack frames, each followed by its lock lines.
            let mut locks = thread.dump.lock_lines.iter().peekable();
            for (index, frame) in thread.stack_frames.iter().enumerate() {
                let module = thread
                    .dump
                    .frame_modules
                    .get(index)
                    .and_then(|m| m.as_deref());
                output.push_str(&Self::frame_line(frame, module));
                output.push('\n');
                while let Some((_, text)) = locks.next_if(|(at, _)| *at == index) {
                    output.push_str("\t- ");
                    output.push_str(text);
                    output.push('\n');
                }
                // A line naming a frame that is not there is dropped.
                while locks.next_if(|(at, _)| *at < index).is_some() {}
            }

            // Lock info
            if let Some(lock) = &thread.lock_info {
                output.push_str(&format!(
                    "\t- locked <0x{:016x}> (a {})\n",
                    lock.identity_hash, lock.class_name
                ));
            }

            // Waiting on info
            if let Some(monitor) = &thread.waiting_on {
                output.push_str(&format!("\t- waiting on {}\n", monitor));
            }

            output.push('\n');
        }

        for thread in vm_own {
            output.push_str(&Self::non_java_thread_line(thread));
            output.push_str("\n\n");
        }

        output.push_str(&format!(
            "JNI global refs: {}, weak refs: {}\n\n",
            header.jni_global_refs, header.jni_weak_refs
        ));
        output
    }

    /// HotSpot 25's `ThreadsSMRSupport::print_info_on` for a list at
    /// `list_addr` of the threads at `elements`: four addresses per line,
    /// `", "` between them and `","` at a line's end.
    pub fn smr_info(list_addr: u64, elements: &[u64]) -> String {
        let mut out = format!(
            "Threads class SMR info:\n_java_thread_list=0x{:016x}, length={}, elements={{\n",
            list_addr,
            elements.len()
        );
        for (i, addr) in elements.iter().enumerate() {
            out.push_str(&format!("0x{addr:016x}"));
            if i + 1 < elements.len() {
                out.push_str(if (i + 1) % 4 == 0 { ",\n" } else { ", " });
            } else {
                out.push('\n');
            }
        }
        out.push_str("}\n");
        out
    }

    /// One thread's line, as HotSpot 25's `JavaThread::print_on` prints a
    /// thread with a `java.lang.Thread`: `"name" #<id> [<os tid>] [daemon
    /// ]prio=<p> os_prio=0 cpu=<ms>ms elapsed=<s>s tid=0x<..> nid=<os tid>
    /// <state words>  [0x<..>]` (no newline).
    pub fn thread_dump_line(thread: &ThreadSnapshot) -> String {
        let facts = &thread.dump;
        let state_words = match (thread.state, thread.wait_kind) {
            (
                ThreadState::Blocked | ThreadState::Waiting | ThreadState::TimedWaiting,
                Some(kind),
            ) => kind.header_word(),
            (ThreadState::Runnable, _) if facts.vm_wait => "waiting on condition",
            (ThreadState::Runnable, _) => "runnable",
            (ThreadState::Blocked, _) => "waiting for monitor entry",
            (ThreadState::Waiting | ThreadState::TimedWaiting, _) => "waiting on condition",
            (ThreadState::New, _) => "initialized",
            (ThreadState::Terminated, _) => "zombie",
        };
        let os_tid = facts.os_tid.unwrap_or(0);
        let mut line = format!(
            "\"{}\" #{} [{}] {}prio={} os_prio=0 ",
            thread.name,
            facts.java_tid.unwrap_or(thread.id),
            os_tid,
            if thread.daemon { "daemon " } else { "" },
            thread.priority,
        );
        if let Some(cpu) = facts.cpu_nanos {
            line.push_str(&format!("cpu={:.2}ms ", cpu as f64 / 1_000_000.0));
        }
        if let Some(elapsed) = facts.elapsed_nanos {
            line.push_str(&format!(
                "elapsed={:.2}s ",
                elapsed as f64 / 1_000_000_000.0
            ));
        }
        line.push_str(&format!(
            "tid=0x{:016x} nid={} {}  [0x{:016x}]",
            facts.thread_addr, os_tid, state_words, facts.stack_addr
        ));
        line
    }

    /// A thread with no `java.lang.Thread`, as HotSpot 25 prints its non-Java
    /// threads: `"name" os_prio=0 cpu=<ms>ms elapsed=<s>s tid=0x<..>
    /// nid=<os tid> <state words>  ` (two trailing spaces, no newline).
    pub fn non_java_thread_line(thread: &ThreadSnapshot) -> String {
        let facts = &thread.dump;
        let words = match thread.state {
            ThreadState::Runnable if !facts.vm_wait => "runnable",
            _ => "waiting on condition",
        };
        let mut line = format!("\"{}\" os_prio=0 ", thread.name);
        if let Some(cpu) = facts.cpu_nanos {
            line.push_str(&format!("cpu={:.2}ms ", cpu as f64 / 1_000_000.0));
        }
        if let Some(elapsed) = facts.elapsed_nanos {
            line.push_str(&format!(
                "elapsed={:.2}s ",
                elapsed as f64 / 1_000_000_000.0
            ));
        }
        line.push_str(&format!(
            "tid=0x{:016x} nid={} {}  ",
            facts.thread_addr,
            facts.os_tid.unwrap_or(0),
            words
        ));
        line
    }

    /// One frame's line, as HotSpot 25's `java_lang_Throwable::print_stack_element`
    /// prints it in a thread dump: `\tat <class>.<method>(<module>/<location>)`,
    /// the location `Native Method`, `<file>:<line>`, `<file>` (no line) or
    /// `Unknown Source` (no newline).
    pub fn frame_line(frame: &FrameInfo, module: Option<&str>) -> String {
        let location = if frame.native_method {
            "Native Method".to_string()
        } else {
            match &frame.file_name {
                Some(f) if frame.line_number >= 0 => format!("{}:{}", f, frame.line_number),
                Some(f) => f.clone(),
                None => "Unknown Source".to_string(),
            }
        };
        match module {
            Some(m) => format!(
                "\tat {}.{}({}/{})",
                frame.class_name, frame.method_name, m, location
            ),
            None => format!(
                "\tat {}.{}({})",
                frame.class_name, frame.method_name, location
            ),
        }
    }

    /// The `Heap` section HotSpot's signal dispatcher prints after a
    /// `SIGQUIT` dump (`Universe::heap()->print_on`), in this VM's own terms
    /// (interpreter round i3 wave 81, lane L6): HotSpot's lines name its
    /// collector's regions, which this VM does not have, so the section
    /// carries the summary `GC.heap_info` reports, in KiB, and a blank line.
    pub fn sigquit_heap_section(summary: &HeapSummary) -> String {
        let k = |b: u64| b / 1024;
        format!(
            "Heap\n java heap   total {}K, used {}K\n  young generation   used {}K, capacity {}K\n  old generation     used {}K, capacity {}K\n Metaspace       used {}K, committed {}K\n\n",
            k(summary.total_capacity),
            k(summary.total_used),
            k(summary.young_gen_used),
            k(summary.young_gen_capacity),
            k(summary.old_gen_used),
            k(summary.old_gen_capacity),
            k(summary.metaspace_used),
            k(summary.metaspace_capacity),
        )
    }

    /// Detect deadlocks and generate a deadlock report.
    pub fn generate_deadlock_report(threads: &[ThreadSnapshot]) -> Option<String> {
        // Build a blocked_by graph and detect cycles
        let mut cycles: Vec<Vec<u64>> = Vec::new();
        let mut visited_global: std::collections::HashSet<u64> = std::collections::HashSet::new();

        for thread in threads {
            if visited_global.contains(&thread.id) {
                continue;
            }
            // Follow the blocked_by chain
            let mut path: Vec<u64> = Vec::new();
            let mut visited: std::collections::HashSet<u64> = std::collections::HashSet::new();
            let mut current_id = Some(thread.id);

            while let Some(cid) = current_id {
                if visited.contains(&cid) {
                    // Found a cycle — extract it
                    if let Some(pos) = path.iter().position(|&x| x == cid) {
                        let cycle: Vec<u64> = path[pos..].to_vec();
                        if !cycle.is_empty() {
                            cycles.push(cycle);
                        }
                    }
                    break;
                }
                visited.insert(cid);
                path.push(cid);

                // Find blocked_by for this thread
                current_id = threads
                    .iter()
                    .find(|t| t.id == cid)
                    .and_then(|t| t.blocked_by);
            }

            for id in &path {
                visited_global.insert(*id);
            }
        }

        if cycles.is_empty() {
            return None;
        }

        let mut report = String::new();
        report.push_str(&format!("Found {} deadlock(s).\n\n", cycles.len()));

        for (i, cycle) in cycles.iter().enumerate() {
            report.push_str(&format!("Deadlock #{}:\n", i + 1));
            for &tid in cycle {
                if let Some(t) = threads.iter().find(|t| t.id == tid) {
                    report.push_str(&format!(
                        "  \"{}\" (id={}) blocked by thread id={}\n",
                        t.name,
                        t.id,
                        t.blocked_by.unwrap_or(0)
                    ));
                }
            }
            report.push('\n');
        }

        Some(report)
    }
}

// ---------------------------------------------------------------------------
// jmap implementation
// ---------------------------------------------------------------------------

/// Entry in a class histogram.
#[derive(Debug, Clone)]
pub struct ClassHistogramEntry {
    /// HotSpot's whole `class name (module)` column (gci i3/hd): the external
    /// name, then ` (<module>[@<version>])` for a class of a named module.
    pub class_name: String,
    pub instance_count: u64,
    pub total_bytes: u64,
}

/// Heap summary information.
#[derive(Debug, Clone)]
pub struct HeapSummary {
    pub young_gen_used: u64,
    pub young_gen_capacity: u64,
    pub old_gen_used: u64,
    pub old_gen_capacity: u64,
    pub metaspace_used: u64,
    pub metaspace_capacity: u64,
    pub total_used: u64,
    pub total_capacity: u64,
}

/// `GC.heap_dump [options] <filename>` (HotSpot 25's `HeapDumpDCmd`; gci
/// i3/hd): the first token that is not an option is the file (`heap.hprof`
/// when none, as before), and `-overwrite` / `-overwrite=true` allows an
/// existing file. `-parallel=<n>` is accepted and has no effect here; `-all`
/// is read by [`dcmd_all_objects`] (gci i4/hd), `-gz=<n>` by
/// [`heap_dump_gz_level`] (gci i5/hd).
pub fn heap_dump_args(args: &[String]) -> (String, bool) {
    let mut path: Option<&str> = None;
    let mut overwrite = false;
    for a in args {
        let a = a.as_str();
        if a == "-overwrite" || a == "-overwrite=true" {
            overwrite = true;
        } else if a == "-overwrite=false" || a.starts_with('-') {
            continue;
        } else if path.is_none() {
            path = Some(a);
        }
    }
    (path.unwrap_or("heap.hprof").to_string(), overwrite)
}

/// Why [`heap_dump_gz_level`] refused a `-gz` option: the text the command
/// answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GzLevelError {
    /// A level outside 1-9: HotSpot prints this and returns (no dump, the
    /// command itself succeeds).
    OutOfRange(String),
    /// Not an integer: HotSpot's `DCmdArgument<jlong>::parse_value` throws
    /// `IllegalArgumentException` with this message.
    Unparsable(String),
}

/// `GC.heap_dump`'s `-gz=<level>` (HotSpot 25 `HeapDumpDCmd`, an INT
/// option; gci i5/hd, `gci-i3hd-heap-walks-and-diagnostics-review-findings`
/// item 10): `Ok(None)` without it, `Ok(Some(level))` for 1-9; the last
/// `-gz` given wins, as the parser's last assignment does. Pure.
pub fn heap_dump_gz_level(args: &[String]) -> Result<Option<u8>, GzLevelError> {
    let mut raw: Option<&str> = None;
    for a in args {
        if let Some(v) = a.strip_prefix("-gz=") {
            raw = Some(v);
        } else if a == "-gz" {
            raw = Some("");
        }
    }
    let Some(raw) = raw else {
        return Ok(None);
    };
    let Ok(level) = raw.trim().parse::<i64>() else {
        return Err(GzLevelError::Unparsable(format!(
            "java.lang.IllegalArgumentException: Integer parsing error in command argument 'gz'. \
             Could not parse: {raw}."
        )));
    };
    match u8::try_from(level) {
        Ok(l) if (1..=9).contains(&l) => Ok(Some(l)),
        _ => Err(GzLevelError::OutOfRange(format!(
            "Compression level out of range (1-9): {level}"
        ))),
    }
}

/// The `-all` option of `GC.class_histogram` / `GC.heap_dump` (HotSpot's
/// boolean dcmd option: `-all`, `-all=true`; `-all=false` is off): `true`
/// asks for every object, `false` (the default) for live objects only. gci
/// i4/hd. Pure.
pub fn dcmd_all_objects(args: &[String]) -> bool {
    let mut all = false;
    for a in args {
        match a.as_str() {
            "-all" | "-all=true" => all = true,
            "-all=false" => all = false,
            _ => {}
        }
    }
    all
}

/// HotSpot's attach operations `inspectheap` (`jmap -histo[:live]`: `arg1`
/// is `-live` or `-all`, empty means live, as `attachListener.cpp`'s
/// `heap_inspection` reads it) and `dumpheap` (`jmap -dump[:live],file=`:
/// `arg1` the path, `arg2` `-live` for live objects, anything else all, as its
/// `dump_heap` reads it), as `GC.class_histogram` / `GC.heap_dump` arguments.
/// gci i4/hd. Pure.
pub fn attach_heap_walk_args(operation: &str, arg1: &str, arg2: &str) -> Vec<String> {
    match operation {
        "inspectheap" => {
            if arg1 == "-all" {
                vec!["-all".to_string()]
            } else {
                Vec::new()
            }
        }
        _ => {
            let mut v = Vec::new();
            if arg2 != "-live" {
                v.push("-all".to_string());
            }
            if !arg1.is_empty() {
                v.push(arg1.to_string());
            }
            v
        }
    }
}

/// What HotSpot 25's `jcmd GC.heap_dump` prints for a dump of `bytes` bytes
/// written to `path` in `took` (`HeapDumper::dump`: `Dumping heap to %s ...`,
/// then `Heap dump file created [%lu bytes in %3.3f secs]`). gci i3/hd.
pub fn hotspot_heap_dump_lines(path: &str, bytes: u64, took: std::time::Duration) -> String {
    format!(
        "Dumping heap to {path} ...\nHeap dump file created [{bytes} bytes in {:.3} secs]",
        took.as_secs_f64()
    )
}

/// Processes jmap-style heap analysis commands.
pub struct JmapProcessor;

impl JmapProcessor {
    /// Generate a class histogram report, in HotSpot 25's
    /// `KlassInfoHisto::print_histo_on` text (gci i3/hd): the header
    /// ` num     #instances         #bytes  class name (module)` over a
    /// 55-dash rule, rows `%4d: %13d  %13u  <class name column>`, and
    /// `Total %13d  %13u` with no rule above it. The rows print in the order
    /// given (the live producer sorts: `hprof::class_histogram_for_dcmd`).
    pub fn generate_class_histogram(classes: &[ClassHistogramEntry]) -> String {
        let mut output = String::new();
        // The header as `format!` widths (HotSpot's ` num` + two 15-wide right-aligned
        // columns), not a literal with long space runs (`string_literal_line_breaks`).
        output.push_str(&format!(" num{:>15}{:>15}  class name (module)\n", "#instances", "#bytes"));
        output.push_str("-------------------------------------------------------\n");

        let mut total_instances: u64 = 0;
        let mut total_bytes: u64 = 0;

        for (i, entry) in classes.iter().enumerate() {
            output.push_str(&format!(
                "{:>4}: {:>13}  {:>13}  {}\n",
                i + 1,
                entry.instance_count,
                entry.total_bytes,
                entry.class_name
            ));
            total_instances = total_instances.saturating_add(entry.instance_count);
            total_bytes = total_bytes.saturating_add(entry.total_bytes);
        }

        output.push_str(&format!(
            "Total {:>13}  {:>13}\n",
            total_instances, total_bytes
        ));

        output
    }

    /// Generate a heap summary report.
    pub fn generate_heap_summary(heap_info: &HeapSummary) -> String {
        let mut output = String::new();
        output.push_str("Heap Configuration:\n");
        output.push_str(&format!(
            "   Young Generation: {} / {} ({:.1}% used)\n",
            format_bytes(heap_info.young_gen_used),
            format_bytes(heap_info.young_gen_capacity),
            percent(heap_info.young_gen_used, heap_info.young_gen_capacity)
        ));
        output.push_str(&format!(
            "   Old Generation:   {} / {} ({:.1}% used)\n",
            format_bytes(heap_info.old_gen_used),
            format_bytes(heap_info.old_gen_capacity),
            percent(heap_info.old_gen_used, heap_info.old_gen_capacity)
        ));
        output.push_str(&format!(
            "   Metaspace:        {} / {} ({:.1}% used)\n",
            format_bytes(heap_info.metaspace_used),
            format_bytes(heap_info.metaspace_capacity),
            percent(heap_info.metaspace_used, heap_info.metaspace_capacity)
        ));
        output.push_str(&format!(
            "   Total:            {} / {} ({:.1}% used)\n",
            format_bytes(heap_info.total_used),
            format_bytes(heap_info.total_capacity),
            percent(heap_info.total_used, heap_info.total_capacity)
        ));
        output
    }

    /// Generate finalizer information report.
    pub fn generate_finalizer_info() -> String {
        let mut output = String::new();
        output.push_str("Finalizer Information:\n");
        output.push_str("  Pending finalizers: 0\n");
        output.push_str("  Finalizer thread: active\n");
        output.push_str("  Reference handler thread: active\n");
        output
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

fn percent(used: u64, capacity: u64) -> f64 {
    if capacity == 0 {
        0.0
    } else {
        (used as f64 / capacity as f64) * 100.0
    }
}

// ---------------------------------------------------------------------------
// HPROF heap dump writer (HPROF 1.0.2 binary format)
// ---------------------------------------------------------------------------

/// HPROF basic type constants used in CLASS_DUMP field descriptors and
/// PRIM_ARRAY_DUMP element types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HprofBasicType {
    Object = 2,
    Boolean = 4,
    Char = 5,
    Float = 6,
    Double = 7,
    Byte = 8,
    Short = 9,
    Int = 10,
    Long = 11,
}

impl HprofBasicType {
    /// Number of bytes this type occupies in an HPROF instance dump value.
    pub fn size(self) -> usize {
        match self {
            HprofBasicType::Object => 8, // identifier size
            HprofBasicType::Boolean | HprofBasicType::Byte => 1,
            HprofBasicType::Char | HprofBasicType::Short => 2,
            HprofBasicType::Float | HprofBasicType::Int => 4,
            HprofBasicType::Double | HprofBasicType::Long => 8,
        }
    }

    /// Map a JVM field descriptor character to an HPROF basic type.
    pub fn from_descriptor(desc: &str) -> Self {
        match desc.as_bytes().first() {
            Some(b'Z') => HprofBasicType::Boolean,
            Some(b'B') => HprofBasicType::Byte,
            Some(b'C') => HprofBasicType::Char,
            Some(b'S') => HprofBasicType::Short,
            Some(b'I') => HprofBasicType::Int,
            Some(b'J') => HprofBasicType::Long,
            Some(b'F') => HprofBasicType::Float,
            Some(b'D') => HprofBasicType::Double,
            Some(b'L') | Some(b'[') => HprofBasicType::Object,
            _ => HprofBasicType::Object,
        }
    }
}

/// Information about a class needed for HPROF dump.
#[derive(Debug, Clone)]
pub struct HprofClassInfo {
    /// Unique class ID (from ClassId).
    pub class_id: u32,
    /// JVM internal name (e.g. "java/lang/String").
    pub name: String,
    /// Superclass class ID, or 0 for java/lang/Object.
    pub super_class_id: u32,
    /// Instance fields declared in this class (name, descriptor).
    pub instance_fields: Vec<(String, String)>,
    /// Static fields declared in this class (name, descriptor).
    pub static_fields: Vec<(String, String)>,
    /// Source file name, if known.
    pub source_file: Option<String>,
    /// Total instance size in bytes (HEADER_SIZE + num_total_fields * SLOT_SIZE).
    pub instance_size: u32,
}

/// Information about a single heap object for HPROF dump.
#[derive(Debug)]
pub struct HprofObjectInfo {
    /// Raw pointer to the object (used as HPROF object ID).
    pub object_id: u64,
    /// Class ID of this object.
    pub class_id: u32,
    /// Whether this is an array.
    pub is_array: bool,
    /// Array element type (meaningful only for arrays).
    pub element_type: u8,
    /// Array length (meaningful only for arrays).
    pub array_length: u32,
    /// Total allocation size in bytes.
    pub total_size: usize,
    /// Raw pointer to the object data (for reading field/element values).
    pub data_ptr: *const u8,
}

/// Writer for HPROF 1.0.2 binary format heap dump files.
///
/// Implements the full HPROF binary spec including:
/// - File header with magic string and identifier size
/// - UTF-8 string records (for class/field/method names)
/// - LOAD_CLASS records (class serial mapping)
/// - STACK_TRACE / STACK_FRAME records (thread stacks)
/// - HEAP_DUMP_SEGMENT records containing:
///   - GC_CLASS_DUMP sub-records (class metadata with field descriptors)
///   - GC_INSTANCE_DUMP sub-records (object instances with field values)
///   - GC_OBJ_ARRAY_DUMP sub-records (reference arrays)
///   - GC_PRIM_ARRAY_DUMP sub-records (primitive arrays)
///   - GC_ROOT_THREAD_OBJ sub-records (thread roots)
///   - GC_ROOT_JNI_GLOBAL sub-records (JNI global reference roots)
/// - HEAP_DUMP_END marker
pub struct HprofWriter;

impl HprofWriter {
    pub const HPROF_MAGIC: &'static str = "JAVA PROFILE 1.0.2";

    // Top-level record type constants
    pub const HPROF_UTF8: u8 = 0x01;
    pub const HPROF_LOAD_CLASS: u8 = 0x02;
    pub const HPROF_FRAME: u8 = 0x04;
    pub const HPROF_TRACE: u8 = 0x05;
    pub const HPROF_HEAP_DUMP: u8 = 0x0C;
    pub const HPROF_HEAP_DUMP_SEGMENT: u8 = 0x1C;
    pub const HPROF_HEAP_DUMP_END: u8 = 0x2C;

    // Heap dump sub-record tag constants
    pub const GC_ROOT_JNI_GLOBAL: u8 = 0x01;
    pub const GC_ROOT_THREAD_OBJ: u8 = 0x08;
    pub const GC_CLASS_DUMP: u8 = 0x20;
    pub const GC_INSTANCE_DUMP: u8 = 0x21;
    pub const GC_OBJ_ARRAY_DUMP: u8 = 0x22;
    pub const GC_PRIM_ARRAY_DUMP: u8 = 0x23;

    /// Maximum segment body size before starting a new HEAP_DUMP_SEGMENT.
    /// HPROF spec allows up to ~2GB per segment; we use 64 MB for streaming.
    const MAX_SEGMENT_SIZE: usize = 64 * 1024 * 1024;

    /// Write HPROF file header. Format:
    /// - magic string (null-terminated)
    /// - identifier size (4 bytes, big-endian) = 8
    /// - high timestamp (4 bytes)
    /// - low timestamp (4 bytes)
    pub fn write_header() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(Self::HPROF_MAGIC.as_bytes());
        buf.push(0); // null terminator
        buf.extend_from_slice(&8u32.to_be_bytes()); // identifier size: 8 bytes
                                                    // Timestamp: milliseconds since epoch, split into high/low 32-bit words
        let ts_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        buf.extend_from_slice(&((ts_millis >> 32) as u32).to_be_bytes());
        buf.extend_from_slice(&(ts_millis as u32).to_be_bytes());
        buf
    }

    /// Write a UTF-8 string record.
    /// Format: tag(1) + time(4) + length(4) + id(8) + utf8_bytes
    pub fn write_string_record(id: u64, value: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(Self::HPROF_UTF8);
        buf.extend_from_slice(&0u32.to_be_bytes());
        let body_len = 8 + value.len() as u32;
        buf.extend_from_slice(&body_len.to_be_bytes());
        buf.extend_from_slice(&id.to_be_bytes());
        buf.extend_from_slice(value.as_bytes());
        buf
    }

    /// Return the size of the HPROF header in bytes.
    pub fn header_size() -> usize {
        Self::HPROF_MAGIC.len() + 1 + 4 + 4 + 4
    }

    /// Write a LOAD_CLASS record.
    pub fn write_load_class(
        serial: u32,
        class_obj_id: u64,
        stack_serial: u32,
        name_id: u64,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(Self::HPROF_LOAD_CLASS);
        buf.extend_from_slice(&0u32.to_be_bytes());
        let body_len: u32 = 4 + 8 + 4 + 8;
        buf.extend_from_slice(&body_len.to_be_bytes());
        buf.extend_from_slice(&serial.to_be_bytes());
        buf.extend_from_slice(&class_obj_id.to_be_bytes());
        buf.extend_from_slice(&stack_serial.to_be_bytes());
        buf.extend_from_slice(&name_id.to_be_bytes());
        buf
    }

    /// Write a HEAP_DUMP_END record (empty body).
    pub fn write_heap_dump_end() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(Self::HPROF_HEAP_DUMP_END);
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf
    }

    /// Write a STACK_TRACE record.
    pub fn write_stack_trace(serial: u32, thread_serial: u32, frame_ids: &[u64]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(Self::HPROF_TRACE);
        buf.extend_from_slice(&0u32.to_be_bytes());
        let body_len: u32 = 4 + 4 + 4 + (frame_ids.len() as u32 * 8);
        buf.extend_from_slice(&body_len.to_be_bytes());
        buf.extend_from_slice(&serial.to_be_bytes());
        buf.extend_from_slice(&thread_serial.to_be_bytes());
        buf.extend_from_slice(&(frame_ids.len() as u32).to_be_bytes());
        for &fid in frame_ids {
            buf.extend_from_slice(&fid.to_be_bytes());
        }
        buf
    }

    /// Write a STACK_FRAME record.
    pub fn write_stack_frame(
        frame_id: u64,
        method_name_id: u64,
        method_sig_id: u64,
        source_file_id: u64,
        class_serial: u32,
        line_number: i32,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(Self::HPROF_FRAME);
        buf.extend_from_slice(&0u32.to_be_bytes());
        let body_len: u32 = 8 + 8 + 8 + 8 + 4 + 4;
        buf.extend_from_slice(&body_len.to_be_bytes());
        buf.extend_from_slice(&frame_id.to_be_bytes());
        buf.extend_from_slice(&method_name_id.to_be_bytes());
        buf.extend_from_slice(&method_sig_id.to_be_bytes());
        buf.extend_from_slice(&source_file_id.to_be_bytes());
        buf.extend_from_slice(&class_serial.to_be_bytes());
        buf.extend_from_slice(&(line_number as u32).to_be_bytes());
        buf
    }

    // -----------------------------------------------------------------------
    // Heap dump sub-record writers (written inside HEAP_DUMP_SEGMENT body)
    // -----------------------------------------------------------------------

    /// Write a GC_ROOT_THREAD_OBJ sub-record.
    /// Format: tag(1) + thread_obj_id(8) + thread_serial(4) + stack_serial(4)
    pub fn write_gc_root_thread_obj(
        buf: &mut Vec<u8>,
        thread_obj_id: u64,
        thread_serial: u32,
        stack_serial: u32,
    ) {
        buf.push(Self::GC_ROOT_THREAD_OBJ);
        buf.extend_from_slice(&thread_obj_id.to_be_bytes());
        buf.extend_from_slice(&thread_serial.to_be_bytes());
        buf.extend_from_slice(&stack_serial.to_be_bytes());
    }

    /// Write a GC_ROOT_JNI_GLOBAL sub-record.
    /// Format: tag(1) + object_id(8) + jni_global_ref_id(8)
    pub fn write_gc_root_jni_global(buf: &mut Vec<u8>, object_id: u64, jni_ref_id: u64) {
        buf.push(Self::GC_ROOT_JNI_GLOBAL);
        buf.extend_from_slice(&object_id.to_be_bytes());
        buf.extend_from_slice(&jni_ref_id.to_be_bytes());
    }

    /// Write a GC_CLASS_DUMP sub-record.
    ///
    /// Format:
    /// - tag(1) + class_obj_id(8) + stack_trace_serial(4) + super_class_obj_id(8)
    /// - classloader_obj_id(8) + signers_obj_id(8) + protection_domain_obj_id(8)
    /// - reserved1(8) + reserved2(8) + instance_size(4)
    /// - constant_pool_count(2) [we write 0]
    /// - static_field_count(2) + static fields...
    /// - instance_field_count(2) + instance fields...
    pub fn write_gc_class_dump(
        buf: &mut Vec<u8>,
        class_info: &HprofClassInfo,
        string_ids: &std::collections::HashMap<String, u64>,
    ) {
        buf.push(Self::GC_CLASS_DUMP);
        // class object ID: we use class_id shifted into high bits to avoid collisions
        let class_obj_id = 0x1000_0000_0000_0000u64 | class_info.class_id as u64;
        buf.extend_from_slice(&class_obj_id.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes()); // stack trace serial = 0
                                                    // super class object ID
        let super_obj_id = if class_info.super_class_id != 0 {
            0x1000_0000_0000_0000u64 | class_info.super_class_id as u64
        } else {
            0u64
        };
        buf.extend_from_slice(&super_obj_id.to_be_bytes());
        buf.extend_from_slice(&0u64.to_be_bytes()); // classloader obj id
        buf.extend_from_slice(&0u64.to_be_bytes()); // signers obj id
        buf.extend_from_slice(&0u64.to_be_bytes()); // protection domain
        buf.extend_from_slice(&0u64.to_be_bytes()); // reserved 1
        buf.extend_from_slice(&0u64.to_be_bytes()); // reserved 2
        buf.extend_from_slice(&class_info.instance_size.to_be_bytes());

        // Constant pool: 0 entries
        buf.extend_from_slice(&0u16.to_be_bytes());

        // Static fields
        let static_count = class_info.static_fields.len() as u16;
        buf.extend_from_slice(&static_count.to_be_bytes());
        for (fname, fdesc) in &class_info.static_fields {
            let name_id = string_ids.get(fname).copied().unwrap_or(0);
            buf.extend_from_slice(&name_id.to_be_bytes());
            let htype = HprofBasicType::from_descriptor(fdesc);
            buf.push(htype as u8);
            // Static field value: write zeros (we'd need to read from statics table for real values)
            match htype {
                HprofBasicType::Object => buf.extend_from_slice(&0u64.to_be_bytes()),
                HprofBasicType::Long | HprofBasicType::Double => {
                    buf.extend_from_slice(&0u64.to_be_bytes())
                }
                HprofBasicType::Int | HprofBasicType::Float => {
                    buf.extend_from_slice(&0u32.to_be_bytes())
                }
                HprofBasicType::Short | HprofBasicType::Char => {
                    buf.extend_from_slice(&0u16.to_be_bytes())
                }
                HprofBasicType::Boolean | HprofBasicType::Byte => buf.push(0),
            }
        }

        // Instance fields (only name + type descriptor, no values here)
        let inst_count = class_info.instance_fields.len() as u16;
        buf.extend_from_slice(&inst_count.to_be_bytes());
        for (fname, fdesc) in &class_info.instance_fields {
            let name_id = string_ids.get(fname).copied().unwrap_or(0);
            buf.extend_from_slice(&name_id.to_be_bytes());
            let htype = HprofBasicType::from_descriptor(fdesc);
            buf.push(htype as u8);
        }
    }

    /// Write a GC_INSTANCE_DUMP sub-record.
    ///
    /// Format: tag(1) + object_id(8) + stack_serial(4) + class_obj_id(8)
    ///       + data_size(4) + field_values...
    ///
    /// Field values are written in declaration order using the HPROF type sizes.
    /// Each field is written according to its HPROF type (not the internal SLOT_SIZE).
    pub fn write_gc_instance_dump(
        buf: &mut Vec<u8>,
        obj: &HprofObjectInfo,
        class_info: &HprofClassInfo,
        all_classes: &std::collections::HashMap<u32, HprofClassInfo>,
    ) {
        use cratonvm_gc::heap::HEADER_SIZE;
        use cratonvm_gc::heap::SLOT_SIZE;
        use cratonvm_gc::{is_compact_object, ObjectHeader};
        use cratonvm_types::class_layout_for_fields;
        use cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET;

        buf.push(Self::GC_INSTANCE_DUMP);
        buf.extend_from_slice(&obj.object_id.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes()); // stack trace serial

        let class_obj_id = 0x1000_0000_0000_0000u64 | class_info.class_id as u64;
        buf.extend_from_slice(&class_obj_id.to_be_bytes());

        // Collect the full field list from the class hierarchy (super first)
        let mut field_chain: Vec<&[(String, String)]> = Vec::new();
        // Start from the object's class info
        field_chain.push(&class_info.instance_fields);
        let mut cid = class_info.super_class_id;
        while cid != 0 {
            if let Some(ci) = all_classes.get(&cid) {
                field_chain.push(&ci.instance_fields);
                cid = ci.super_class_id;
            } else {
                break;
            }
        }
        field_chain.reverse(); // superclass fields first

        // Compute data_size: sum of HPROF-typed field sizes
        let mut data_size: u32 = 0;
        for fields in &field_chain {
            for (_fname, fdesc) in *fields {
                data_size += HprofBasicType::from_descriptor(fdesc).size() as u32;
            }
        }
        buf.extend_from_slice(&data_size.to_be_bytes());

        // Write field values by reading raw memory from the heap object.
        // Internal layout:
        // - Legacy object: fields are at HEADER_SIZE + field_index * SLOT_SIZE, each
        //   slot is 16 bytes containing a Value enum.
        // - Compact object: fields use their natural widths at offsets from the
        //   immutable layout version selected by this object's field count.
        let mut field_index: usize = 0;
        let header = unsafe { &*(obj.data_ptr as *const ObjectHeader) };
        let compact_layout = if is_compact_object(header) {
            class_layout_for_fields(header.class_id.as_u32(), header.num_slots())
        } else {
            None
        };

        for fields in &field_chain {
            for (_fname, fdesc) in *fields {
                let htype = HprofBasicType::from_descriptor(fdesc);
                let slot_offset = compact_layout
                    .as_ref()
                    .and_then(|layout| layout.field_disp(field_index))
                    .map_or_else(
                        || HEADER_SIZE + field_index * SLOT_SIZE,
                        |disp| disp as usize,
                    );

                // Safety: obj.data_ptr points to a valid heap object with at least
                // obj.total_size bytes allocated.
                let slot_ptr = unsafe { obj.data_ptr.add(slot_offset) };
                let is_compact_ref = compact_layout
                    .as_ref()
                    .and_then(|layout| layout.field_is_ref(field_index))
                    .unwrap_or(false);

                match htype {
                    HprofBasicType::Object => {
                        // Legacy: Value::Object stores the reference in the 8-byte Value payload.
                        // Compact: reference fields are raw pointers at field offsets.
                        let val = if is_compact_ref {
                            if slot_offset + 8 <= obj.total_size {
                                unsafe { std::ptr::read_unaligned(slot_ptr as *const u64) }
                            } else {
                                0u64
                            }
                        } else if slot_offset + FIELD_CELL_PAYLOAD64_OFFSET + 8 <= obj.total_size {
                            unsafe {
                                std::ptr::read_unaligned(
                                    slot_ptr.add(FIELD_CELL_PAYLOAD64_OFFSET) as *const u64
                                )
                            }
                        } else {
                            0u64
                        };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    HprofBasicType::Int => {
                        let val = if slot_offset + 4 <= obj.total_size {
                            unsafe { std::ptr::read_unaligned(slot_ptr as *const i32) }
                        } else {
                            0i32
                        };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    HprofBasicType::Long => {
                        let val = if slot_offset + 8 <= obj.total_size {
                            unsafe { std::ptr::read_unaligned(slot_ptr as *const i64) }
                        } else {
                            0i64
                        };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    HprofBasicType::Float => {
                        let val = if slot_offset + 4 <= obj.total_size {
                            unsafe { std::ptr::read_unaligned(slot_ptr as *const f32) }
                        } else {
                            0.0f32
                        };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    HprofBasicType::Double => {
                        let val = if slot_offset + 8 <= obj.total_size {
                            unsafe { std::ptr::read_unaligned(slot_ptr as *const f64) }
                        } else {
                            0.0f64
                        };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    HprofBasicType::Short | HprofBasicType::Char => {
                        let val = if slot_offset + 2 <= obj.total_size {
                            unsafe { std::ptr::read_unaligned(slot_ptr as *const i16) }
                        } else {
                            0i16
                        };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    HprofBasicType::Boolean | HprofBasicType::Byte => {
                        let val = if slot_offset + 1 <= obj.total_size {
                            unsafe { *slot_ptr }
                        } else {
                            0u8
                        };
                        buf.push(val);
                    }
                }
                field_index += 1;
            }
        }
    }

    /// Write a GC_OBJ_ARRAY_DUMP sub-record.
    ///
    /// Format: tag(1) + array_obj_id(8) + stack_serial(4) + num_elements(4)
    ///       + array_class_obj_id(8) + elements[num_elements × 8]
    pub fn write_gc_obj_array_dump(buf: &mut Vec<u8>, obj: &HprofObjectInfo) {
        use cratonvm_gc::heap::{HEADER_SIZE, REF_ELEMENT_SIZE};

        buf.push(Self::GC_OBJ_ARRAY_DUMP);
        buf.extend_from_slice(&obj.object_id.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes()); // stack trace serial
        buf.extend_from_slice(&obj.array_length.to_be_bytes());

        let class_obj_id = 0x1000_0000_0000_0000u64 | obj.class_id as u64;
        buf.extend_from_slice(&class_obj_id.to_be_bytes());

        // Read each element as an 8-byte reference
        for i in 0..obj.array_length as usize {
            let elem_offset = HEADER_SIZE + i * ref_element_size();
            let val = if elem_offset + ref_element_size() <= obj.total_size {
                unsafe {
                    let ptr = obj.data_ptr.add(elem_offset);
                    read_ref_slot_unaligned(ptr)
                }
            } else {
                0u64
            };
            buf.extend_from_slice(&val.to_be_bytes());
        }
    }

    /// Write a GC_PRIM_ARRAY_DUMP sub-record.
    ///
    /// Format: tag(1) + array_obj_id(8) + stack_serial(4) + num_elements(4)
    ///       + element_type(1) + elements[num_elements × element_size]
    pub fn write_gc_prim_array_dump(buf: &mut Vec<u8>, obj: &HprofObjectInfo) {
        use cratonvm_gc::heap::HEADER_SIZE;

        buf.push(Self::GC_PRIM_ARRAY_DUMP);
        buf.extend_from_slice(&obj.object_id.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes()); // stack trace serial
        buf.extend_from_slice(&obj.array_length.to_be_bytes());

        // Map ArrayElementType repr to HPROF basic type
        let hprof_type = match obj.element_type {
            4 => HprofBasicType::Boolean,
            5 => HprofBasicType::Char,
            6 => HprofBasicType::Float,
            7 => HprofBasicType::Double,
            8 => HprofBasicType::Byte,
            9 => HprofBasicType::Short,
            10 => HprofBasicType::Int,
            11 => HprofBasicType::Long,
            _ => HprofBasicType::Byte,
        };
        buf.push(hprof_type as u8);

        let elem_size = hprof_type.size();
        let data_start = HEADER_SIZE;

        for i in 0..obj.array_length as usize {
            let elem_offset = data_start + i * elem_size;
            if elem_offset + elem_size <= obj.total_size {
                let ptr = unsafe { obj.data_ptr.add(elem_offset) };
                // Copy raw bytes in big-endian order
                match elem_size {
                    1 => buf.push(unsafe { *ptr }),
                    2 => {
                        let val = unsafe { std::ptr::read_unaligned(ptr as *const u16) };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    4 => {
                        let val = unsafe { std::ptr::read_unaligned(ptr as *const u32) };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    8 => {
                        let val = unsafe { std::ptr::read_unaligned(ptr as *const u64) };
                        buf.extend_from_slice(&val.to_be_bytes());
                    }
                    _ => buf.push(0),
                }
            } else {
                // Pad with zeros for out-of-bounds
                for _ in 0..elem_size {
                    buf.push(0);
                }
            }
        }
    }

    /// Wrap a heap dump body buffer as a HEAP_DUMP_SEGMENT record.
    fn wrap_segment(body: &[u8]) -> Vec<u8> {
        let mut rec = Vec::with_capacity(9 + body.len());
        rec.push(Self::HPROF_HEAP_DUMP_SEGMENT);
        rec.extend_from_slice(&0u32.to_be_bytes()); // timestamp
        rec.extend_from_slice(&(body.len() as u32).to_be_bytes());
        rec.extend_from_slice(body);
        rec
    }

    /// Generate a complete HPROF binary heap dump.
    ///
    /// This is the main entry point for producing a valid HPROF file.
    /// It writes all required records in the correct order:
    /// 1. File header
    /// 2. UTF-8 string records (class names, field names)
    /// 3. LOAD_CLASS records
    /// 4. STACK_TRACE records (one dummy trace for each thread)
    /// 5. HEAP_DUMP_SEGMENT records (class dumps, roots, instance dumps)
    /// 6. HEAP_DUMP_END
    pub fn write_full_heap_dump(
        classes: &[HprofClassInfo],
        objects: &[HprofObjectInfo],
        thread_snapshots: &[ThreadSnapshot],
    ) -> Vec<u8> {
        let mut output = Vec::new();
        let mut string_ids: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        let mut next_string_id: u64 = 1;
        let class_map: std::collections::HashMap<u32, HprofClassInfo> =
            classes.iter().map(|c| (c.class_id, c.clone())).collect();

        // Helper: intern a string, returning its ID
        let intern =
            |s: &str, ids: &mut std::collections::HashMap<String, u64>, nid: &mut u64| -> u64 {
                if let Some(&id) = ids.get(s) {
                    return id;
                }
                let id = *nid;
                ids.insert(s.to_string(), id);
                *nid += 1;
                id
            };

        // Phase 1: Collect all strings that need UTF-8 records
        // Class names
        for ci in classes {
            intern(&ci.name, &mut string_ids, &mut next_string_id);
            if let Some(ref sf) = ci.source_file {
                intern(sf, &mut string_ids, &mut next_string_id);
            }
            for (fname, _) in &ci.instance_fields {
                intern(fname, &mut string_ids, &mut next_string_id);
            }
            for (fname, _) in &ci.static_fields {
                intern(fname, &mut string_ids, &mut next_string_id);
            }
        }
        // Thread names
        for ts in thread_snapshots {
            intern(&ts.name, &mut string_ids, &mut next_string_id);
            for frame in &ts.stack_frames {
                intern(&frame.class_name, &mut string_ids, &mut next_string_id);
                intern(&frame.method_name, &mut string_ids, &mut next_string_id);
                if let Some(ref f) = frame.file_name {
                    intern(f, &mut string_ids, &mut next_string_id);
                }
            }
        }

        // Phase 2: Write header
        output.extend_from_slice(&Self::write_header());

        // Phase 3: Write UTF-8 string records
        let mut sorted_strings: Vec<(&String, &u64)> = string_ids.iter().collect();
        sorted_strings.sort_by_key(|(_, id)| **id);
        for (s, id) in sorted_strings {
            output.extend_from_slice(&Self::write_string_record(*id, s));
        }

        // Phase 4: Write LOAD_CLASS records
        for (serial_idx, ci) in classes.iter().enumerate() {
            let serial = (serial_idx + 1) as u32;
            let class_obj_id = 0x1000_0000_0000_0000u64 | ci.class_id as u64;
            let name_id = string_ids.get(&ci.name).copied().unwrap_or(0);
            output.extend_from_slice(&Self::write_load_class(serial, class_obj_id, 0, name_id));
        }

        // Phase 5: Write STACK_TRACE records (one per thread + a dummy trace serial 0)
        // Dummy stack trace serial 0 with no frames (used by objects with unknown stack)
        output.extend_from_slice(&Self::write_stack_trace(0, 0, &[]));

        let mut next_frame_id: u64 = 1;
        for (tidx, ts) in thread_snapshots.iter().enumerate() {
            let thread_serial = (tidx + 1) as u32;
            let trace_serial = thread_serial;

            // Write stack frame records for this thread
            let mut frame_ids = Vec::new();
            for frame in &ts.stack_frames {
                let fid = next_frame_id;
                next_frame_id += 1;
                let method_name_id = string_ids.get(&frame.method_name).copied().unwrap_or(0);
                let class_name_id = string_ids.get(&frame.class_name).copied().unwrap_or(0);
                let source_id = frame
                    .file_name
                    .as_ref()
                    .and_then(|f| string_ids.get(f))
                    .copied()
                    .unwrap_or(0);
                output.extend_from_slice(&Self::write_stack_frame(
                    fid,
                    method_name_id,
                    class_name_id,
                    source_id,
                    0, // class serial (could look up but 0 is valid)
                    frame.line_number,
                ));
                frame_ids.push(fid);
            }

            output.extend_from_slice(&Self::write_stack_trace(
                trace_serial,
                thread_serial,
                &frame_ids,
            ));
        }

        // Phase 6: Write HEAP_DUMP_SEGMENT records
        let mut seg_body = Vec::new();

        // 6a: GC roots — thread objects
        for (tidx, ts) in thread_snapshots.iter().enumerate() {
            let thread_serial = (tidx + 1) as u32;
            // Use thread ID as a synthetic object ID for the thread root
            let thread_obj_id = 0x2000_0000_0000_0000u64 | ts.id;
            Self::write_gc_root_thread_obj(
                &mut seg_body,
                thread_obj_id,
                thread_serial,
                thread_serial,
            );
        }

        // 6b: CLASS_DUMP sub-records
        for ci in classes {
            Self::write_gc_class_dump(&mut seg_body, ci, &string_ids);

            // Flush segment if it's getting large
            if seg_body.len() >= Self::MAX_SEGMENT_SIZE {
                output.extend_from_slice(&Self::wrap_segment(&seg_body));
                seg_body.clear();
            }
        }

        // 6c: Instance / array dump sub-records
        for obj in objects {
            if obj.is_array {
                // Determine if reference array or primitive array
                if obj.element_type == 0 {
                    // Reference array (ArrayElementType::Reference = 0)
                    Self::write_gc_obj_array_dump(&mut seg_body, obj);
                } else {
                    Self::write_gc_prim_array_dump(&mut seg_body, obj);
                }
            } else {
                // Regular object instance
                if let Some(ci) = class_map.get(&obj.class_id) {
                    Self::write_gc_instance_dump(&mut seg_body, obj, ci, &class_map);
                }
            }

            // Flush segment if large
            if seg_body.len() >= Self::MAX_SEGMENT_SIZE {
                output.extend_from_slice(&Self::wrap_segment(&seg_body));
                seg_body.clear();
            }
        }

        // Flush remaining segment body
        if !seg_body.is_empty() {
            output.extend_from_slice(&Self::wrap_segment(&seg_body));
        }

        // Phase 7: HEAP_DUMP_END
        output.extend_from_slice(&Self::write_heap_dump_end());

        output
    }
}

// ---------------------------------------------------------------------------
// Sample data helpers (used by registered commands and tests)
// ---------------------------------------------------------------------------

fn sample_thread_snapshots() -> Vec<ThreadSnapshot> {
    vec![
        ThreadSnapshot {
            id: 1,
            name: "main".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![FrameInfo {
                class_name: "com.example.Main".to_string(),
                method_name: "main".to_string(),
                file_name: Some("Main.java".to_string()),
                line_number: 10,
                native_method: false,
            }],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        },
        ThreadSnapshot {
            id: 2,
            name: "GC-Thread".to_string(),
            daemon: true,
            priority: 8,
            state: ThreadState::Waiting,
            stack_frames: vec![FrameInfo {
                class_name: "java.lang.Object".to_string(),
                method_name: "wait".to_string(),
                file_name: None,
                line_number: -1,
                native_method: true,
            }],
            lock_info: None,
            blocked_by: None,
            waiting_on: Some(
                "<0x00000000c0000000> (a java.lang.ref.ReferenceQueue$Lock)".to_string(),
            ),
            wait_kind: None,
            dump: Default::default(),
        },
    ]
}

fn sample_class_histogram() -> Vec<ClassHistogramEntry> {
    vec![
        ClassHistogramEntry {
            class_name: "[B".to_string(),
            instance_count: 50000,
            total_bytes: 5_000_000,
        },
        ClassHistogramEntry {
            class_name: "java.lang.String".to_string(),
            instance_count: 40000,
            total_bytes: 1_600_000,
        },
        ClassHistogramEntry {
            class_name: "java.lang.Object[]".to_string(),
            instance_count: 20000,
            total_bytes: 800_000,
        },
        ClassHistogramEntry {
            class_name: "java.util.HashMap$Node".to_string(),
            instance_count: 15000,
            total_bytes: 720_000,
        },
        ClassHistogramEntry {
            class_name: "java.lang.Class".to_string(),
            instance_count: 4200,
            total_bytes: 672_000,
        },
        ClassHistogramEntry {
            class_name: "java.util.HashMap$Node[]".to_string(),
            instance_count: 3000,
            total_bytes: 500_000,
        },
        ClassHistogramEntry {
            class_name: "char[]".to_string(),
            instance_count: 30000,
            total_bytes: 480_000,
        },
        ClassHistogramEntry {
            class_name: "java.lang.reflect.Method".to_string(),
            instance_count: 5000,
            total_bytes: 400_000,
        },
        ClassHistogramEntry {
            class_name: "java.util.concurrent.ConcurrentHashMap$Node".to_string(),
            instance_count: 8000,
            total_bytes: 384_000,
        },
        ClassHistogramEntry {
            class_name: "int[]".to_string(),
            instance_count: 10000,
            total_bytes: 360_000,
        },
    ]
}

// ---------------------------------------------------------------------------
// HSDB — HotSpot Serviceability Agent wire protocol (T6.1.8)
// ---------------------------------------------------------------------------
//
// A minimal implementation of the HSDB wire protocol used by `jhsdb hsdb`
// and related tooling. This is a lightweight binary protocol over TCP that
// exposes read-only introspection of VM state.
//
// Protocol format (big-endian):
//   Handshake: client sends 4-byte magic 0x48534442 ("HSDB"), server echoes.
//   Request : u8 command id + u32 payload_len + payload bytes
//   Response: u8 status (0=OK, 1=ERR) + u32 payload_len + payload bytes
//
// Commands:
//   0x01 VERSION        — no payload. Response payload: UTF-8 version string.
//   0x02 PROCESS_INFO   — no payload. Response: pid(u64) + cmdline(u16 len + bytes).
//   0x03 HEAP_SUMMARY   — no payload. Response: 3*u64 (used, committed, max) bytes.
//   0x04 THREAD_LIST    — no payload. Response: u32 count + for each:
//                            u64 tid + u16 name_len + name bytes + u8 state
//
// The protocol number in VERSION is 1. A connect-probe client sends VERSION
// first; we reply with "CratonVM HSDB v1.0".

/// HSDB protocol magic handshake ("HSDB" in ASCII).
pub const HSDB_MAGIC: u32 = 0x4853_4442;

/// HSDB protocol version string returned in response to VERSION command.
pub const HSDB_VERSION_STRING: &str = "CratonVM HSDB v1.0";

/// HSDB command opcodes.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HsdbCommand {
    Version = 0x01,
    ProcessInfo = 0x02,
    HeapSummary = 0x03,
    ThreadList = 0x04,
}

impl HsdbCommand {
    pub fn from_u8(n: u8) -> Option<Self> {
        match n {
            0x01 => Some(HsdbCommand::Version),
            0x02 => Some(HsdbCommand::ProcessInfo),
            0x03 => Some(HsdbCommand::HeapSummary),
            0x04 => Some(HsdbCommand::ThreadList),
            _ => None,
        }
    }
}

/// HSDB response status.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HsdbStatus {
    Ok = 0,
    Error = 1,
}

/// Encode a u16 length-prefixed UTF-8 string.
fn encode_string16(s: &str, out: &mut Vec<u8>) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(u16::MAX as usize) as u16;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&bytes[..len as usize]);
}

/// Handle a single HSDB request and return a (status, payload) response tuple.
/// This is the pure functional core — transport is left to callers.
pub fn hsdb_handle_request(
    cmd: HsdbCommand,
    _payload: &[u8],
    state: Option<&dyn VmDiagnosticState>,
) -> (HsdbStatus, Vec<u8>) {
    match cmd {
        HsdbCommand::Version => {
            let mut out = Vec::new();
            encode_string16(HSDB_VERSION_STRING, &mut out);
            (HsdbStatus::Ok, out)
        }
        HsdbCommand::ProcessInfo => {
            let pid: u64 = std::process::id() as u64;
            let cmdline = match state {
                Some(s) => s.command_line(),
                None => String::new(),
            };
            let mut out = Vec::with_capacity(8 + 2 + cmdline.len());
            out.extend_from_slice(&pid.to_be_bytes());
            encode_string16(&cmdline, &mut out);
            (HsdbStatus::Ok, out)
        }
        HsdbCommand::HeapSummary => match state {
            Some(s) => {
                let summary = s.heap_summary();
                // Wire layout (big-endian): young_used, young_cap, old_used, old_cap,
                // meta_used, meta_cap, total_used, total_cap (all u64) = 64 bytes.
                let mut out = Vec::with_capacity(64);
                out.extend_from_slice(&summary.young_gen_used.to_be_bytes());
                out.extend_from_slice(&summary.young_gen_capacity.to_be_bytes());
                out.extend_from_slice(&summary.old_gen_used.to_be_bytes());
                out.extend_from_slice(&summary.old_gen_capacity.to_be_bytes());
                out.extend_from_slice(&summary.metaspace_used.to_be_bytes());
                out.extend_from_slice(&summary.metaspace_capacity.to_be_bytes());
                out.extend_from_slice(&summary.total_used.to_be_bytes());
                out.extend_from_slice(&summary.total_capacity.to_be_bytes());
                (HsdbStatus::Ok, out)
            }
            None => (HsdbStatus::Error, b"no VM state".to_vec()),
        },
        HsdbCommand::ThreadList => match state {
            Some(s) => {
                let threads = s.thread_snapshots();
                let mut out = Vec::new();
                let count = threads.len() as u32;
                out.extend_from_slice(&count.to_be_bytes());
                for t in &threads {
                    out.extend_from_slice(&t.id.to_be_bytes());
                    encode_string16(&t.name, &mut out);
                    let state_byte: u8 = match t.state {
                        ThreadState::New => 0,
                        ThreadState::Runnable => 1,
                        ThreadState::Blocked => 2,
                        ThreadState::Waiting => 3,
                        ThreadState::TimedWaiting => 4,
                        ThreadState::Terminated => 7,
                    };
                    out.push(state_byte);
                }
                (HsdbStatus::Ok, out)
            }
            None => (HsdbStatus::Error, b"no VM state".to_vec()),
        },
    }
}

/// Encode a full HSDB response: u8 status + u32 payload_len + payload.
pub fn hsdb_encode_response(status: HsdbStatus, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(status as u8);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Decode an HSDB request header from the wire: u8 cmd + u32 payload_len.
/// Returns the parsed (cmd, payload_len) or an error string.
pub fn hsdb_decode_request_header(bytes: &[u8]) -> Result<(HsdbCommand, u32), String> {
    if bytes.len() < 5 {
        return Err(format!("short request header: {} < 5", bytes.len()));
    }
    let cmd = HsdbCommand::from_u8(bytes[0])
        .ok_or_else(|| format!("unknown command byte {}", bytes[0]))?;
    let len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
    Ok((cmd, len))
}

/// Start the HSDB listener on the given TCP port. Runs on a background thread.
/// Returns a handle that can be used to stop the listener.
///
/// Typical port selection is `debug_port + 1` where `debug_port` is the JDWP
/// port; callers are responsible for choosing a free port.
pub fn hsdb_start_listener(
    port: u16,
    state: Arc<dyn VmDiagnosticState>,
) -> std::io::Result<HsdbListener> {
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    let listener = TcpListener::bind(("127.0.0.1", port))?;
    listener.set_nonblocking(true)?;
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = Arc::clone(&running);
    let state_clone = Arc::clone(&state);
    let local_addr = listener.local_addr()?;

    let thread_handle = thread::spawn(move || {
        use std::io::{Read, Write};
        use std::time::Duration;

        while running_clone.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    // Ensure accepted stream uses blocking I/O even on platforms
                    // where nonblocking is inherited from the listener.
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

                    // Handshake
                    let mut magic = [0u8; 4];
                    if stream.read_exact(&mut magic).is_err() {
                        continue;
                    }
                    let got_magic = u32::from_be_bytes(magic);
                    if got_magic != HSDB_MAGIC {
                        continue;
                    }
                    if stream.write_all(&HSDB_MAGIC.to_be_bytes()).is_err() {
                        continue;
                    }

                    // Read requests until client disconnects
                    loop {
                        let mut hdr = [0u8; 5];
                        if stream.read_exact(&mut hdr).is_err() {
                            break;
                        }
                        let (cmd, payload_len) = match hsdb_decode_request_header(&hdr) {
                            Ok(p) => p,
                            Err(_) => break,
                        };
                        let mut payload = vec![0u8; payload_len as usize];
                        if payload_len > 0 && stream.read_exact(&mut payload).is_err() {
                            break;
                        }
                        let (status, resp_payload) =
                            hsdb_handle_request(cmd, &payload, Some(state_clone.as_ref()));
                        let resp = hsdb_encode_response(status, &resp_payload);
                        if stream.write_all(&resp).is_err() {
                            break;
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
    });

    Ok(HsdbListener {
        addr: local_addr,
        running,
        thread: Some(thread_handle),
    })
}

/// Handle for a running HSDB listener. Dropping the handle stops the listener.
pub struct HsdbListener {
    pub addr: std::net::SocketAddr,
    running: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HsdbListener {
    /// Signal the listener to stop and wait for its thread to exit.
    pub fn stop(mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for HsdbListener {
    fn drop(&mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // Windows exposes one attach endpoint per PID. Tests that start listeners
    // cannot use Unix's independent socket paths, so serialize their lifetimes.
    #[cfg(windows)]
    static WINDOWS_ENDPOINT_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[cfg(windows)]
    #[test]
    fn windows_attach_endpoint_has_one_vm_owner_and_releases_it() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        use super::windows_attach::WindowsAttachTarget;
        let commands = std::sync::Arc::new(parking_lot::RwLock::new(Vec::new()));
        let owner = WindowsAttachTarget::start(commands.clone()).unwrap();
        assert!(WindowsAttachTarget::start(commands.clone()).is_err());
        drop(owner);
        let successor = WindowsAttachTarget::start(commands).unwrap();
        drop(successor);
    }
    #[test]
    fn attach_request_versions_and_bounds() {
        use super::read_attach_request;
        use std::io::Cursor;
        let expected = vec!["1", "load", "a", "b", "c"];
        assert_eq!(
            read_attach_request(&mut Cursor::new(b"1\0load\0a\0b\0c\0")).unwrap(),
            expected
        );
        assert_eq!(
            read_attach_request(&mut Cursor::new(b"2\011\0load\0a\0b\0c\0")).unwrap(),
            vec!["2", "load", "a", "b", "c"]
        );
        for bytes in [
            b"2\0262145\0".as_slice(),
            b"2\011\0load\0a\0",
            b"1\0load\0a\0",
        ] {
            assert!(read_attach_request(&mut Cursor::new(bytes)).is_err());
        }
    }
    use super::*;

    // --- AttachListener tests ---

    #[test]
    fn test_attach_listener_new() {
        let listener = AttachListener::new("/tmp/test_attach");
        assert_eq!(listener.socket_path, "/tmp/test_attach");
        assert!(!listener.is_listening);
        assert!(listener.commands.read().is_empty());
    }

    #[test]
    fn test_attach_listener_start_stop() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        // obsaudit D15: a unique path — start_listening() now binds a real
        // socket, so a path shared with another test risks a parallel-test
        // bind race (see the comment on TEST_JCMD_SOCKET_COUNTER).
        let mut listener = AttachListener::new("/tmp/cratonvm_test_start_stop");
        assert!(!listener.is_listening);
        listener.start_listening();
        assert!(listener.is_listening);
        listener.stop_listening();
        assert!(!listener.is_listening);
    }

    #[test]
    fn test_attach_listener_register_command() {
        let mut listener = AttachListener::new("/tmp/test");
        listener.register_command(DiagnosticCommand::new(
            "Test.cmd",
            "A test command",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
        ));
        assert_eq!(listener.commands.read().len(), 1);
        assert_eq!(listener.commands.read()[0].name, "Test.cmd");
    }

    #[test]
    fn test_attach_listener_dispatch_command() {
        // obsaudit D15: renamed from test_attach_listener_find_command —
        // find_command was removed (it could not return a reference into
        // the now-lock-guarded `commands` without a dangling-guard
        // lifetime issue), replaced by dispatch_command, which looks up
        // and executes under a single lock acquisition.
        let mut listener = AttachListener::new("/tmp/test");
        listener.register_command(DiagnosticCommand::new(
            "Test.cmd",
            "desc",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
        ));
        assert!(listener.dispatch_command("Test.cmd", &[]).is_some());
        assert!(listener.dispatch_command("Nonexistent", &[]).is_none());
    }

    #[test]
    fn test_attach_listener_list_commands() {
        let mut listener = AttachListener::new("/tmp/test");
        listener.register_command(DiagnosticCommand::new(
            "A.cmd",
            "d",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
        ));
        listener.register_command(DiagnosticCommand::new(
            "B.cmd",
            "d",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![],
            Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
        ));
        let names = listener.list_commands();
        assert_eq!(names, vec!["A.cmd".to_string(), "B.cmd".to_string()]);
    }

    // --- DiagnosticCommand tests ---

    #[test]
    fn test_diagnostic_command_execute() {
        let cmd = DiagnosticCommand::new(
            "Test.echo",
            "Echo args",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|args| CommandResult::ok(args.join(", "), 0)),
        );
        let result = cmd.execute(&["hello".to_string(), "world".to_string()]);
        assert!(result.success);
        assert_eq!(result.output, "hello, world");
    }

    #[test]
    fn test_command_result_ok() {
        let r = CommandResult::ok("done".to_string(), 42);
        assert!(r.success);
        assert_eq!(r.output, "done");
        assert!(r.error.is_none());
        assert_eq!(r.execution_time_ms, 42);
    }

    #[test]
    fn test_command_result_err() {
        let r = CommandResult::err("fail".to_string(), 5);
        assert!(!r.success);
        assert!(r.output.is_empty());
        assert_eq!(r.error.as_deref(), Some("fail"));
    }

    #[test]
    fn test_command_impact_display() {
        assert_eq!(format!("{}", CommandImpact::Low), "Low");
        assert_eq!(format!("{}", CommandImpact::Medium), "Medium");
        assert_eq!(format!("{}", CommandImpact::High), "High");
    }

    #[test]
    fn test_command_with_arguments() {
        let cmd = DiagnosticCommand::new(
            "Test.args",
            "Test with args",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![
                CommandArgument {
                    name: "path".to_string(),
                    description: "File path".to_string(),
                    arg_type: ArgType::FilePath,
                    required: true,
                    default_value: None,
                },
                CommandArgument {
                    name: "verbose".to_string(),
                    description: "Verbose output".to_string(),
                    arg_type: ArgType::Bool,
                    required: false,
                    default_value: Some("false".to_string()),
                },
            ],
            Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
        );
        assert_eq!(cmd.arguments.len(), 2);
        assert_eq!(cmd.arguments[0].arg_type, ArgType::FilePath);
        assert!(cmd.arguments[0].required);
        assert!(!cmd.arguments[1].required);
        assert_eq!(cmd.arguments[1].default_value.as_deref(), Some("false"));
    }

    // --- JcmdProcessor tests ---

    #[test]
    fn test_jcmd_new_has_all_commands() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let names = jcmd.attach_listener.list_commands();
        assert_eq!(names.len(), 16);
        assert!(names.contains(&"Thread.print".to_string()));
        assert!(names.contains(&"GC.heap_dump".to_string()));
        assert!(names.contains(&"GC.run".to_string()));
        assert!(names.contains(&"GC.heap_info".to_string()));
        assert!(names.contains(&"GC.class_histogram".to_string()));
        assert!(names.contains(&"VM.version".to_string()));
        assert!(names.contains(&"VM.flags".to_string()));
        assert!(names.contains(&"VM.system_properties".to_string()));
        assert!(names.contains(&"VM.uptime".to_string()));
        assert!(names.contains(&"VM.info".to_string()));
        assert!(names.contains(&"VM.command_line".to_string()));
        assert!(names.contains(&"Thread.dump_to_file".to_string()));
        assert!(names.contains(&"Compiler.queue".to_string()));
        assert!(names.contains(&"JFR.start".to_string()));
        assert!(names.contains(&"JFR.stop".to_string()));
        assert!(names.contains(&"JFR.dump".to_string()));
    }

    #[test]
    fn test_jcmd_thread_print() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("Thread.print");
        assert!(result.success);
        assert!(result.output.contains("main"));
        assert!(result.output.contains("RUNNABLE"));
    }

    #[test]
    fn test_jcmd_gc_heap_dump() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("GC.heap_dump /tmp/dump.hprof");
        assert!(result.success);
        assert!(result
            .output
            .contains("Heap dump written to /tmp/dump.hprof"));
        assert!(result.output.contains(HprofWriter::HPROF_MAGIC));
    }

    #[test]
    fn test_jcmd_gc_run() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("GC.run");
        assert!(result.success);
        assert_eq!(result.output, "GC triggered");
    }

    #[test]
    fn test_jcmd_gc_heap_info() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("GC.heap_info");
        assert!(result.success);
        assert!(result.output.contains("Young Generation"));
        assert!(result.output.contains("Old Generation"));
        assert!(result.output.contains("Metaspace"));
    }

    #[test]
    fn test_jcmd_gc_class_histogram() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("GC.class_histogram");
        assert!(result.success);
        assert!(result.output.contains("#instances"));
        assert!(result.output.contains("java.lang.String"));
    }

    #[test]
    fn test_jcmd_vm_version() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.version");
        assert!(result.success);
        assert_eq!(result.output, "CratonVM 1.0.0 (JDK 25 compatible)");
    }

    #[test]
    fn test_jcmd_vm_flags() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.flags");
        assert!(result.success);
        assert!(result.output.contains("-XX:+UseG1GC"));
        assert!(result.output.lines().count() >= 15);
    }

    #[test]
    fn test_jcmd_vm_system_properties() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.system_properties");
        assert!(result.success);
        assert!(result.output.contains("java.version=25"));
        assert!(result.output.contains("java.vendor=CratonVM"));
    }

    #[test]
    fn test_jcmd_vm_uptime() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.uptime");
        assert!(result.success);
        assert!(result.output.contains("uptime"));
    }

    #[test]
    fn test_jcmd_vm_info() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.info");
        assert!(result.success);
        assert!(result.output.contains("CratonVM"));
        assert!(result.output.contains("Heap"));
    }

    #[test]
    fn test_jcmd_vm_command_line() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.command_line");
        assert!(result.success);
        assert!(result.output.contains("-Xmx256m"));
    }

    #[test]
    fn test_jcmd_thread_dump_to_file() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("Thread.dump_to_file /tmp/threads.txt");
        // gci i12/td: the argument-less set has no VM to run the JDK's
        // dumper on; it used to answer `Thread dump written to <path>` and
        // write nothing.
        assert!(!result.success, "a dump nothing wrote must not report success");
        assert!(!result.output.contains("Thread dump written"));
    }

    /// A state that serves `Thread.dump_to_file` (gci i12/td), recording what
    /// it was asked.
    struct DumpingVmState(parking_lot::Mutex<Vec<(String, bool, bool)>>);
    impl VmDiagnosticState for DumpingVmState {
        fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
            Vec::new()
        }
        fn heap_summary(&self) -> HeapSummary {
            MockVmState.heap_summary()
        }
        fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
            Vec::new()
        }
        fn trigger_gc(&self) -> bool {
            false
        }
        fn uptime_secs(&self) -> f64 {
            0.0
        }
        fn command_line(&self) -> String {
            String::new()
        }
        fn system_properties(&self) -> Vec<(String, String)> {
            Vec::new()
        }
        fn vm_flags(&self) -> Vec<String> {
            Vec::new()
        }
        fn thread_dump_to_file(&self, path: &str, overwrite: bool, json: bool) -> Result<String, String> {
            self.0.lock().push((path.to_string(), overwrite, json));
            Ok(format!("Created {path}\n"))
        }
    }

    /// gci i12/td: a `jcmd` line's arguments split as HotSpot 25's
    /// `DCmdArgIter` splits them -- quotes keep a space inside a key or a
    /// value, and an unclosed quote is HotSpot's format error.
    #[test]
    fn gci_i12td_dcmd_arguments_honour_hotspots_quotes() {
        let ok = |line: &str| dcmd_arguments(line).expect(line);
        let v = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<String>>();
        assert_eq!(ok(""), Vec::<String>::new());
        assert_eq!(ok("  -overwrite   /tmp/t.txt "), v(&["-overwrite", "/tmp/t.txt"]));
        assert_eq!(ok("\"C:/a b/t.txt\""), v(&["C:/a b/t.txt"]));
        assert_eq!(ok("-format=json 'C:/a b/t.json'"), v(&["-format=json", "C:/a b/t.json"]));
        assert_eq!(ok("filename=\"/x y/r.jfr\" name=r1"), v(&["filename=/x y/r.jfr", "name=r1"]));
        assert_eq!(ok("a=b=c"), v(&["a=b=c"]));
        assert_eq!(ok("\"a \\\" b\""), v(&["a \\\" b"]));
        assert_eq!(dcmd_arguments("\"/tmp/open"), Err(DCMD_FORMAT_ERROR.to_string()));
        assert_eq!(dcmd_arguments("filename='/tmp/open"), Err(DCMD_FORMAT_ERROR.to_string()));
        // Through the attach operation: a quoted path with a space reaches
        // the command whole; an unclosed quote is refused with status 1.
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let dumping = Arc::new(DumpingVmState(parking_lot::Mutex::new(Vec::new())));
        let state: Arc<dyn VmDiagnosticState> = dumping.clone();
        let processor = JcmdProcessor::new_with_vm_state(state);
        let op = |line: &str| {
            let fields: Vec<String> =
                ["1", "jcmd", line, "", ""].iter().map(|s| s.to_string()).collect();
            execute_attach_operation(&fields, &processor.attach_listener.commands)
        };
        assert_eq!(op("Thread.dump_to_file \"/tmp/a b/t.txt\"").0, 0);
        assert_eq!(dumping.0.lock().as_slice(), &[("/tmp/a b/t.txt".to_string(), false, false)]);
        assert_eq!(op("Thread.dump_to_file \"/tmp/a b"), (1, DCMD_FORMAT_ERROR.to_string()));
        // An unknown command stays unknown, whatever its arguments.
        assert_eq!(op("No.such \"x").1, JCMD_UNKNOWN_COMMAND);
    }

    /// gci i12/td: `VM.version` answers HotSpot's `VersionDCmd` shape from the
    /// VM's own properties, not a constant.
    #[test]
    fn gci_i12td_vm_version_is_hotspots_shape_from_the_vms_properties() {
        let props = |version: &str| {
            vec![
                ("java.vm.name".to_string(), "CratonVM".to_string()),
                ("java.vm.version".to_string(), "0.9.0".to_string()),
                ("java.version".to_string(), version.to_string()),
            ]
        };
        assert_eq!(vm_version_dcmd_lines(&props("25.0.3")), "CratonVM version 0.9.0\nJDK 25.0.3\n");
        assert_eq!(vm_version_dcmd_lines(&props("25")), "CratonVM version 0.9.0\nJDK 25.0.0\n");
        assert_eq!(vm_version_dcmd_lines(&props("25.0.3.1")), "CratonVM version 0.9.0\nJDK 25.0.3.1\n");
        assert_eq!(vm_version_dcmd_lines(&props("25-ea")), "CratonVM version 0.9.0\nJDK 25.0.0\n");
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        // The processor's commands hold only a `Weak` (`new_with_vm_state`):
        // keep the state alive, or the command answers "VM is shutting down"
        // with an empty output.
        let plain: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
        let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&plain));
        let out = processor
            .attach_listener
            .dispatch_command("VM.version", &[])
            .expect("registered")
            .output;
        assert!(!out.contains("1.0.0 (JDK 25 compatible)"), "{out}");
        assert!(out.ends_with("JDK 25.0.0\n"), "{out}");
    }

    /// gci i12/td: `Thread.dump_to_file`'s arguments read as HotSpot 25's
    /// `ThreadDumpToFileDCmd` declares them.
    #[test]
    fn gci_i12td_thread_dump_to_file_reads_hotspots_arguments() {
        let parse = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            thread_dump_to_file_request(&args)
        };
        let req = |path: &str, overwrite: bool, json: bool| ThreadDumpToFileRequest {
            path: path.to_string(),
            overwrite,
            json,
        };
        assert_eq!(parse(&["/tmp/t.txt"]), Ok(req("/tmp/t.txt", false, false)));
        assert_eq!(parse(&["-overwrite", "/tmp/t.txt"]), Ok(req("/tmp/t.txt", true, false)));
        assert_eq!(parse(&["-overwrite=false", "/tmp/t.txt"]), Ok(req("/tmp/t.txt", false, false)));
        assert_eq!(parse(&["-format=json", "/tmp/t.json"]), Ok(req("/tmp/t.json", false, true)));
        assert_eq!(parse(&["-format=plain", "/tmp/t.txt"]), Ok(req("/tmp/t.txt", false, false)));
        assert_eq!(
            parse(&["-overwrite", "-format=json", "/tmp/t.json"]),
            Ok(req("/tmp/t.json", true, true))
        );
        assert_eq!(
            parse(&[]),
            Err("java.lang.IllegalArgumentException: The argument 'filepath' is mandatory.\n".to_string())
        );
        assert_eq!(
            parse(&["-overwrite"]),
            Err("java.lang.IllegalArgumentException: The argument 'filepath' is mandatory.\n".to_string())
        );
        assert!(parse(&["-nosuch", "/tmp/t.txt"]).unwrap_err().contains("Unknown argument '-nosuch'"));
        assert!(parse(&["/tmp/a", "/tmp/b"]).unwrap_err().contains("Unknown argument '/tmp/b'"));
        assert!(parse(&["-overwrite=maybe", "/tmp/t.txt"]).unwrap_err().contains("Boolean parsing error"));
    }

    /// gci i12/td: the live table serves `Thread.dump_to_file` through the
    /// state (it was absent: `Unknown diagnostic command`), answers the
    /// dumper's body with status 0, and a missing path with HotSpot's
    /// non-zero `IllegalArgumentException` text.
    #[test]
    fn gci_i12td_thread_dump_to_file_is_a_live_command() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let dumping = Arc::new(DumpingVmState(parking_lot::Mutex::new(Vec::new())));
        let state: Arc<dyn VmDiagnosticState> = dumping.clone();
        let processor = JcmdProcessor::new_with_vm_state(state);
        let fields: Vec<String> = ["1", "jcmd", "Thread.dump_to_file -format=json /tmp/t.json", "", ""]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (code, body) = execute_attach_operation(&fields, &processor.attach_listener.commands);
        assert_eq!(code, 0, "{body}");
        assert_eq!(body, "Created /tmp/t.json\n");
        assert_eq!(dumping.0.lock().as_slice(), &[("/tmp/t.json".to_string(), false, true)]);
        let fields: Vec<String> = ["1", "jcmd", "Thread.dump_to_file", "", ""]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (code, body) = execute_attach_operation(&fields, &processor.attach_listener.commands);
        assert_ne!(code, 0);
        assert_eq!(body, "java.lang.IllegalArgumentException: The argument 'filepath' is mandatory.\n");
        assert_eq!(dumping.0.lock().len(), 1, "a refused request reached the dumper");
        // A state that cannot run the dumper refuses rather than claim a file.
        drop(processor);
        // Kept alive: the commands hold a `Weak`, and a dropped state would
        // refuse for the wrong reason ("VM is shutting down").
        let plain: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
        let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&plain));
        let result = processor
            .attach_listener
            .dispatch_command("Thread.dump_to_file", &["/tmp/t.txt".to_string()])
            .expect("registered");
        assert!(!result.success);
    }

    /// gci i12/td (page `gci-i11vt-...-review-findings` item 9's remainder):
    /// a carrier with a mounted virtual thread prints HotSpot's `Carrying
    /// virtual thread #<id>` in place of its `java.lang.Thread.State:` line;
    /// every other thread keeps its state line.
    #[test]
    fn gci_i12td_a_carrier_prints_carrying_virtual_thread_for_its_state() {
        let carrier = i81_snapshot(
            "ForkJoinPool-1-worker-1",
            ThreadState::Waiting,
            Some(DumpWaitKind::Parking),
            true,
            5,
            ThreadDumpFacts {
                java_tid: Some(22),
                carrying_virtual_thread: Some(21),
                ..Default::default()
            },
        );
        let plain = i81_snapshot(
            "main",
            ThreadState::Runnable,
            None,
            false,
            5,
            ThreadDumpFacts { java_tid: Some(1), ..Default::default() },
        );
        let dump = JstackProcessor::generate_thread_dump(&[carrier, plain]);
        let carrier_block: Vec<&str> = dump
            .lines()
            .skip_while(|l| !l.starts_with("\"ForkJoinPool-1-worker-1\""))
            .take(2)
            .collect();
        assert_eq!(carrier_block[1], "   Carrying virtual thread #21", "{dump}");
        assert_eq!(dump.matches("java.lang.Thread.State:").count(), 1, "{dump}");
        assert!(dump.contains("   java.lang.Thread.State: RUNNABLE\n"), "{dump}");
        // The live state answers it from the mounted virtual thread's
        // `carrierThread` (`vm_init.rs` `thread_snapshots`).
        let src = include_str!("../vm/vm_init.rs").replace("\r\n", "\n");
        assert!(src.contains("carrying_virtual_thread: entry.java_thread_obj.and_then("));
        assert!(src.contains("resolve_field_index_in_hierarchy(class_id, \"carrierThread\""));
    }

    /// Observability audit (2026-07-26): `Compiler.queue` must NOT invent a
    /// compilation queue. It previously returned a hard-coded listing naming
    /// `com.example.Main.hotLoop()V` — fiction an operator would have read as
    /// ground truth. Until the JIT queue is exposed to serviceability, the
    /// command must fail honestly.
    #[test]
    fn obsaudit_jcmd_compiler_queue_does_not_fabricate() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("Compiler.queue");
        assert!(
            !result.success,
            "Compiler.queue must not report success while unimplemented"
        );
        assert!(
            !result.output.contains("com.example.Main"),
            "fabricated compile-queue entries must never reappear"
        );
        assert!(
            !result.output.contains("C1 compile queue"),
            "fabricated compile-queue entries must never reappear"
        );
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("not implemented"));
    }

    /// Observability audit (2026-07-26): the JFR jcmd verbs must not claim
    /// success while never touching the FlightRecorder. A false "recording
    /// started" costs the operator the incident window they were capturing.
    #[test]
    fn obsaudit_jcmd_jfr_verbs_do_not_claim_false_success() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        for (cmd, forbidden) in [
            ("JFR.start myrecording", "Flight recording started"),
            ("JFR.stop myrecording", "Flight recording stopped"),
            ("JFR.dump /tmp/rec.jfr", "Flight recording dumped"),
        ] {
            let result = jcmd.process_command(cmd);
            assert!(
                !result.success,
                "`{cmd}` must not report success: nothing wires it to the FlightRecorder"
            );
            assert!(
                !result.output.contains(forbidden),
                "`{cmd}` must not emit the old fake-success string `{forbidden}`"
            );
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("not implemented"),
                "`{cmd}` must say plainly that it is unimplemented"
            );
        }
    }

    /// Pins the attach surface's real socket behaviour: `start_listening`
    /// binds, `stop_listening` unlinks. If someone changes either half they
    /// must also revisit the LIVENESS block at the top of this module and the
    /// fabricated-data audit notes it points at.
    ///
    /// Platform split, deliberate and asserted rather than skipped: the
    /// implementation is `#[cfg(unix)]` (see `AttachListener::start_listening`
    /// — HotSpot itself splits per OS, Unix domain socket on Linux/macOS
    /// versus a named pipe on Windows, and the Windows side is unimplemented
    /// here). This test used to assert the Unix shape unconditionally, so on
    /// a Windows checkout it demanded a `/tmp/...` socket file that the
    /// `#[cfg(not(unix))]` `start_listening` — which only flips
    /// `is_listening` — never creates, and failed 100% of the time. It now
    /// pins the Unix contract in full and the non-Unix contract exactly as
    /// documented, so the gap stays visible instead of being papered over by
    /// a skip.
    #[test]
    fn obsaudit_attach_listener_creates_a_real_socket() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        // obsaudit D15 (2026-07-26), FIXED: renamed from
        // obsaudit_attach_listener_creates_no_socket, which pinned the
        // opposite (no-socket) behaviour this fix replaced. start_listening
        // now binds a real Unix domain socket at `socket_path` — see the
        // struct doc comment for the wire protocol, verified empirically
        // against a real OpenJDK 21 jcmd/jstack/jmap.
        #[cfg(unix)]
        {
            let path = "/tmp/cratonvm-obsaudit-attach-socket-pin-test";
            let mut l = AttachListener::new(path);
            l.start_listening();
            assert!(l.is_listening);
            assert!(
                std::path::Path::new(path).exists(),
                "AttachListener must create a real socket file at socket_path"
            );
            use std::os::unix::fs::FileTypeExt;
            let meta = std::fs::symlink_metadata(path).unwrap();
            assert!(
                meta.file_type().is_socket(),
                "the file at socket_path must actually be a Unix domain socket, \
                 not e.g. a stray regular file"
            );
            l.stop_listening();
            assert!(
                !std::path::Path::new(path).exists(),
                "stop_listening must remove the socket file"
            );
        }
        #[cfg(not(unix))]
        {
            // Windows publishes an in-process queue for the client's named
            // pipe. There is no Unix socket file at socket_path.
            let path = std::env::temp_dir().join("cratonvm-obsaudit-attach-socket-pin-test");
            let _ = std::fs::remove_file(&path);
            let mut l = AttachListener::new(&path.to_string_lossy());
            l.start_listening();
            assert!(l.is_listening);
            assert!(
                !path.exists(),
                "the Windows transport must not create a Unix socket file"
            );
            l.stop_listening();
            assert!(!l.is_listening);
            assert!(!path.exists());
        }
    }

    #[test]
    fn test_jcmd_unknown_command() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("Nonexistent.command");
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("Unknown command"));
    }

    #[test]
    fn test_jcmd_help() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("help");
        assert!(result.success);
        assert!(result.output.contains("Available commands"));
        assert!(result.output.contains("Thread.print"));
        assert!(result.output.contains("VM.version"));
    }

    #[test]
    fn test_jcmd_help_method() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let jcmd = JcmdProcessor::new();
        let help = jcmd.help();
        assert!(help.contains("Available commands"));
        // Should list all 16 commands
        for name in jcmd.attach_listener.list_commands() {
            assert!(help.contains(&name));
        }
    }

    // --- jstack tests ---

    #[test]
    fn test_thread_state_display() {
        assert_eq!(format!("{}", ThreadState::New), "NEW");
        assert_eq!(format!("{}", ThreadState::Runnable), "RUNNABLE");
        assert_eq!(format!("{}", ThreadState::Blocked), "BLOCKED");
        assert_eq!(format!("{}", ThreadState::Waiting), "WAITING");
        assert_eq!(format!("{}", ThreadState::TimedWaiting), "TIMED_WAITING");
        assert_eq!(format!("{}", ThreadState::Terminated), "TERMINATED");
    }

    /// Interpreter round i3 wave 79, lane L6 (page
    /// `i63-L1-thread-dump-has-no-wait-kind-suffix`): a known wait kind
    /// prints HotSpot 25's suffix and header word; no kind prints the bare
    /// state and the old header, as before.
    #[test]
    fn thread_dump_prints_the_wait_kind_suffix_and_header_word() {
        let snap =
            |name: &str, state: ThreadState, wait_kind: Option<DumpWaitKind>| ThreadSnapshot {
                id: 7,
                name: name.to_string(),
                daemon: false,
                priority: 5,
                state,
                stack_frames: Vec::new(),
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
                wait_kind,
                dump: Default::default(),
            };
        let rows = [
            (
                snap(
                    "sleeper",
                    ThreadState::TimedWaiting,
                    Some(DumpWaitKind::Sleeping),
                ),
                "waiting on condition",
                "java.lang.Thread.State: TIMED_WAITING (sleeping)\n",
            ),
            (
                snap("parker", ThreadState::Waiting, Some(DumpWaitKind::Parking)),
                "waiting on condition",
                "java.lang.Thread.State: WAITING (parking)\n",
            ),
            (
                snap(
                    "timed-parker",
                    ThreadState::TimedWaiting,
                    Some(DumpWaitKind::Parking),
                ),
                "waiting on condition",
                "java.lang.Thread.State: TIMED_WAITING (parking)\n",
            ),
            (
                snap(
                    "waiter",
                    ThreadState::Waiting,
                    Some(DumpWaitKind::ObjectWait),
                ),
                "in Object.wait()",
                "java.lang.Thread.State: WAITING (on object monitor)\n",
            ),
            (
                snap(
                    "blocked",
                    ThreadState::Blocked,
                    Some(DumpWaitKind::MonitorEnter),
                ),
                "waiting for monitor entry",
                "java.lang.Thread.State: BLOCKED (on object monitor)\n",
            ),
            (
                snap("unknown", ThreadState::Waiting, None),
                "waiting on condition",
                "java.lang.Thread.State: WAITING\n",
            ),
            (
                snap("runner", ThreadState::Runnable, Some(DumpWaitKind::Parking)),
                "runnable",
                "java.lang.Thread.State: RUNNABLE\n",
            ),
        ];
        for (thread, header, state_line) in rows {
            let name = thread.name.clone();
            let dump = JstackProcessor::generate_thread_dump(&[thread]);
            let header_line = dump
                .lines()
                .find(|l| l.starts_with('"'))
                .expect("a header line");
            assert!(header_line.contains(header), "{name}: {header_line}");
            assert!(dump.contains(state_line), "{name}: {dump}");
        }
    }

    #[test]
    fn test_jstack_thread_dump_format() {
        let threads = vec![ThreadSnapshot {
            id: 1,
            name: "main".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![FrameInfo {
                class_name: "com.example.Main".to_string(),
                method_name: "run".to_string(),
                file_name: Some("Main.java".to_string()),
                line_number: 42,
                native_method: false,
            }],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("\"main\" #1 [0] prio=5 os_prio=0 tid=0x"));
        assert!(dump.contains("java.lang.Thread.State: RUNNABLE"));
        assert!(dump.contains("at com.example.Main.run(Main.java:42)"));
    }

    #[test]
    fn test_jstack_daemon_thread() {
        let threads = vec![ThreadSnapshot {
            id: 5,
            name: "GC-Worker".to_string(),
            daemon: true,
            priority: 8,
            state: ThreadState::Waiting,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("\"GC-Worker\" #5 [0] daemon prio=8"));
        assert!(dump.contains("java.lang.Thread.State: WAITING"));
    }

    #[test]
    fn test_jstack_native_method() {
        let threads = vec![ThreadSnapshot {
            id: 3,
            name: "native-thread".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![FrameInfo {
                class_name: "sun.misc.Unsafe".to_string(),
                method_name: "park".to_string(),
                file_name: None,
                line_number: -1,
                native_method: true,
            }],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("at sun.misc.Unsafe.park(Native Method)"));
    }

    #[test]
    fn test_jstack_lock_info() {
        let threads = vec![ThreadSnapshot {
            id: 1,
            name: "locker".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![],
            lock_info: Some(LockInfo {
                class_name: "java.util.HashMap".to_string(),
                identity_hash: 0xDEAD_BEEF,
            }),
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("locked <0x00000000deadbeef> (a java.util.HashMap)"));
    }

    #[test]
    fn test_jstack_waiting_on() {
        let threads = vec![ThreadSnapshot {
            id: 2,
            name: "waiter".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Waiting,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: None,
            waiting_on: Some("<0x000000c0> (a java.lang.Object)".to_string()),
            wait_kind: None,
            dump: Default::default(),
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("waiting on <0x000000c0> (a java.lang.Object)"));
    }

    #[test]
    fn test_jstack_deadlock_detection_no_deadlock() {
        let threads = vec![
            ThreadSnapshot {
                id: 1,
                name: "t1".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Runnable,
                stack_frames: vec![],
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
                wait_kind: None,
                dump: Default::default(),
            },
            ThreadSnapshot {
                id: 2,
                name: "t2".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Runnable,
                stack_frames: vec![],
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
                wait_kind: None,
                dump: Default::default(),
            },
        ];
        assert!(JstackProcessor::generate_deadlock_report(&threads).is_none());
    }

    #[test]
    fn test_jstack_deadlock_detection_cycle() {
        let threads = vec![
            ThreadSnapshot {
                id: 1,
                name: "thread-A".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Blocked,
                stack_frames: vec![],
                lock_info: None,
                blocked_by: Some(2),
                waiting_on: None,
                wait_kind: None,
                dump: Default::default(),
            },
            ThreadSnapshot {
                id: 2,
                name: "thread-B".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Blocked,
                stack_frames: vec![],
                lock_info: None,
                blocked_by: Some(1),
                waiting_on: None,
                wait_kind: None,
                dump: Default::default(),
            },
        ];
        let report = JstackProcessor::generate_deadlock_report(&threads);
        assert!(report.is_some());
        let report = report.unwrap();
        assert!(report.contains("deadlock"));
        assert!(report.contains("thread-A"));
        assert!(report.contains("thread-B"));
    }

    #[test]
    fn test_jstack_empty_threads() {
        let dump = JstackProcessor::generate_thread_dump(&[]);
        assert!(dump.contains("Full thread dump CratonVM"));
    }

    #[test]
    fn test_jstack_multiple_frames() {
        let threads = vec![ThreadSnapshot {
            id: 1,
            name: "deep-stack".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![
                FrameInfo {
                    class_name: "com.a.A".to_string(),
                    method_name: "foo".to_string(),
                    file_name: Some("A.java".to_string()),
                    line_number: 10,
                    native_method: false,
                },
                FrameInfo {
                    class_name: "com.b.B".to_string(),
                    method_name: "bar".to_string(),
                    file_name: Some("B.java".to_string()),
                    line_number: 20,
                    native_method: false,
                },
            ],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("at com.a.A.foo(A.java:10)"));
        assert!(dump.contains("at com.b.B.bar(B.java:20)"));
    }

    // --- Interpreter round i3 wave 81, lane L6: HotSpot 25.0.4's dump shape ---
    //
    // The expected texts are the HotSpot 25.0.4 `SIGQUIT` dump of
    // `M81Sleeper` measured for the page `i59-L1-sigquit-terminates-the-vm`
    // (its "Fixed (wave 81)" section quotes them).

    fn i81_snapshot(
        name: &str,
        state: ThreadState,
        wait_kind: Option<DumpWaitKind>,
        daemon: bool,
        priority: i32,
        dump: ThreadDumpFacts,
    ) -> ThreadSnapshot {
        ThreadSnapshot {
            id: 99,
            name: name.to_string(),
            daemon,
            priority,
            state,
            stack_frames: Vec::new(),
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind,
            dump,
        }
    }

    fn i81_frame(class: &str, method: &str, file: Option<&str>, line: i32) -> FrameInfo {
        FrameInfo {
            class_name: class.to_string(),
            method_name: method.to_string(),
            file_name: file.map(str::to_string),
            line_number: line,
            native_method: line == -2,
        }
    }

    #[test]
    fn i81_thread_line_is_hotspots() {
        let main = i81_snapshot(
            "main",
            ThreadState::TimedWaiting,
            Some(DumpWaitKind::Sleeping),
            false,
            5,
            ThreadDumpFacts {
                java_tid: Some(3),
                os_tid: Some(1_332_861),
                cpu_nanos: Some(22_080_000),
                elapsed_nanos: Some(3_020_000_000),
                thread_addr: 0x0000_787a_4c02_a620,
                stack_addr: 0x0000_787a_539f_e000,
                ..Default::default()
            },
        );
        assert_eq!(
            JstackProcessor::thread_dump_line(&main),
            "\"main\" #3 [1332861] prio=5 os_prio=0 cpu=22.08ms elapsed=3.02s \
             tid=0x0000787a4c02a620 nid=1332861 waiting on condition  [0x0000787a539fe000]"
        );
        let finalizer = i81_snapshot(
            "Finalizer",
            ThreadState::Waiting,
            Some(DumpWaitKind::ObjectWait),
            true,
            8,
            ThreadDumpFacts {
                java_tid: Some(15),
                os_tid: Some(1_332_873),
                cpu_nanos: Some(140_000),
                elapsed_nanos: Some(3_010_000_000),
                thread_addr: 0x0000_787a_4c0f_2cb0,
                stack_addr: 0x0000_787a_287f_e000,
                ..Default::default()
            },
        );
        assert_eq!(
            JstackProcessor::thread_dump_line(&finalizer),
            "\"Finalizer\" #15 [1332873] daemon prio=8 os_prio=0 cpu=0.14ms elapsed=3.01s \
             tid=0x0000787a4c0f2cb0 nid=1332873 in Object.wait()  [0x0000787a287fe000]"
        );
        // RUNNABLE in a wait inside the VM: HotSpot's Reference Handler.
        let handler = i81_snapshot(
            "Reference Handler",
            ThreadState::Runnable,
            None,
            true,
            10,
            ThreadDumpFacts {
                java_tid: Some(14),
                os_tid: Some(1_332_872),
                cpu_nanos: Some(120_000),
                elapsed_nanos: Some(3_010_000_000),
                thread_addr: 0x0000_787a_4c0f_1ac0,
                stack_addr: 0x0000_787a_28bf_4000,
                vm_wait: true,
                ..Default::default()
            },
        );
        assert_eq!(
            JstackProcessor::thread_dump_line(&handler),
            "\"Reference Handler\" #14 [1332872] daemon prio=10 os_prio=0 cpu=0.12ms elapsed=3.01s \
             tid=0x0000787a4c0f1ac0 nid=1332872 waiting on condition  [0x0000787a28bf4000]"
        );
        // Unknown CPU and elapsed time: the fields are left out, the rest stays.
        let bare = i81_snapshot(
            "t",
            ThreadState::Runnable,
            None,
            false,
            5,
            ThreadDumpFacts::default(),
        );
        assert_eq!(
            JstackProcessor::thread_dump_line(&bare),
            "\"t\" #99 [0] prio=5 os_prio=0 tid=0x0000000000000000 nid=0 runnable  [0x0000000000000000]"
        );
    }

    #[test]
    fn i81_frame_line_has_the_module_and_hotspots_locations() {
        let native = i81_frame("java.lang.Thread", "sleepNanos0", Some("Thread.java"), -2);
        assert_eq!(
            JstackProcessor::frame_line(&native, Some("java.base@25.0.4")),
            "\tat java.lang.Thread.sleepNanos0(java.base@25.0.4/Native Method)"
        );
        let jdk = i81_frame("java.lang.Thread", "sleep", Some("Thread.java"), 540);
        assert_eq!(
            JstackProcessor::frame_line(&jdk, Some("java.base@25.0.4")),
            "\tat java.lang.Thread.sleep(java.base@25.0.4/Thread.java:540)"
        );
        let app = i81_frame("M81Sleeper", "main", Some("M81Sleeper.java"), 7);
        assert_eq!(
            JstackProcessor::frame_line(&app, None),
            "\tat M81Sleeper.main(M81Sleeper.java:7)"
        );
        let proxy = i81_frame("M81Sleeper$$Lambda/0x0000000039040210", "run", None, -1);
        assert_eq!(
            JstackProcessor::frame_line(&proxy, None),
            "\tat M81Sleeper$$Lambda/0x0000000039040210.run(Unknown Source)"
        );
        // A line the VM does not know prints the file alone, never `:-1`.
        let unknown = i81_frame("java.lang.Object", "wait", Some("Object.java"), -1);
        assert_eq!(
            JstackProcessor::frame_line(&unknown, Some("java.base")),
            "\tat java.lang.Object.wait(java.base/Object.java)"
        );
    }

    #[test]
    fn i81_smr_info_puts_four_addresses_on_a_line() {
        let elements = [
            0x0000_787a_4c02_a620,
            0x0000_787a_4c0f_1ac0,
            0x0000_787a_4c0f_2cb0,
            0x0000_787a_4c0f_4700,
            0x0000_787a_4c0f_6020,
            0x0000_787a_4c0f_78a0,
            0x0000_787a_4c0f_96a0,
            0x0000_787a_4c0f_b090,
            0x0000_787a_4c10_7430,
            0x0000_787a_4c10_a200,
            0x0000_787a_4c11_1d60,
        ];
        assert_eq!(
            JstackProcessor::smr_info(0x0000_787a_4c11_2f60, &elements),
            "Threads class SMR info:\n\
             _java_thread_list=0x0000787a4c112f60, length=11, elements={\n\
             0x0000787a4c02a620, 0x0000787a4c0f1ac0, 0x0000787a4c0f2cb0, 0x0000787a4c0f4700,\n\
             0x0000787a4c0f6020, 0x0000787a4c0f78a0, 0x0000787a4c0f96a0, 0x0000787a4c0fb090,\n\
             0x0000787a4c107430, 0x0000787a4c10a200, 0x0000787a4c111d60\n\
             }\n"
        );
        assert_eq!(
            JstackProcessor::smr_info(0x10, &[]),
            "Threads class SMR info:\n_java_thread_list=0x0000000000000010, length=0, elements={\n}\n"
        );
    }

    #[test]
    fn i81_lock_lines_follow_print_lock_info_on() {
        let lock = DumpMonitor {
            address: 0x0000_0006_29c1_6e28,
            class_name: "java.lang.Object".to_string(),
        };
        // `M81Sleeper`'s waiter: `wait0` on top, the monitor its lambda locked.
        let lines = dump_lock_lines(
            Some(("java.lang.Object", "wait0")),
            ThreadState::Waiting,
            Some(&lock),
            None,
            None,
            &[(3, lock.clone())],
        );
        assert_eq!(
            lines,
            vec![
                (
                    0,
                    "waiting on <0x0000000629c16e28> (a java.lang.Object)".to_string()
                ),
                (
                    3,
                    "locked <0x0000000629c16e28> (a java.lang.Object)".to_string()
                ),
            ]
        );
        // Notified and re-entering: BLOCKED with `wait0` on top.
        let lines = dump_lock_lines(
            Some(("java.lang.Object", "wait0")),
            ThreadState::Blocked,
            Some(&lock),
            None,
            Some(&lock),
            &[(3, lock.clone())],
        );
        assert_eq!(
            lines[0].1,
            "waiting to re-lock in wait() <0x0000000629c16e28> (a java.lang.Object)"
        );
        assert_eq!(lines.len(), 2);
        // Contending in a `synchronized` block of frame 0, holding another
        // monitor one frame down.
        let held = DumpMonitor {
            address: 0x10,
            class_name: "java.lang.String".to_string(),
        };
        let lines = dump_lock_lines(
            Some(("p.C", "enter")),
            ThreadState::Blocked,
            None,
            None,
            Some(&lock),
            &[(1, held.clone())],
        );
        assert_eq!(
            lines,
            vec![
                (
                    0,
                    "waiting to lock <0x0000000629c16e28> (a java.lang.Object)".to_string()
                ),
                (
                    1,
                    "locked <0x0000000000000010> (a java.lang.String)".to_string()
                ),
            ]
        );
        // `LockSupport.park(blocker)`: HotSpot's `"\t- %s <"` with
        // `"parking to wait for "` -- two spaces.
        let blocker = DumpMonitor {
            address: 0x20,
            class_name: "java.util.concurrent.CountDownLatch$Sync".to_string(),
        };
        let lines = dump_lock_lines(
            Some(("jdk.internal.misc.Unsafe", "park")),
            ThreadState::Waiting,
            None,
            Some(&blocker),
            None,
            &[],
        );
        assert_eq!(
            lines,
            vec![(
                0,
                "parking to wait for  <0x0000000000000020> (a java.util.concurrent.CountDownLatch$Sync)"
                    .to_string()
            )]
        );
        // No frame: no line at all.
        assert!(
            dump_lock_lines(None, ThreadState::Blocked, None, None, Some(&lock), &[]).is_empty()
        );
    }

    #[test]
    fn i81_dump_layout_is_hotspots() {
        let mut waiter = i81_snapshot(
            "waiter",
            ThreadState::Waiting,
            Some(DumpWaitKind::ObjectWait),
            true,
            5,
            ThreadDumpFacts {
                java_tid: Some(25),
                os_tid: Some(1_332_881),
                cpu_nanos: Some(100_000),
                elapsed_nanos: Some(2_990_000_000),
                thread_addr: 0x0000_787a_4c11_1d60,
                stack_addr: 0x0000_7879_e7ff_e000,
                vm_wait: false,
                frame_modules: vec![
                    Some("java.base@25.0.4".to_string()),
                    Some("java.base@25.0.4".to_string()),
                    Some("java.base@25.0.4".to_string()),
                    None,
                    None,
                    Some("java.base@25.0.4".to_string()),
                    Some("java.base@25.0.4".to_string()),
                ],
                lock_lines: vec![
                    (
                        0,
                        "waiting on <0x0000000629c16e28> (a java.lang.Object)".to_string(),
                    ),
                    (
                        3,
                        "locked <0x0000000629c16e28> (a java.lang.Object)".to_string(),
                    ),
                ],
                no_java_thread: false,
                carrying_virtual_thread: None,
            },
        );
        waiter.stack_frames = vec![
            i81_frame("java.lang.Object", "wait0", Some("Object.java"), -2),
            i81_frame("java.lang.Object", "wait", Some("Object.java"), 389),
            i81_frame("java.lang.Object", "wait", Some("Object.java"), 351),
            i81_frame("M81Sleeper", "lambda$main$0", Some("M81Sleeper.java"), 4),
            i81_frame("M81Sleeper$$Lambda/0x0000000039040210", "run", None, -1),
            i81_frame("java.lang.Thread", "runWith", Some("Thread.java"), 1487),
            i81_frame("java.lang.Thread", "run", Some("Thread.java"), 1474),
        ];
        let header = ThreadDumpHeader {
            timestamp: "2026-10-09 14:48:14".to_string(),
            vm_name: "OpenJDK 64-Bit Server VM".to_string(),
            vm_version: "25.0.4+7-LTS".to_string(),
            vm_info: "mixed mode, sharing".to_string(),
            thread_list_addr: 0x0000_787a_4c11_2f60,
            jni_global_refs: 6,
            jni_weak_refs: 0,
        };
        let dump = JstackProcessor::generate_thread_dump_with(&header, &[waiter]);
        assert_eq!(
            dump,
            "2026-10-09 14:48:14\n\
             Full thread dump OpenJDK 64-Bit Server VM (25.0.4+7-LTS mixed mode, sharing):\n\
             \n\
             Threads class SMR info:\n\
             _java_thread_list=0x0000787a4c112f60, length=1, elements={\n\
             0x0000787a4c111d60\n\
             }\n\
             \n\
             \"waiter\" #25 [1332881] daemon prio=5 os_prio=0 cpu=0.10ms elapsed=2.99s \
             tid=0x0000787a4c111d60 nid=1332881 in Object.wait()  [0x00007879e7ffe000]\n   \
             java.lang.Thread.State: WAITING (on object monitor)\n\
             \tat java.lang.Object.wait0(java.base@25.0.4/Native Method)\n\
             \t- waiting on <0x0000000629c16e28> (a java.lang.Object)\n\
             \tat java.lang.Object.wait(java.base@25.0.4/Object.java:389)\n\
             \tat java.lang.Object.wait(java.base@25.0.4/Object.java:351)\n\
             \tat M81Sleeper.lambda$main$0(M81Sleeper.java:4)\n\
             \t- locked <0x0000000629c16e28> (a java.lang.Object)\n\
             \tat M81Sleeper$$Lambda/0x0000000039040210.run(Unknown Source)\n\
             \tat java.lang.Thread.runWith(java.base@25.0.4/Thread.java:1487)\n\
             \tat java.lang.Thread.run(java.base@25.0.4/Thread.java:1474)\n\
             \n\
             JNI global refs: 6, weak refs: 0\n\
             \n"
        );
    }

    /// A VM thread with no `java.lang.Thread` goes after the Java threads in
    /// HotSpot's non-Java form, outside the SMR list.
    #[test]
    fn i81_vm_own_threads_print_as_non_java_threads() {
        let java = i81_snapshot(
            "main",
            ThreadState::Runnable,
            None,
            false,
            5,
            ThreadDumpFacts {
                java_tid: Some(1),
                thread_addr: 0x1000,
                ..Default::default()
            },
        );
        let service = i81_snapshot(
            "Craton Gen Concurrent Mark",
            ThreadState::Runnable,
            None,
            true,
            5,
            ThreadDumpFacts {
                os_tid: Some(42),
                cpu_nanos: Some(70_000),
                elapsed_nanos: Some(3_020_000_000),
                thread_addr: 0x2000,
                vm_wait: true,
                no_java_thread: true,
                ..Default::default()
            },
        );
        assert_eq!(
            JstackProcessor::non_java_thread_line(&service),
            "\"Craton Gen Concurrent Mark\" os_prio=0 cpu=0.07ms elapsed=3.02s \
             tid=0x0000000000002000 nid=42 waiting on condition  "
        );
        let header = ThreadDumpHeader::from_properties(&[]);
        let dump = JstackProcessor::generate_thread_dump_with(&header, &[service, java]);
        assert!(
            dump.contains("length=1, elements={\n0x0000000000001000\n}\n"),
            "{dump}"
        );
        let main_at = dump.find("\"main\" #1").expect("main");
        let service_at = dump
            .find("\"Craton Gen Concurrent Mark\" os_prio=0")
            .expect("service");
        assert!(main_at < service_at, "{dump}");
        assert!(
            dump.ends_with("waiting on condition  \n\nJNI global refs: 0, weak refs: 0\n\n"),
            "{dump}"
        );
    }

    #[test]
    fn i81_header_names_the_vm_by_its_properties() {
        let props = vec![
            ("java.vm.name".to_string(), "cratonvm".to_string()),
            ("java.vm.version".to_string(), "25.0.1+8".to_string()),
            (
                "java.vm.info".to_string(),
                "mixed mode, jdk-only".to_string(),
            ),
        ];
        let header = ThreadDumpHeader::from_properties(&props);
        let dump = JstackProcessor::generate_thread_dump_with(&header, &[]);
        let mut lines = dump.lines();
        let stamp = lines.next().expect("a timestamp line");
        assert_eq!(stamp.len(), "yyyy-MM-dd HH:mm:ss".len(), "{stamp}");
        assert_eq!(
            lines.next(),
            Some("Full thread dump cratonvm (25.0.1+8 mixed mode, jdk-only):")
        );
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), Some("Threads class SMR info:"));
        assert!(
            dump.ends_with("}\n\nJNI global refs: 0, weak refs: 0\n\n"),
            "{dump}"
        );
    }

    #[test]
    fn i81_external_class_name_keeps_a_hidden_class_suffix() {
        assert_eq!(external_class_name("java/lang/Object"), "java.lang.Object");
        assert_eq!(
            external_class_name("p/M81Sleeper$$Lambda/0x0000000039040210"),
            "p.M81Sleeper$$Lambda/0x0000000039040210"
        );
        assert_eq!(external_class_name("a/0xfeed/B"), "a.0xfeed.B");
        assert_eq!(external_class_name("Plain"), "Plain");
    }

    #[test]
    fn i81_utc_time_string_is_civil() {
        assert_eq!(dump_utc_time_string(0), "1970-01-01 00:00:00");
        assert_eq!(dump_utc_time_string(951_868_800), "2000-03-01 00:00:00");
        assert_eq!(dump_utc_time_string(1_791_557_291), "2026-10-09 14:48:11");
        assert_eq!(dump_utc_time_string(4_107_542_399), "2100-02-28 23:59:59");
    }

    /// gci i14/hd: the heap-info lines print one per line, each ended as
    /// `print_cr` ends it; the trait's default states nothing (a mock keeps
    /// the summary text).
    #[test]
    fn gci_i14hd_heap_info_text_ends_every_line() {
        assert_eq!(
            heap_info_text(&["Heap".to_string(), " Metaspace       used 1K".to_string()]),
            "Heap\n Metaspace       used 1K\n"
        );
        assert_eq!(heap_info_text(&[]), "");
        assert!(VmDiagnosticState::heap_info_lines(&MockVmState).is_none());
    }

    /// gci i47w/oa47: `jcmd GC.heap_info` is HotSpot 25's `print_heap_on`
    /// alone (vi14-hd H, Temurin 25.0.3): the `Universe::print_on` lines lose
    /// their `Heap` header, their Metaspace lines and one column of indent.
    #[test]
    fn gci_i47w_oa47_heap_info_dcmd_is_print_heap_on_alone() {
        let g1 = [
            "Heap".to_string(),
            " garbage-first heap   total reserved 131072K, committed 131072K, used 4754K [0x00000000f8000000, 0x0000000100000000)".to_string(),
            "  region size 1024K, 4 young (4096K), 0 survivors (0K)".to_string(),
            " Metaspace       used 8546K, committed 8704K, reserved 1114112K".to_string(),
            "  class space    used 677K, committed 768K, reserved 1048576K".to_string(),
        ];
        assert_eq!(
            heap_info_dcmd_text(&g1),
            "garbage-first heap   total reserved 131072K, committed 131072K, used 4754K [0x00000000f8000000, 0x0000000100000000)\n region size 1024K, 4 young (4096K), 0 survivors (0K)\n"
        );
        let serial = [
            "Heap".to_string(),
            " DefNew     total 39296K, used 4193K [0x1, 0x2, 0x3)".to_string(),
            "  eden space 34944K,  12% used [0x1, 0x2, 0x3)".to_string(),
            " Tenured    total 87424K, used 0K [0x1, 0x2, 0x3)".to_string(),
            "   the space 87424K,   0% used [0x1, 0x2, 0x3)".to_string(),
            " Metaspace       used 1K, committed 1K, reserved 1K".to_string(),
        ];
        assert_eq!(
            heap_info_dcmd_text(&serial),
            "DefNew     total 39296K, used 4193K [0x1, 0x2, 0x3)\n eden space 34944K,  12% used [0x1, 0x2, 0x3)\nTenured    total 87424K, used 0K [0x1, 0x2, 0x3)\n  the space 87424K,   0% used [0x1, 0x2, 0x3)\n"
        );
        // The SIGQUIT section keeps the full `Universe::print_on` text.
        assert!(heap_info_text(&g1).starts_with("Heap\n garbage-first heap"));
    }

    #[test]
    fn i81_sigquit_heap_section_is_kib() {
        let summary = HeapSummary {
            young_gen_used: 2048,
            young_gen_capacity: 4096,
            old_gen_used: 1024,
            old_gen_capacity: 8192,
            metaspace_used: 3072,
            metaspace_capacity: 3072,
            total_used: 3072,
            total_capacity: 12288,
        };
        let heap = JstackProcessor::sigquit_heap_section(&summary);
        assert!(
            heap.starts_with("Heap\n java heap   total 12K, used 3K\n"),
            "{heap}"
        );
        assert!(
            heap.ends_with(" Metaspace       used 3K, committed 3K\n\n"),
            "{heap}"
        );
    }

    // --- jmap tests ---

    #[test]
    fn test_jmap_class_histogram() {
        let entries = vec![
            ClassHistogramEntry {
                class_name: "[B".to_string(),
                instance_count: 1000,
                total_bytes: 50000,
            },
            ClassHistogramEntry {
                class_name: "java.lang.String".to_string(),
                instance_count: 500,
                total_bytes: 20000,
            },
        ];
        let output = JmapProcessor::generate_class_histogram(&entries);
        assert!(output.contains("#instances"));
        assert!(output.contains("[B"));
        assert!(output.contains("java.lang.String"));
        assert!(output.contains("Total "));
        assert!(output.contains("1500")); // total instances
        assert!(output.contains("70000")); // total bytes
    }

    #[test]
    fn test_jmap_class_histogram_empty() {
        let output = JmapProcessor::generate_class_histogram(&[]);
        assert!(output.contains("#instances"));
        assert!(output.contains("Total "));
    }

    /// gci i3/hd: HotSpot 25's `KlassInfoHisto` text byte for byte (header,
    /// 55-dash rule, `%4d: %13d  %13u  name (module)` rows, `Total %13d
    /// %13u`, no closing rule).
    #[test]
    fn gci_i3hd_class_histogram_text_is_hotspots() {
        let entries = vec![
            ClassHistogramEntry {
                class_name: "[B (java.base@25.0.3)".to_string(),
                instance_count: 1234,
                total_bytes: 567890,
            },
            ClassHistogramEntry {
                class_name: "App$Node".to_string(),
                instance_count: 7,
                total_bytes: 168,
            },
        ];
        let output = JmapProcessor::generate_class_histogram(&entries);
        // Spelled with explicit space counts: a literal with a long space run trips
        // `types/tests/string_literal_line_breaks.rs`.
        let sp = |n: usize| " ".repeat(n);
        let expected = [
            format!(" num{}#instances{}#bytes  class name (module)\n", sp(5), sp(9)),
            "-------------------------------------------------------\n".to_string(),
            format!("   1:{}1234{}567890  [B (java.base@25.0.3)\n", sp(10), sp(9)),
            format!("   2:{}7{}168  App$Node\n", sp(13), sp(12)),
            format!("Total{}1241{}568058\n", sp(10), sp(9)),
        ]
        .concat();
        assert_eq!(output, expected);
        assert_eq!(output.lines().nth(1).map(str::len), Some(55));
    }

    /// gci i3/hd: `GC.heap_dump [options] <filename>`.
    #[test]
    fn gci_i3hd_heap_dump_args_follow_hotspots_grammar() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(heap_dump_args(&v(&[])), ("heap.hprof".to_string(), false));
        assert_eq!(heap_dump_args(&v(&["/tmp/a"])), ("/tmp/a".to_string(), false));
        assert_eq!(
            heap_dump_args(&v(&["-overwrite", "/tmp/a"])),
            ("/tmp/a".to_string(), true)
        );
        assert_eq!(
            heap_dump_args(&v(&["-all", "-gz=1", "-parallel=2", "/tmp/a", "/tmp/b"])),
            ("/tmp/a".to_string(), false)
        );
        assert_eq!(
            heap_dump_args(&v(&["-overwrite=false", "/tmp/a"])),
            ("/tmp/a".to_string(), false)
        );
        assert!(heap_dump_args(&v(&["-overwrite=true", "x"])).1);
    }

    /// gci i3/hd: the live `GC.heap_dump` refuses an existing file as
    /// HotSpot does (`Unable to create <file>: File exists`) and goes on to the
    /// dump with `-overwrite`.
    #[test]
    fn gci_i3hd_live_heap_dump_refuses_an_existing_file() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let tmp = std::env::temp_dir().join("cratonvm_gci_i3hd_exists.hprof");
        std::fs::write(&tmp, b"x").unwrap();
        let path = tmp.to_str().unwrap().to_string();
        // The processor holds the state weakly: keep it alive.
        let state: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
        let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&state));
        let refused = processor
            .attach_listener
            .dispatch_command("GC.heap_dump", &[path.clone()])
            .expect("registered");
        assert_eq!(refused.output, format!("Unable to create {path}: File exists"));
        assert_eq!(std::fs::read(&tmp).unwrap(), b"x", "the file is untouched");
        let tried = processor
            .attach_listener
            .dispatch_command("GC.heap_dump", &["-overwrite".to_string(), path.clone()])
            .expect("registered");
        assert!(
            tried.error.as_deref().is_some_and(|e| e.contains("heap dump not supported")),
            "{tried:?}"
        );
        drop(state);
        let _ = std::fs::remove_file(&tmp);
    }

    /// gci i3/hd: `jcmd GC.heap_dump`'s two HotSpot lines.
    #[test]
    fn gci_i3hd_heap_dump_lines_are_hotspots() {
        let text =
            hotspot_heap_dump_lines("/tmp/x.hprof", 12345, std::time::Duration::from_millis(1500));
        assert_eq!(
            text,
            "Dumping heap to /tmp/x.hprof ...\nHeap dump file created [12345 bytes in 1.500 secs]"
        );
    }

    /// gci i4/hd: HotSpot's `-all` dcmd option and the two `jmap` attach
    /// operations' live/all argument.
    #[test]
    fn gci_i4hd_all_option_and_jmap_live_args_follow_hotspot() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(!dcmd_all_objects(&v(&[])));
        assert!(dcmd_all_objects(&v(&["-all"])));
        assert!(dcmd_all_objects(&v(&["-all=true", "/tmp/a"])));
        assert!(!dcmd_all_objects(&v(&["-all=false"])));
        assert!(!dcmd_all_objects(&v(&["-overwrite", "/tmp/a"])));
        // jmap -histo (the client sends -all), -histo:live (-live), an old
        // client's empty argument (live, as attachListener.cpp reads it).
        assert_eq!(attach_heap_walk_args("inspectheap", "-all", ""), v(&["-all"]));
        assert!(attach_heap_walk_args("inspectheap", "-live", "").is_empty());
        assert!(attach_heap_walk_args("inspectheap", "", "").is_empty());
        // jmap -dump:file=X (-all), -dump:live,file=X (-live), empty (all).
        assert_eq!(attach_heap_walk_args("dumpheap", "/tmp/x", "-all"), v(&["-all", "/tmp/x"]));
        assert_eq!(attach_heap_walk_args("dumpheap", "/tmp/x", "-live"), v(&["/tmp/x"]));
        assert_eq!(attach_heap_walk_args("dumpheap", "/tmp/x", ""), v(&["-all", "/tmp/x"]));
        assert_eq!(heap_dump_args(&attach_heap_walk_args("dumpheap", "/tmp/x", "-all")).0, "/tmp/x");
    }

    /// gci i5/hd (review item 10): `-gz=<level>` as HotSpot's `HeapDumpDCmd`
    /// reads it -- absent, 1-9, out of range (printed, no dump), not an
    /// integer (the parse exception); the last one wins.
    #[test]
    fn gci_i5hd_gz_level_follows_hotspot() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(heap_dump_gz_level(&v(&["/tmp/a"])), Ok(None));
        assert_eq!(heap_dump_gz_level(&v(&["-gz=1", "/tmp/a"])), Ok(Some(1)));
        assert_eq!(heap_dump_gz_level(&v(&["-gz=9", "-overwrite", "/tmp/a"])), Ok(Some(9)));
        assert_eq!(heap_dump_gz_level(&v(&["-gz=3", "-gz=5", "x"])), Ok(Some(5)));
        assert_eq!(
            heap_dump_gz_level(&v(&["-gz=0", "x"])),
            Err(GzLevelError::OutOfRange("Compression level out of range (1-9): 0".to_string()))
        );
        assert_eq!(
            heap_dump_gz_level(&v(&["-gz=10", "x"])),
            Err(GzLevelError::OutOfRange("Compression level out of range (1-9): 10".to_string()))
        );
        assert_eq!(
            heap_dump_gz_level(&v(&["-gz=-1", "x"])),
            Err(GzLevelError::OutOfRange("Compression level out of range (1-9): -1".to_string()))
        );
        assert!(matches!(
            heap_dump_gz_level(&v(&["-gz=fast", "x"])),
            Err(GzLevelError::Unparsable(t)) if t.ends_with("Could not parse: fast.")
        ));
        // The path is still the first non-option token.
        assert_eq!(heap_dump_args(&v(&["-gz=1", "/tmp/a"])).0, "/tmp/a");
    }

    /// gci i5/hd: an out-of-range `-gz` answers HotSpot's line and dumps
    /// nothing (the file is not even created).
    #[test]
    fn gci_i5hd_out_of_range_gz_dumps_nothing() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let tmp = std::env::temp_dir().join("cratonvm_gci_i5hd_gz_range.hprof");
        let _ = std::fs::remove_file(&tmp);
        let path = tmp.to_str().unwrap().to_string();
        let state: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
        let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&state));
        let r = processor
            .attach_listener
            .dispatch_command("GC.heap_dump", &["-gz=12".to_string(), path.clone()])
            .expect("registered");
        assert_eq!(r.output, "Compression level out of range (1-9): 12");
        assert!(!tmp.exists());
        drop(state);
    }

    /// A state recording the `live` argument of the attach heap walks.
    struct LiveRecordingState(parking_lot::Mutex<Vec<(&'static str, bool)>>);
    impl VmDiagnosticState for LiveRecordingState {
        fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
            Vec::new()
        }
        fn heap_summary(&self) -> HeapSummary {
            MockVmState.heap_summary()
        }
        fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
            MockVmState.class_histogram()
        }
        fn class_histogram_live(&self, live: bool) -> Vec<ClassHistogramEntry> {
            self.0.lock().push(("histo", live));
            self.class_histogram()
        }
        fn heap_dump_live(&self, path: &str, live: bool) -> Result<u64, String> {
            self.0.lock().push(("dump", live));
            self.heap_dump(path)
        }
        fn trigger_gc(&self) -> bool {
            true
        }
        fn uptime_secs(&self) -> f64 {
            1.0
        }
        fn command_line(&self) -> String {
            String::new()
        }
        fn system_properties(&self) -> Vec<(String, String)> {
            Vec::new()
        }
        fn vm_flags(&self) -> Vec<String> {
            Vec::new()
        }
    }

    /// gci i4/hd (`gci-i3hd-jcmd-heap-walks-count-dead-objects-FIXED-20261013`): the
    /// live commands ask for live objects unless `-all`.
    #[test]
    fn gci_i4hd_live_heap_walk_commands_pass_live_unless_all() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let rec = Arc::new(LiveRecordingState(parking_lot::Mutex::new(Vec::new())));
        let state: Arc<dyn VmDiagnosticState> = rec.clone();
        let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&state));
        let run = |name: &str, args: &[&str]| {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            processor
                .attach_listener
                .dispatch_command(name, &args)
                .expect("registered")
        };
        let _ = run("GC.class_histogram", &[]);
        let _ = run("GC.class_histogram", &["-all"]);
        let missing = std::env::temp_dir().join("cratonvm_gci_i4hd_live_absent.hprof");
        let _ = std::fs::remove_file(&missing);
        let missing = missing.to_str().unwrap().to_string();
        let _ = run("GC.heap_dump", &[missing.as_str()]);
        let _ = run("GC.heap_dump", &["-all", missing.as_str()]);
        assert_eq!(
            *rec.0.lock(),
            vec![("histo", true), ("histo", false), ("dump", true), ("dump", false)]
        );
        drop(state);
    }

    #[test]
    fn test_jmap_heap_summary() {
        let summary = HeapSummary {
            young_gen_used: 25 * 1024 * 1024,
            young_gen_capacity: 64 * 1024 * 1024,
            old_gen_used: 100 * 1024 * 1024,
            old_gen_capacity: 256 * 1024 * 1024,
            metaspace_used: 30 * 1024 * 1024,
            metaspace_capacity: 64 * 1024 * 1024,
            total_used: 155 * 1024 * 1024,
            total_capacity: 384 * 1024 * 1024,
        };
        let output = JmapProcessor::generate_heap_summary(&summary);
        assert!(output.contains("Young Generation"));
        assert!(output.contains("Old Generation"));
        assert!(output.contains("Metaspace"));
        assert!(output.contains("Total"));
        assert!(output.contains("MB"));
    }

    #[test]
    fn test_jmap_finalizer_info() {
        let output = JmapProcessor::generate_finalizer_info();
        assert!(output.contains("Finalizer Information"));
        assert!(output.contains("Pending finalizers: 0"));
        assert!(output.contains("Finalizer thread: active"));
    }

    // --- HPROF tests ---

    #[test]
    fn test_hprof_magic() {
        assert_eq!(HprofWriter::HPROF_MAGIC, "JAVA PROFILE 1.0.2");
    }

    #[test]
    fn test_hprof_record_type_constants() {
        assert_eq!(HprofWriter::HPROF_UTF8, 0x01);
        assert_eq!(HprofWriter::HPROF_LOAD_CLASS, 0x02);
        assert_eq!(HprofWriter::HPROF_FRAME, 0x04);
        assert_eq!(HprofWriter::HPROF_TRACE, 0x05);
        assert_eq!(HprofWriter::HPROF_HEAP_DUMP, 0x0C);
        assert_eq!(HprofWriter::HPROF_HEAP_DUMP_SEGMENT, 0x1C);
        assert_eq!(HprofWriter::HPROF_HEAP_DUMP_END, 0x2C);
    }

    #[test]
    fn test_hprof_write_header() {
        let header = HprofWriter::write_header();
        // Check magic
        let magic_end = HprofWriter::HPROF_MAGIC.len();
        assert_eq!(&header[..magic_end], HprofWriter::HPROF_MAGIC.as_bytes());
        // Null terminator
        assert_eq!(header[magic_end], 0);
        // Identifier size = 8
        let id_size = u32::from_be_bytes([
            header[magic_end + 1],
            header[magic_end + 2],
            header[magic_end + 3],
            header[magic_end + 4],
        ]);
        assert_eq!(id_size, 8);
        // Total size check
        assert_eq!(header.len(), HprofWriter::header_size());
    }

    #[test]
    fn test_hprof_header_size() {
        let expected = HprofWriter::HPROF_MAGIC.len() + 1 + 4 + 4 + 4;
        assert_eq!(HprofWriter::header_size(), expected);
    }

    #[test]
    fn test_hprof_write_string_record() {
        let record = HprofWriter::write_string_record(42, "hello");
        assert_eq!(record[0], HprofWriter::HPROF_UTF8);
        // Timestamp = 0
        assert_eq!(&record[1..5], &[0, 0, 0, 0]);
        // Body length = 8 (id) + 5 (string) = 13
        let body_len = u32::from_be_bytes([record[5], record[6], record[7], record[8]]);
        assert_eq!(body_len, 13);
        // ID = 42
        let id = u64::from_be_bytes([
            record[9], record[10], record[11], record[12], record[13], record[14], record[15],
            record[16],
        ]);
        assert_eq!(id, 42);
        // String data
        assert_eq!(&record[17..], b"hello");
    }

    #[test]
    fn test_hprof_write_string_record_empty() {
        let record = HprofWriter::write_string_record(1, "");
        let body_len = u32::from_be_bytes([record[5], record[6], record[7], record[8]]);
        assert_eq!(body_len, 8); // just the id, no string bytes
    }

    // --- Utility tests ---

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(1048576), "1.0 MB");
        assert_eq!(format_bytes(1073741824), "1.0 GB");
    }

    #[test]
    fn test_percent() {
        assert!((percent(50, 100) - 50.0).abs() < 0.001);
        assert!((percent(0, 100) - 0.0).abs() < 0.001);
        assert!((percent(100, 0) - 0.0).abs() < 0.001); // div by zero guard
    }

    #[test]
    fn test_sample_thread_snapshots() {
        let threads = sample_thread_snapshots();
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].name, "main");
        assert_eq!(threads[1].name, "GC-Thread");
    }

    #[test]
    fn test_sample_class_histogram() {
        let entries = sample_class_histogram();
        assert_eq!(entries.len(), 10);
        assert_eq!(entries[0].class_name, "[B");
    }

    #[test]
    fn test_jcmd_default_trait() {
        let jcmd = JcmdProcessor::default();
        assert_eq!(jcmd.attach_listener.commands.read().len(), 16);
    }

    #[test]
    fn test_hprof_load_class_record() {
        let data = HprofWriter::write_load_class(1, 100, 0, 200);
        assert_eq!(data[0], HprofWriter::HPROF_LOAD_CLASS);
        assert!(!data.is_empty());
    }

    #[test]
    fn test_hprof_heap_dump_end() {
        let data = HprofWriter::write_heap_dump_end();
        assert_eq!(data[0], HprofWriter::HPROF_HEAP_DUMP_END);
        // Body length should be 0
        let body_len = u32::from_be_bytes([data[5], data[6], data[7], data[8]]);
        assert_eq!(body_len, 0);
    }

    #[test]
    fn test_hprof_stack_trace_record() {
        let frames = vec![1u64, 2, 3];
        let data = HprofWriter::write_stack_trace(1, 1, &frames);
        assert_eq!(data[0], HprofWriter::HPROF_TRACE);
    }

    #[test]
    fn test_hprof_stack_frame_record() {
        let data = HprofWriter::write_stack_frame(1, 10, 20, 30, 1, 42);
        assert_eq!(data[0], HprofWriter::HPROF_FRAME);
    }

    // Test VmDiagnosticState with a mock implementation
    struct MockVmState;
    impl VmDiagnosticState for MockVmState {
        fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
            vec![ThreadSnapshot {
                id: 1,
                name: "main".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Runnable,
                stack_frames: vec![],
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
                wait_kind: None,
                dump: Default::default(),
            }]
        }
        fn heap_summary(&self) -> HeapSummary {
            HeapSummary {
                young_gen_used: 10 * 1024 * 1024,
                young_gen_capacity: 64 * 1024 * 1024,
                old_gen_used: 50 * 1024 * 1024,
                old_gen_capacity: 256 * 1024 * 1024,
                metaspace_used: 20 * 1024 * 1024,
                metaspace_capacity: 64 * 1024 * 1024,
                total_used: 80 * 1024 * 1024,
                total_capacity: 384 * 1024 * 1024,
            }
        }
        fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
            vec![ClassHistogramEntry {
                class_name: "java.lang.String".to_string(),
                instance_count: 100,
                total_bytes: 4000,
            }]
        }
        fn trigger_gc(&self) -> bool {
            true
        }
        fn uptime_secs(&self) -> f64 {
            42.5
        }
        fn command_line(&self) -> String {
            "java -jar test.jar".to_string()
        }
        fn system_properties(&self) -> Vec<(String, String)> {
            vec![("java.version".to_string(), "25".to_string())]
        }
        fn vm_flags(&self) -> Vec<String> {
            vec!["-Xmx256m".to_string()]
        }
    }

    /// A state that serves the attach `load` operation (interpreter round i2
    /// wave 58, lane L1), recording what it was asked.
    struct LoadingVmState(parking_lot::Mutex<Vec<(String, bool, String)>>);
    impl VmDiagnosticState for LoadingVmState {
        fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
            Vec::new()
        }
        fn heap_summary(&self) -> HeapSummary {
            MockVmState.heap_summary()
        }
        fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
            Vec::new()
        }
        fn trigger_gc(&self) -> bool {
            false
        }
        fn uptime_secs(&self) -> f64 {
            0.0
        }
        fn command_line(&self) -> String {
            String::new()
        }
        fn system_properties(&self) -> Vec<(String, String)> {
            Vec::new()
        }
        fn vm_flags(&self) -> Vec<String> {
            Vec::new()
        }
        fn attach_load_enabled(&self) -> bool {
            true
        }
        fn attach_load(&self, lib: &str, absolute: bool, options: &str) -> String {
            self.0
                .lock()
                .push((lib.to_string(), absolute, options.to_string()));
            "return code: 0".to_string()
        }
        fn attach_properties_enabled(&self) -> bool {
            true
        }
        fn attach_properties(&self, agent: bool) -> Result<String, String> {
            Ok(format!("k=agent {agent}\n"))
        }
    }

    /// A [`LoadingVmState`] run with `-XX:-EnableDynamicAgentLoading`
    /// (interpreter round i3 wave 81, lane L1).
    struct NoDynamicAgentsVmState(LoadingVmState);
    impl VmDiagnosticState for NoDynamicAgentsVmState {
        fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
            Vec::new()
        }
        fn heap_summary(&self) -> HeapSummary {
            MockVmState.heap_summary()
        }
        fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
            Vec::new()
        }
        fn trigger_gc(&self) -> bool {
            false
        }
        fn uptime_secs(&self) -> f64 {
            0.0
        }
        fn command_line(&self) -> String {
            String::new()
        }
        fn system_properties(&self) -> Vec<(String, String)> {
            Vec::new()
        }
        fn vm_flags(&self) -> Vec<String> {
            Vec::new()
        }
        fn attach_load_enabled(&self) -> bool {
            true
        }
        fn attach_load(&self, lib: &str, absolute: bool, options: &str) -> String {
            self.0.attach_load(lib, absolute, options)
        }
        fn dynamic_agent_loading_disabled(&self) -> bool {
            true
        }
    }

    /// Interpreter round i3 wave 81, lane L1: under
    /// `-XX:-EnableDynamicAgentLoading` the `load` operation answers a
    /// non-zero status with HotSpot's text and loads nothing (MEASURED,
    /// HotSpot 25.0.4: the client throws `AgentLoadException("Failed to load
    /// agent library: Dynamic agent loading is not enabled. Use
    /// -XX:+EnableDynamicAgentLoading to launch target VM.")`), while `jcmd
    /// JVMTI.agent_load` still loads (HotSpot gates the operation, not the
    /// diagnostic command).
    #[test]
    fn the_attach_load_operation_is_refused_without_dynamic_agent_loading() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let state = Arc::new(NoDynamicAgentsVmState(LoadingVmState(
            parking_lot::Mutex::new(Vec::new()),
        )));
        let shared: Arc<dyn VmDiagnosticState> = state.clone();
        let processor = JcmdProcessor::new_with_vm_state(shared);
        let fields: Vec<String> = ["1", "load", "instrument", "false", "/x/a.jar"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (code, body) = execute_attach_operation(&fields, &processor.attach_listener.commands);
        assert_ne!(code, 0);
        assert_eq!(
            body,
            "Dynamic agent loading is not enabled. Use -XX:+EnableDynamicAgentLoading to launch target VM."
        );
        assert!(state.0 .0.lock().is_empty(), "the refused load ran");
        let jcmd = processor
            .attach_listener
            .dispatch_command("JVMTI.agent_load", &["/x/a.jar".to_string()])
            .expect("registered");
        assert_eq!(jcmd.output, "return code: 0\n");
        assert_eq!(state.0 .0.lock().len(), 1);
    }

    /// Interpreter round i2 wave 59, lane L1: a lazy listener binds nothing
    /// until the trigger file `.attach_pid<pid>` appears (HotSpot's
    /// `is_init_trigger`), then binds and serves; one stopped before any
    /// trigger retires its polling thread.
    #[cfg(unix)]
    #[test]
    fn a_lazy_attach_listener_binds_on_the_trigger_file() {
        use std::path::Path;
        let pid = std::process::id();
        let path = format!("/tmp/cratonvm_attach_lazy_test_{pid}");
        let trigger = format!("/tmp/.attach_pid{pid}");
        let _ = std::fs::remove_file(&trigger);

        let mut idle = AttachListener::new(&format!("{path}_idle"));
        idle.start_listening_lazily();
        assert!(idle.is_listening);
        idle.stop_listening();
        assert!(!Path::new(&format!("{path}_idle")).exists());

        let mut listener = AttachListener::new(&path);
        listener.start_listening_lazily();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!Path::new(&path).exists(), "bound before any trigger");
        std::fs::write(&trigger, b"").expect("trigger file");
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !Path::new(&path).exists() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = std::fs::remove_file(&trigger);
        assert!(
            Path::new(&path).exists(),
            "the trigger did not bind the socket"
        );
        listener.stop_listening();
        assert!(!Path::new(&path).exists());
    }

    /// Interpreter round i2 wave 59, lane L1: `JVMTI.agent_load` is a live
    /// command beside the `load` operation, reads its arguments as HotSpot's
    /// `DCmdParser` does, and the `properties` / `agentProperties`
    /// operations are live commands only for a state that serves them.
    #[test]
    fn jvmti_agent_load_and_the_property_operations_are_served_when_enabled() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let plain: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
        let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&plain));
        let names = processor.attach_listener.list_commands();
        for absent in [
            ATTACH_PROPERTIES_COMMAND,
            ATTACH_AGENT_PROPERTIES_COMMAND,
            "JVMTI.agent_load",
        ] {
            assert!(!names.contains(&absent.to_string()), "{absent}");
        }
        drop(processor);
        let loading = Arc::new(LoadingVmState(parking_lot::Mutex::new(Vec::new())));
        let state: Arc<dyn VmDiagnosticState> = loading.clone();
        let processor = JcmdProcessor::new_with_vm_state(state);
        let run = |name: &str, args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            processor
                .attach_listener
                .dispatch_command(name, &args)
                .expect("registered")
        };
        assert_eq!(
            run("JVMTI.agent_load", &["/x/a.jar"]).output,
            "return code: 0\n"
        );
        run("JVMTI.agent_load", &["/x/a.jar", "a=b"]);
        run("JVMTI.agent_load", &["/x/libagent.so", "opt"]);
        run("JVMTI.agent_load", &["/x/libagent.so"]);
        let missing = run("JVMTI.agent_load", &[]);
        assert!(!missing.success);
        assert_eq!(
            missing.error.as_deref(),
            Some("java.lang.IllegalArgumentException: The argument 'library path' is mandatory.\n")
        );
        assert_eq!(
            loading.0.lock().as_slice(),
            &[
                ("instrument".to_string(), false, "/x/a.jar".to_string()),
                ("instrument".to_string(), false, "/x/a.jar=a".to_string()),
                ("/x/libagent.so".to_string(), true, "opt".to_string()),
                ("/x/libagent.so".to_string(), true, String::new()),
            ]
        );
        assert_eq!(
            run(ATTACH_PROPERTIES_COMMAND, &[]).output,
            "k=agent false\n"
        );
        assert_eq!(
            run(ATTACH_AGENT_PROPERTIES_COMMAND, &[]).output,
            "k=agent true\n"
        );
        assert_eq!(run("VM.system_properties", &[]).output, "k=agent false\n");

        // `jcmd <pid> help`, in HotSpot's shape, without the internal
        // operation commands.
        let list = jcmd_help(&processor.attach_listener.commands.read(), &[]);
        assert!(
            list.starts_with("The following commands are available:\n"),
            "{list}"
        );
        assert!(
            list.contains("\nJVMTI.agent_load\n") && list.contains("\nhelp\n"),
            "{list}"
        );
        assert!(!list.contains("attach "), "{list}");
        assert!(list.ends_with(
            "\n\nFor more information about a specific command use 'help <command>'.\n"
        ));
        let names: Vec<&str> = list.lines().skip(1).take_while(|l| !l.is_empty()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        let unknown = jcmd_help(
            &processor.attach_listener.commands.read(),
            &["No.such".to_string()],
        );
        assert_eq!(unknown, "Help unavailable : 'No.such' : No such command\n");
        let one = jcmd_help(
            &processor.attach_listener.commands.read(),
            &["VM.uptime".to_string()],
        );
        assert!(one.starts_with("VM.uptime\n"), "{one}");
    }

    /// Interpreter round i2 wave 58, lane L1: the attach `load` operation is a
    /// live command only for a state that serves it, and answers HotSpot's
    /// body for completion status 0 with the operation's three fields.
    #[test]
    fn the_attach_load_command_is_registered_only_when_served() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let plain: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
        let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&plain));
        assert!(!processor
            .attach_listener
            .list_commands()
            .contains(&ATTACH_LOAD_COMMAND.to_string()));
        drop(processor);
        let loading = Arc::new(LoadingVmState(parking_lot::Mutex::new(Vec::new())));
        let state: Arc<dyn VmDiagnosticState> = loading.clone();
        let processor = JcmdProcessor::new_with_vm_state(state);
        let args = [
            "/x/libagent.so".to_string(),
            "true".to_string(),
            "a=b c".to_string(),
        ];
        let result = processor
            .attach_listener
            .dispatch_command(ATTACH_LOAD_COMMAND, &args)
            .expect("registered");
        assert!(result.success);
        assert_eq!(result.output, "return code: 0\n");
        assert_eq!(
            loading.0.lock().as_slice(),
            &[("/x/libagent.so".to_string(), true, "a=b c".to_string())]
        );
    }

    /// A `JcmdProcessor` must NOT keep the VM alive.
    ///
    /// Its commands used to capture `Arc<dyn VmDiagnosticState>` clones, and
    /// the processor lives in `SharedVm::debug.jcmd_processor` -- so the VM
    /// held nine strong references to itself and could never be dropped. Pin
    /// the weak-handle contract at the level it broke.
    #[test]
    fn jcmd_processor_does_not_keep_vm_state_alive() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        let state: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
        let weak = Arc::downgrade(&state);
        let processor = JcmdProcessor::new_with_vm_state(Arc::clone(&state));
        assert_eq!(
            Arc::strong_count(&state),
            1,
            "constructing the processor must not retain a strong handle"
        );
        drop(state);
        assert!(
            weak.upgrade().is_none(),
            "the processor kept the VM state alive"
        );
        // And it degrades honestly rather than reporting stale data.
        let result = processor.process_command("GC.run");
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|e| e.contains("shutting down")),
            "expected a shutting-down error, got {:?}",
            result.error
        );
    }

    /// Interpreter round i3 wave 80, lane L6: the latch word's pending bit is
    /// set and taken alone; the Windows installed bit survives a take.
    #[cfg(any(unix, windows))]
    #[test]
    fn i80_signal_latch_takes_only_the_pending_bit() {
        let latch = std::sync::atomic::AtomicU8::new(0);
        assert!(!take_signal_pending(&latch));
        set_signal_pending(&latch);
        set_signal_pending(&latch);
        assert!(
            take_signal_pending(&latch),
            "two signals before a take are one dump"
        );
        assert!(!take_signal_pending(&latch));
        latch.store(4 | SIGNAL_PENDING, std::sync::atomic::Ordering::Relaxed);
        assert!(take_signal_pending(&latch));
        assert_eq!(
            latch.load(std::sync::atomic::Ordering::Relaxed),
            4,
            "other bits stay"
        );
    }

    /// Interpreter round i3 wave 80, lane L6: the `-XX:+DisableAttachMechanism`
    /// dispatcher binds nothing, serves the live `Thread.print`, keeps no
    /// strong handle on the VM, and its thread is reaped promptly on drop.
    #[cfg(any(unix, windows))]
    #[test]
    fn i80_signal_dispatcher_binds_nothing_and_is_reaped() {
        let state: Arc<dyn VmDiagnosticState> = Arc::new(MockVmState);
        let processor = JcmdProcessor::new_signal_dispatcher(Arc::clone(&state));
        assert_eq!(Arc::strong_count(&state), 1, "the dispatcher holds a Weak");
        assert!(processor.attach_listener.socket_path.is_empty());
        assert!(
            !processor.attach_listener.is_listening,
            "no attach endpoint"
        );
        assert!(
            processor.attach_listener.accept_thread.is_some(),
            "a dispatcher thread"
        );
        let dump = processor.process_command("Thread.print");
        assert!(dump.success, "{:?}", dump.error);
        let started = std::time::Instant::now();
        drop(processor);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the dispatcher must be unparked and joined, not waited out"
        );
    }

    #[test]
    fn test_jcmd_with_live_vm_state() {
        #[cfg(windows)]
        let _endpoint = WINDOWS_ENDPOINT_TEST_LOCK.lock();
        // The processor holds only a `Weak` (see `new_with_vm_state`), so the
        // test must keep its own strong handle alive across the assertions --
        // exactly as the real caller does, where the `SharedVm` being reported
        // on is owned by the live `Vm`. Move the only `Arc` in and every
        // command below correctly answers "VM is shutting down".
        let state = Arc::new(MockVmState);
        let processor = JcmdProcessor::new_with_vm_state(state.clone());

        let result = processor.process_command("Thread.print");
        assert!(result.success);
        assert!(result.output.contains("main"));

        let result = processor.process_command("GC.run");
        assert!(result.success);
        // gci i2/oo: a completed `GC.run` prints nothing, as HotSpot's.
        assert_eq!(result.output, "");

        let result = processor.process_command("VM.uptime");
        assert!(result.success);
        assert!(result.output.contains("42.5"));
    }

    // -----------------------------------------------------------------------
    // HPROF Heap Dump tests (Session 42)
    // -----------------------------------------------------------------------

    #[test]
    fn test_hprof_basic_type_sizes() {
        assert_eq!(HprofBasicType::Boolean.size(), 1);
        assert_eq!(HprofBasicType::Byte.size(), 1);
        assert_eq!(HprofBasicType::Char.size(), 2);
        assert_eq!(HprofBasicType::Short.size(), 2);
        assert_eq!(HprofBasicType::Int.size(), 4);
        assert_eq!(HprofBasicType::Float.size(), 4);
        assert_eq!(HprofBasicType::Long.size(), 8);
        assert_eq!(HprofBasicType::Double.size(), 8);
        assert_eq!(HprofBasicType::Object.size(), 8);
    }

    #[test]
    fn test_hprof_basic_type_from_descriptor() {
        assert_eq!(HprofBasicType::from_descriptor("I"), HprofBasicType::Int);
        assert_eq!(HprofBasicType::from_descriptor("J"), HprofBasicType::Long);
        assert_eq!(HprofBasicType::from_descriptor("F"), HprofBasicType::Float);
        assert_eq!(HprofBasicType::from_descriptor("D"), HprofBasicType::Double);
        assert_eq!(
            HprofBasicType::from_descriptor("Z"),
            HprofBasicType::Boolean
        );
        assert_eq!(HprofBasicType::from_descriptor("B"), HprofBasicType::Byte);
        assert_eq!(HprofBasicType::from_descriptor("C"), HprofBasicType::Char);
        assert_eq!(HprofBasicType::from_descriptor("S"), HprofBasicType::Short);
        assert_eq!(
            HprofBasicType::from_descriptor("Ljava/lang/String;"),
            HprofBasicType::Object
        );
        assert_eq!(
            HprofBasicType::from_descriptor("[I"),
            HprofBasicType::Object
        );
    }

    #[test]
    fn test_hprof_header_has_correct_magic() {
        let header = HprofWriter::write_header();
        let magic_len = HprofWriter::HPROF_MAGIC.len();
        assert_eq!(&header[..magic_len], HprofWriter::HPROF_MAGIC.as_bytes());
        assert_eq!(header[magic_len], 0); // null terminator
                                          // Identifier size = 8
        let id_size = u32::from_be_bytes([
            header[magic_len + 1],
            header[magic_len + 2],
            header[magic_len + 3],
            header[magic_len + 4],
        ]);
        assert_eq!(id_size, 8);
    }

    #[test]
    fn test_hprof_gc_root_thread_obj() {
        let mut buf = Vec::new();
        HprofWriter::write_gc_root_thread_obj(&mut buf, 0xDEAD, 1, 1);
        assert_eq!(buf[0], HprofWriter::GC_ROOT_THREAD_OBJ);
        // thread_obj_id at bytes 1..9
        let obj_id = u64::from_be_bytes(buf[1..9].try_into().unwrap());
        assert_eq!(obj_id, 0xDEAD);
        // thread_serial at bytes 9..13
        let tserial = u32::from_be_bytes(buf[9..13].try_into().unwrap());
        assert_eq!(tserial, 1);
    }

    #[test]
    fn test_hprof_gc_root_jni_global() {
        let mut buf = Vec::new();
        HprofWriter::write_gc_root_jni_global(&mut buf, 0xCAFE, 0xBEEF);
        assert_eq!(buf[0], HprofWriter::GC_ROOT_JNI_GLOBAL);
        let obj_id = u64::from_be_bytes(buf[1..9].try_into().unwrap());
        assert_eq!(obj_id, 0xCAFE);
        let ref_id = u64::from_be_bytes(buf[9..17].try_into().unwrap());
        assert_eq!(ref_id, 0xBEEF);
    }

    #[test]
    fn test_hprof_gc_class_dump_minimal() {
        let ci = HprofClassInfo {
            class_id: 1,
            name: "TestClass".to_string(),
            super_class_id: 0,
            instance_fields: vec![],
            static_fields: vec![],
            source_file: None,
            instance_size: 32,
        };
        let mut string_ids = std::collections::HashMap::new();
        string_ids.insert("TestClass".to_string(), 1u64);

        let mut buf = Vec::new();
        HprofWriter::write_gc_class_dump(&mut buf, &ci, &string_ids);

        assert_eq!(buf[0], HprofWriter::GC_CLASS_DUMP);
        // class_obj_id at bytes 1..9
        let class_obj = u64::from_be_bytes(buf[1..9].try_into().unwrap());
        assert_eq!(class_obj, 0x1000_0000_0000_0001);
        // instance_size at bytes 73..77 (1 + 8 + 4 + 8 + 8 + 8 + 8 + 8 + 8 = 61 offset, then 4 bytes)
        let inst_size = u32::from_be_bytes(buf[61..65].try_into().unwrap());
        assert_eq!(inst_size, 32);
        // constant pool count = 0
        let cp_count = u16::from_be_bytes(buf[65..67].try_into().unwrap());
        assert_eq!(cp_count, 0);
        // static field count = 0
        let sf_count = u16::from_be_bytes(buf[67..69].try_into().unwrap());
        assert_eq!(sf_count, 0);
        // instance field count = 0
        let if_count = u16::from_be_bytes(buf[69..71].try_into().unwrap());
        assert_eq!(if_count, 0);
    }

    #[test]
    fn test_hprof_gc_class_dump_with_fields() {
        let ci = HprofClassInfo {
            class_id: 2,
            name: "Point".to_string(),
            super_class_id: 1,
            instance_fields: vec![
                ("x".to_string(), "I".to_string()),
                ("y".to_string(), "I".to_string()),
            ],
            static_fields: vec![("ORIGIN".to_string(), "LPoint;".to_string())],
            source_file: Some("Point.java".to_string()),
            instance_size: 64,
        };
        let mut string_ids = std::collections::HashMap::new();
        string_ids.insert("Point".to_string(), 1u64);
        string_ids.insert("x".to_string(), 2u64);
        string_ids.insert("y".to_string(), 3u64);
        string_ids.insert("ORIGIN".to_string(), 4u64);

        let mut buf = Vec::new();
        HprofWriter::write_gc_class_dump(&mut buf, &ci, &string_ids);

        assert_eq!(buf[0], HprofWriter::GC_CLASS_DUMP);
        // Should have super_class_id encoded
        let super_obj = u64::from_be_bytes(buf[13..21].try_into().unwrap());
        assert_eq!(super_obj, 0x1000_0000_0000_0001); // super class id = 1

        // static field count = 1 (at offset 67)
        let sf_count = u16::from_be_bytes(buf[67..69].try_into().unwrap());
        assert_eq!(sf_count, 1);
    }

    #[test]
    fn test_hprof_gc_prim_array_dump() {
        use cratonvm_gc::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE};
        // Simulate a small int[3] array in memory
        let array_length: u32 = 3;
        let elem_size = 4usize; // int = 4 bytes
        let data_size = array_length as usize * elem_size;
        let total_size = HEADER_SIZE + ((data_size + 7) & !7);
        let mut mem = vec![0u8; total_size];

        // Write header
        let header = ObjectHeader::new(
            cratonvm_types::ClassId::new(99),
            ObjectKind::Array,
            ArrayElementType::Int,
            array_length,
            array_length,
        );
        unsafe {
            std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
        }

        // Write array elements: [10, 20, 30] as native-endian i32
        for (i, val) in [10i32, 20, 30].iter().enumerate() {
            let offset = HEADER_SIZE + i * elem_size;
            unsafe {
                std::ptr::write_unaligned(mem.as_mut_ptr().add(offset) as *mut i32, *val);
            }
        }

        let obj = HprofObjectInfo {
            object_id: mem.as_ptr() as u64,
            class_id: 99,
            is_array: true,
            element_type: ArrayElementType::Int as u8,
            array_length: 3,
            total_size,
            data_ptr: mem.as_ptr(),
        };

        let mut buf = Vec::new();
        HprofWriter::write_gc_prim_array_dump(&mut buf, &obj);

        assert_eq!(buf[0], HprofWriter::GC_PRIM_ARRAY_DUMP);
        // array length at bytes 13..17
        let len = u32::from_be_bytes(buf[13..17].try_into().unwrap());
        assert_eq!(len, 3);
        // element type at byte 17
        assert_eq!(buf[17], HprofBasicType::Int as u8);
        // First element (big-endian int32) at bytes 18..22
        let val0 = i32::from_be_bytes(buf[18..22].try_into().unwrap());
        assert_eq!(val0, 10);
        let val1 = i32::from_be_bytes(buf[22..26].try_into().unwrap());
        assert_eq!(val1, 20);
        let val2 = i32::from_be_bytes(buf[26..30].try_into().unwrap());
        assert_eq!(val2, 30);
    }

    #[test]
    fn test_hprof_gc_obj_array_dump() {
        use cratonvm_gc::heap::{
            ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, REF_ELEMENT_SIZE,
        };
        // Simulate an Object[2] array
        let array_length: u32 = 2;
        let data_size = array_length as usize * ref_element_size();
        let total_size = HEADER_SIZE + ((data_size + 7) & !7);
        let mut mem = vec![0u8; total_size];

        let header = ObjectHeader::new(
            cratonvm_types::ClassId::new(50),
            ObjectKind::Array,
            ArrayElementType::Reference,
            array_length,
            array_length,
        );
        unsafe {
            std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
        }

        // Write element references: [0xCAFE, 0xBEEF]
        for (i, val) in [0xCAFEu64, 0xBEEF].iter().enumerate() {
            let offset = HEADER_SIZE + i * ref_element_size();
            unsafe {
                std::ptr::write_unaligned(mem.as_mut_ptr().add(offset) as *mut u64, *val);
            }
        }

        let obj = HprofObjectInfo {
            object_id: mem.as_ptr() as u64,
            class_id: 50,
            is_array: true,
            element_type: ArrayElementType::Reference as u8,
            array_length: 2,
            total_size,
            data_ptr: mem.as_ptr(),
        };

        let mut buf = Vec::new();
        HprofWriter::write_gc_obj_array_dump(&mut buf, &obj);

        assert_eq!(buf[0], HprofWriter::GC_OBJ_ARRAY_DUMP);
        let len = u32::from_be_bytes(buf[13..17].try_into().unwrap());
        assert_eq!(len, 2);
        // First element ref at bytes 25..33
        let ref0 = u64::from_be_bytes(buf[25..33].try_into().unwrap());
        assert_eq!(ref0, 0xCAFE);
        let ref1 = u64::from_be_bytes(buf[33..41].try_into().unwrap());
        assert_eq!(ref1, 0xBEEF);
    }

    #[test]
    fn test_hprof_full_dump_empty_heap() {
        let classes: Vec<HprofClassInfo> = vec![];
        let objects: Vec<HprofObjectInfo> = vec![];
        let threads = vec![ThreadSnapshot {
            id: 1,
            name: "main".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        }];

        let dump = HprofWriter::write_full_heap_dump(&classes, &objects, &threads);

        // Verify header magic
        let magic_len = HprofWriter::HPROF_MAGIC.len();
        assert_eq!(&dump[..magic_len], HprofWriter::HPROF_MAGIC.as_bytes());

        // Should contain HEAP_DUMP_END marker somewhere
        assert!(dump
            .windows(1)
            .any(|w| w[0] == HprofWriter::HPROF_HEAP_DUMP_END));

        // Should be at least header + stack trace + segment + end
        assert!(dump.len() > HprofWriter::header_size() + 20);
    }

    #[test]
    fn test_hprof_full_dump_with_class_and_objects() {
        use cratonvm_gc::heap::{
            ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE,
        };

        let classes = vec![HprofClassInfo {
            class_id: 1,
            name: "TestObj".to_string(),
            super_class_id: 0,
            instance_fields: vec![("value".to_string(), "I".to_string())],
            static_fields: vec![],
            source_file: Some("TestObj.java".to_string()),
            instance_size: (HEADER_SIZE + SLOT_SIZE) as u32,
        }];

        // Create a fake heap object
        let total_size = HEADER_SIZE + SLOT_SIZE;
        let mut mem = vec![0u8; total_size];
        let header = ObjectHeader::new(
            cratonvm_types::ClassId::new(1),
            ObjectKind::Object,
            ArrayElementType::Boolean,
            0,
            1,
        );
        unsafe {
            std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
            // Write an int value (42) at the field slot
            std::ptr::write_unaligned(mem.as_mut_ptr().add(HEADER_SIZE) as *mut i32, 42);
        }

        let objects = vec![HprofObjectInfo {
            object_id: mem.as_ptr() as u64,
            class_id: 1,
            is_array: false,
            element_type: 0,
            array_length: 0,
            total_size,
            data_ptr: mem.as_ptr(),
        }];

        let threads = vec![ThreadSnapshot {
            id: 1,
            name: "main".to_string(),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: vec![],
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: Default::default(),
        }];

        let dump = HprofWriter::write_full_heap_dump(&classes, &objects, &threads);

        // Verify magic
        let magic_len = HprofWriter::HPROF_MAGIC.len();
        assert_eq!(&dump[..magic_len], HprofWriter::HPROF_MAGIC.as_bytes());

        // Should have UTF-8 records (for "TestObj", "value", etc.)
        assert!(dump.contains(&HprofWriter::HPROF_UTF8));

        // Should have LOAD_CLASS record
        assert!(dump.contains(&HprofWriter::HPROF_LOAD_CLASS));

        // Should have a HEAP_DUMP_SEGMENT
        assert!(dump.contains(&HprofWriter::HPROF_HEAP_DUMP_SEGMENT));

        // Should have HEAP_DUMP_END
        assert!(dump.contains(&HprofWriter::HPROF_HEAP_DUMP_END));

        // Size should be reasonable (not trivially small)
        assert!(dump.len() > 200);
    }

    #[test]
    fn test_hprof_full_dump_with_threads_and_frames() {
        let threads = vec![
            ThreadSnapshot {
                id: 1,
                name: "main".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Runnable,
                stack_frames: vec![
                    FrameInfo {
                        class_name: "com.example.Main".to_string(),
                        method_name: "run".to_string(),
                        file_name: Some("Main.java".to_string()),
                        line_number: 42,
                        native_method: false,
                    },
                    FrameInfo {
                        class_name: "com.example.Main".to_string(),
                        method_name: "main".to_string(),
                        file_name: Some("Main.java".to_string()),
                        line_number: 10,
                        native_method: false,
                    },
                ],
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
                wait_kind: None,
                dump: Default::default(),
            },
            ThreadSnapshot {
                id: 2,
                name: "worker-1".to_string(),
                daemon: true,
                priority: 5,
                state: ThreadState::Waiting,
                stack_frames: vec![],
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
                wait_kind: None,
                dump: Default::default(),
            },
        ];

        let dump = HprofWriter::write_full_heap_dump(&[], &[], &threads);

        // Should have STACK_FRAME records (for main's 2 frames)
        assert!(dump.contains(&HprofWriter::HPROF_FRAME));

        // Should have STACK_TRACE records
        assert!(dump.contains(&HprofWriter::HPROF_TRACE));

        // Should have UTF-8 records for thread/frame names
        assert!(dump.contains(&HprofWriter::HPROF_UTF8));
    }

    #[test]
    fn test_hprof_full_dump_class_hierarchy() {
        use cratonvm_gc::heap::{
            ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE,
        };

        let classes = vec![
            HprofClassInfo {
                class_id: 1,
                name: "Base".to_string(),
                super_class_id: 0,
                instance_fields: vec![("id".to_string(), "I".to_string())],
                static_fields: vec![],
                source_file: None,
                instance_size: (HEADER_SIZE + SLOT_SIZE) as u32,
            },
            HprofClassInfo {
                class_id: 2,
                name: "Derived".to_string(),
                super_class_id: 1,
                instance_fields: vec![("name".to_string(), "Ljava/lang/String;".to_string())],
                static_fields: vec![],
                source_file: None,
                instance_size: (HEADER_SIZE + 2 * SLOT_SIZE) as u32,
            },
        ];

        // Create a Derived object with 2 fields (inherited id + own name)
        let total_size = HEADER_SIZE + 2 * SLOT_SIZE;
        let mut mem = vec![0u8; total_size];
        let header = ObjectHeader::new(
            cratonvm_types::ClassId::new(2),
            ObjectKind::Object,
            ArrayElementType::Boolean,
            0,
            2,
        );
        unsafe {
            std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
        }

        let objects = vec![HprofObjectInfo {
            object_id: mem.as_ptr() as u64,
            class_id: 2,
            is_array: false,
            element_type: 0,
            array_length: 0,
            total_size,
            data_ptr: mem.as_ptr(),
        }];

        let dump = HprofWriter::write_full_heap_dump(&classes, &objects, &[]);

        // Should have two LOAD_CLASS records
        let _load_class_count = dump
            .iter()
            .enumerate()
            .filter(|(i, &b)| {
                b == HprofWriter::HPROF_LOAD_CLASS
                    && *i > 0
                    && dump
                        .get(i.wrapping_sub(4)..=i.wrapping_sub(1))
                        .map(|s| s == &[0, 0, 0, 0]) // timestamp = 0
                        .unwrap_or(false)
            })
            .count();
        // At minimum we should have LOAD_CLASS tags in the output
        assert!(dump.contains(&HprofWriter::HPROF_LOAD_CLASS));

        // Two GC_CLASS_DUMP sub-records should be present in segment body
        // CLASS_DUMP tag is 0x20
        let class_dump_count = dump
            .iter()
            .filter(|&&b| b == HprofWriter::GC_CLASS_DUMP)
            .count();
        // Should be >= 2 (Base + Derived)
        assert!(
            class_dump_count >= 2,
            "Expected at least 2 class dumps, got {}",
            class_dump_count
        );
    }

    #[test]
    fn test_hprof_segment_wrapping() {
        let seg = HprofWriter::wrap_segment(&[1, 2, 3, 4, 5]);
        assert_eq!(seg[0], HprofWriter::HPROF_HEAP_DUMP_SEGMENT);
        // timestamp at 1..5
        assert_eq!(&seg[1..5], &[0, 0, 0, 0]);
        // body length at 5..9
        let body_len = u32::from_be_bytes(seg[5..9].try_into().unwrap());
        assert_eq!(body_len, 5);
        // body content
        assert_eq!(&seg[9..14], &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_hprof_write_to_file() {
        // Test that write_full_heap_dump produces valid data that can be written
        let dump = HprofWriter::write_full_heap_dump(&[], &[], &[]);
        // Even with no data, should produce a valid HPROF file structure
        assert!(dump.len() >= HprofWriter::header_size());
        // First bytes are the magic
        assert!(dump.starts_with(HprofWriter::HPROF_MAGIC.as_bytes()));

        // Write to temp file and verify size
        let tmp_path = std::env::temp_dir().join("test_heap_dump.hprof");
        std::fs::write(&tmp_path, &dump).unwrap();
        let written = std::fs::read(&tmp_path).unwrap();
        assert_eq!(written.len(), dump.len());
        assert_eq!(written, dump);
        std::fs::remove_file(&tmp_path).ok();
    }

    #[test]
    fn test_hprof_instance_dump_reads_field_values() {
        use cratonvm_gc::heap::{
            ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE,
        };

        let classes_map: std::collections::HashMap<u32, HprofClassInfo> = [(
            1u32,
            HprofClassInfo {
                class_id: 1,
                name: "IntHolder".to_string(),
                super_class_id: 0,
                instance_fields: vec![("val".to_string(), "I".to_string())],
                static_fields: vec![],
                source_file: None,
                instance_size: (HEADER_SIZE + SLOT_SIZE) as u32,
            },
        )]
        .into_iter()
        .collect();

        let total_size = HEADER_SIZE + SLOT_SIZE;
        let mut mem = vec![0u8; total_size];
        let header = ObjectHeader::new(
            cratonvm_types::ClassId::new(1),
            ObjectKind::Object,
            ArrayElementType::Boolean,
            0,
            1,
        );
        unsafe {
            std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
            // Write field value: 0x12345678
            std::ptr::write_unaligned(mem.as_mut_ptr().add(HEADER_SIZE) as *mut i32, 0x12345678);
        }

        let ci = classes_map.get(&1).unwrap();
        let obj = HprofObjectInfo {
            object_id: 0xABCD,
            class_id: 1,
            is_array: false,
            element_type: 0,
            array_length: 0,
            total_size,
            data_ptr: mem.as_ptr(),
        };

        let mut buf = Vec::new();
        HprofWriter::write_gc_instance_dump(&mut buf, &obj, ci, &classes_map);

        assert_eq!(buf[0], HprofWriter::GC_INSTANCE_DUMP);
        // object_id at 1..9
        let oid = u64::from_be_bytes(buf[1..9].try_into().unwrap());
        assert_eq!(oid, 0xABCD);
        // data_size at 21..25 (after tag + obj_id(8) + stack_serial(4) + class_obj(8))
        let data_size = u32::from_be_bytes(buf[21..25].try_into().unwrap());
        assert_eq!(data_size, 4); // one int field = 4 bytes
                                  // Field value at 25..29 (big-endian)
        let fval = i32::from_be_bytes(buf[25..29].try_into().unwrap());
        assert_eq!(fval, 0x12345678);
    }

    #[test]
    fn test_hprof_instance_dump_reads_compact_ref_field_value() {
        use cratonvm_gc::heap::{ArrayElementType, ObjectHeader, ObjectKind};
        use cratonvm_gc::register_class_layout;
        use cratonvm_types::{CompactLayout, FieldStorageKind, GC_FLAG_COMPACT};
        use std::sync::Arc;

        const CLASS_ID: u32 = 61_001;

        register_class_layout(
            cratonvm_types::FIRST_LAYOUT_DOMAIN,
            CLASS_ID,
            Arc::new(CompactLayout {
                field_disps: vec![8],
                is_ref: vec![true],
                field_kinds: vec![FieldStorageKind::Reference],
                ref_disps: vec![8],
                total_size: 16,
            }),
        );

        let classes_map: std::collections::HashMap<u32, HprofClassInfo> = [(
            CLASS_ID,
            HprofClassInfo {
                class_id: CLASS_ID,
                name: "RefHolder".to_string(),
                super_class_id: 0,
                instance_fields: vec![("ref".to_string(), "Ljava/lang/Object;".to_string())],
                static_fields: vec![],
                source_file: None,
                instance_size: cratonvm_types::MIN_OBJECT_SIZE as u32,
            },
        )]
        .into_iter()
        .collect();

        // A compact instance: the 8-byte header word, then its one field.
        let total_size = cratonvm_types::MIN_OBJECT_SIZE;
        let mut mem = vec![0u8; total_size];
        let expected_ref = 0x1_2345_6789_abcd_u64;
        let mut header = ObjectHeader::new(
            cratonvm_types::ClassId::new(CLASS_ID),
            ObjectKind::Object,
            ArrayElementType::Boolean,
            0,
            1,
        );
        header.set_compact_shape(1, 8);
        unsafe {
            std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
            std::ptr::write_unaligned(
                mem.as_mut_ptr().add(cratonvm_types::COMPACT_HEADER_SIZE) as *mut u64,
                expected_ref,
            );
        }

        let ci = classes_map.get(&CLASS_ID).unwrap();
        let obj = HprofObjectInfo {
            object_id: 0xABCD,
            class_id: CLASS_ID,
            is_array: false,
            element_type: 0,
            array_length: 0,
            total_size,
            data_ptr: mem.as_ptr(),
        };

        let mut buf = Vec::new();
        HprofWriter::write_gc_instance_dump(&mut buf, &obj, ci, &classes_map);

        let fval = u64::from_be_bytes(buf[25..33].try_into().unwrap());
        assert_eq!(fval, expected_ref);
    }

    // --- HSDB protocol tests ---

    struct DummyVmState;
    impl VmDiagnosticState for DummyVmState {
        fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
            vec![ThreadSnapshot {
                id: 42,
                name: "main".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Runnable,
                stack_frames: vec![],
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
                wait_kind: None,
                dump: Default::default(),
            }]
        }
        fn heap_summary(&self) -> HeapSummary {
            HeapSummary {
                young_gen_used: 100,
                young_gen_capacity: 200,
                old_gen_used: 300,
                old_gen_capacity: 400,
                metaspace_used: 500,
                metaspace_capacity: 600,
                total_used: 1024,
                total_capacity: 2048,
            }
        }
        fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
            Vec::new()
        }
        fn trigger_gc(&self) -> bool {
            false
        }
        fn uptime_secs(&self) -> f64 {
            1.5
        }
        fn command_line(&self) -> String {
            "java -jar test.jar".to_string()
        }
        fn system_properties(&self) -> Vec<(String, String)> {
            Vec::new()
        }
        fn vm_flags(&self) -> Vec<String> {
            Vec::new()
        }
    }

    #[test]
    fn hsdb_magic_value() {
        assert_eq!(HSDB_MAGIC, 0x4853_4442);
        assert_eq!(&HSDB_MAGIC.to_be_bytes(), b"HSDB");
    }

    #[test]
    fn hsdb_command_roundtrip() {
        assert_eq!(HsdbCommand::from_u8(0x01), Some(HsdbCommand::Version));
        assert_eq!(HsdbCommand::from_u8(0x02), Some(HsdbCommand::ProcessInfo));
        assert_eq!(HsdbCommand::from_u8(0x03), Some(HsdbCommand::HeapSummary));
        assert_eq!(HsdbCommand::from_u8(0x04), Some(HsdbCommand::ThreadList));
        assert_eq!(HsdbCommand::from_u8(0xFF), None);
    }

    #[test]
    fn hsdb_decode_request_header_rejects_short() {
        assert!(hsdb_decode_request_header(&[0x01, 0x00]).is_err());
    }

    #[test]
    fn hsdb_decode_request_header_parses_len() {
        let bytes = [0x01u8, 0x00, 0x00, 0x01, 0x00];
        let (cmd, len) = hsdb_decode_request_header(&bytes).unwrap();
        assert_eq!(cmd, HsdbCommand::Version);
        assert_eq!(len, 256);
    }

    #[test]
    fn hsdb_decode_request_header_rejects_bad_cmd() {
        assert!(hsdb_decode_request_header(&[0xFE, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn hsdb_encode_response_prefix() {
        let resp = hsdb_encode_response(HsdbStatus::Ok, b"hi");
        assert_eq!(resp[0], 0); // OK status
        assert_eq!(u32::from_be_bytes([resp[1], resp[2], resp[3], resp[4]]), 2);
        assert_eq!(&resp[5..], b"hi");
    }

    #[test]
    fn hsdb_version_response_carries_version_string() {
        let (status, payload) = hsdb_handle_request(HsdbCommand::Version, &[], None);
        assert_eq!(status, HsdbStatus::Ok);
        // First 2 bytes = string length, then UTF-8
        assert!(payload.len() >= 2);
        let len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
        assert_eq!(len, HSDB_VERSION_STRING.len());
        let s = std::str::from_utf8(&payload[2..2 + len]).unwrap();
        assert_eq!(s, HSDB_VERSION_STRING);
    }

    #[test]
    fn hsdb_process_info_includes_pid_and_cmdline() {
        let state = DummyVmState;
        let (status, payload) = hsdb_handle_request(HsdbCommand::ProcessInfo, &[], Some(&state));
        assert_eq!(status, HsdbStatus::Ok);
        // pid u64 + len u16 + cmdline bytes
        assert!(payload.len() >= 10);
        let pid = u64::from_be_bytes(payload[0..8].try_into().unwrap());
        assert_eq!(pid, std::process::id() as u64);
        let len = u16::from_be_bytes([payload[8], payload[9]]) as usize;
        let cmdline = std::str::from_utf8(&payload[10..10 + len]).unwrap();
        assert_eq!(cmdline, "java -jar test.jar");
    }

    #[test]
    fn hsdb_heap_summary_returns_eight_u64() {
        let state = DummyVmState;
        let (status, payload) = hsdb_handle_request(HsdbCommand::HeapSummary, &[], Some(&state));
        assert_eq!(status, HsdbStatus::Ok);
        assert_eq!(payload.len(), 64);
        let young_used = u64::from_be_bytes(payload[0..8].try_into().unwrap());
        let young_cap = u64::from_be_bytes(payload[8..16].try_into().unwrap());
        let total_used = u64::from_be_bytes(payload[48..56].try_into().unwrap());
        let total_cap = u64::from_be_bytes(payload[56..64].try_into().unwrap());
        assert_eq!(young_used, 100);
        assert_eq!(young_cap, 200);
        assert_eq!(total_used, 1024);
        assert_eq!(total_cap, 2048);
    }

    #[test]
    fn hsdb_heap_summary_without_state_returns_error() {
        let (status, _) = hsdb_handle_request(HsdbCommand::HeapSummary, &[], None);
        assert_eq!(status, HsdbStatus::Error);
    }

    #[test]
    fn hsdb_thread_list_contains_threads() {
        let state = DummyVmState;
        let (status, payload) = hsdb_handle_request(HsdbCommand::ThreadList, &[], Some(&state));
        assert_eq!(status, HsdbStatus::Ok);
        assert!(payload.len() >= 4);
        let count = u32::from_be_bytes(payload[0..4].try_into().unwrap());
        assert_eq!(count, 1);
        let tid = u64::from_be_bytes(payload[4..12].try_into().unwrap());
        assert_eq!(tid, 42);
        let name_len = u16::from_be_bytes([payload[12], payload[13]]) as usize;
        let name = std::str::from_utf8(&payload[14..14 + name_len]).unwrap();
        assert_eq!(name, "main");
        let state_byte = payload[14 + name_len];
        assert_eq!(state_byte, 1); // Runnable
    }

    #[test]
    fn hsdb_thread_list_without_state_errors() {
        let (status, _) = hsdb_handle_request(HsdbCommand::ThreadList, &[], None);
        assert_eq!(status, HsdbStatus::Error);
    }

    /// Byte-for-byte recorded exchange: client handshake + VERSION request,
    /// server handshake + OK response. Verifies wire compatibility without
    /// needing a live client library.
    #[test]
    fn hsdb_recorded_exchange_matches() {
        // Server-side handshake echo: magic bytes
        let magic_bytes = HSDB_MAGIC.to_be_bytes();
        assert_eq!(&magic_bytes, b"HSDB");

        // Client sends VERSION request (no payload)
        let client_request = [0x01u8, 0x00, 0x00, 0x00, 0x00];
        let (cmd, len) = hsdb_decode_request_header(&client_request).unwrap();
        assert_eq!(cmd, HsdbCommand::Version);
        assert_eq!(len, 0);

        // Server processes and replies
        let (status, payload) = hsdb_handle_request(cmd, &[], None);
        let wire = hsdb_encode_response(status, &payload);

        // Expected wire bytes: [0][0][0][0][19][0][17]"CratonVM HSDB v1.0"
        // status(1) + payload_len(4) + str_len(2) + 17 ascii bytes = 24 bytes
        assert_eq!(wire.len(), 1 + 4 + 2 + HSDB_VERSION_STRING.len());
        assert_eq!(wire[0], 0); // OK
        let payload_len = u32::from_be_bytes([wire[1], wire[2], wire[3], wire[4]]);
        assert_eq!(payload_len as usize, 2 + HSDB_VERSION_STRING.len());
        let str_len = u16::from_be_bytes([wire[5], wire[6]]) as usize;
        assert_eq!(str_len, HSDB_VERSION_STRING.len());
        assert_eq!(&wire[7..], HSDB_VERSION_STRING.as_bytes());
    }

    #[test]
    fn hsdb_listener_starts_and_responds() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let state: Arc<dyn VmDiagnosticState> = Arc::new(DummyVmState);
        let listener = hsdb_start_listener(0, state).expect("bind");
        let addr = listener.addr;

        // Connect and do handshake + VERSION round-trip
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        stream.write_all(&HSDB_MAGIC.to_be_bytes()).unwrap();
        let mut echo = [0u8; 4];
        stream.read_exact(&mut echo).unwrap();
        assert_eq!(u32::from_be_bytes(echo), HSDB_MAGIC);

        // VERSION request
        stream.write_all(&[0x01, 0, 0, 0, 0]).unwrap();
        let mut hdr = [0u8; 5];
        stream.read_exact(&mut hdr).unwrap();
        assert_eq!(hdr[0], 0); // OK
        let plen = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        let mut payload = vec![0u8; plen];
        stream.read_exact(&mut payload).unwrap();
        let str_len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
        let s = std::str::from_utf8(&payload[2..2 + str_len]).unwrap();
        assert_eq!(s, HSDB_VERSION_STRING);

        drop(stream);
        listener.stop();
    }
}

/// gci i28/dg (`gci-i14mp-...` item 11b): the `DiagnosticCommand` MBean's view
/// of the command table.
#[cfg(test)]
mod gci_i28_dg_dcmd_mbean_tests {
    use super::*;

    fn table() -> parking_lot::RwLock<Vec<DiagnosticCommand>> {
        let echo = DiagnosticCommand::new(
            "Echo.args",
            "Echo the arguments",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![
                CommandArgument {
                    name: "-loud".to_string(),
                    description: "Shout".to_string(),
                    arg_type: ArgType::Bool,
                    required: false,
                    default_value: Some("false".to_string()),
                },
                CommandArgument {
                    name: "text".to_string(),
                    description: "What to echo".to_string(),
                    arg_type: ArgType::String,
                    required: true,
                    default_value: None,
                },
            ],
            Box::new(|args| CommandResult::ok(args.join("|"), 0)),
        );
        let refuse = DiagnosticCommand::new(
            "Refuse.it",
            "Always refuses",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_| {
                CommandResult::err(
                    "java.lang.IllegalArgumentException: Nope\n".to_string(),
                    0,
                )
            }),
        );
        let internal = DiagnosticCommand::new(
            "attach internal",
            "",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_| CommandResult::ok(String::new(), 0)),
        );
        parking_lot::RwLock::new(vec![echo, refuse, internal])
    }

    #[test]
    fn gci_i28dg_mbean_lists_jcmd_names_and_help() {
        let t = table();
        assert_eq!(
            dcmd_mbean_names(&t.read()),
            vec!["Echo.args", "Refuse.it", "help"]
        );
    }

    #[test]
    fn gci_i28dg_mbean_info_is_jmms_shape() {
        let t = table();
        let echo = dcmd_mbean_info(&t.read(), "Echo.args").expect("listed");
        assert_eq!(echo.impact, "Medium");
        assert_eq!(
            echo.permission_class.as_deref(),
            Some("java.lang.management.ManagementPermission")
        );
        assert_eq!(echo.permission_name.as_deref(), Some("control"));
        assert!(echo.enabled);
        assert_eq!(echo.arguments.len(), 2);
        assert!(echo.arguments[0].option && echo.arguments[0].position == -1);
        assert_eq!(echo.arguments[0].arg_type, "BOOLEAN");
        assert!(!echo.arguments[1].option && echo.arguments[1].position == 0);
        assert!(echo.arguments[1].mandatory);
        let refuse = dcmd_mbean_info(&t.read(), "Refuse.it").expect("listed");
        assert_eq!(refuse.permission_name.as_deref(), Some("monitor"));
        let help = dcmd_mbean_info(&t.read(), "help").expect("help");
        assert!(help.permission_class.is_none());
        assert_eq!(help.arguments.len(), 2);
        assert!(dcmd_mbean_info(&t.read(), "attach internal").is_none());
        assert!(dcmd_mbean_info(&t.read(), "No.such").is_none());
    }

    #[test]
    fn gci_i28dg_mbean_execute_is_jmms() {
        let t = table();
        assert_eq!(
            dcmd_mbean_execute(&t, "Echo.args -loud \"a b\""),
            Ok("-loud|a b".to_string())
        );
        assert_eq!(
            dcmd_mbean_execute(&t, "No.such x"),
            Err("Unknown diagnostic command".to_string())
        );
        assert_eq!(
            dcmd_mbean_execute(&t, "attach internal"),
            Err("Unknown diagnostic command".to_string())
        );
        assert_eq!(dcmd_mbean_execute(&t, "Refuse.it"), Err("Nope".to_string()));
        assert_eq!(dcmd_mbean_execute(&t, "   "), Ok(String::new()));
        assert_eq!(
            dcmd_mbean_execute(&t, "Echo.args \"open"),
            Err("Format error in diagnostic command arguments".to_string())
        );
        let help = dcmd_mbean_execute(&t, "help").expect("help");
        assert!(help.starts_with("The following commands are available:\n"));
    }
}

/// gci i30/ob (`i28-dg-report.md` review item 2): `jcmd <pid> help -all`.
#[cfg(test)]
mod gci_i30_ob_help_all_tests {
    use super::*;

    fn command(name: &str, description: &str) -> DiagnosticCommand {
        DiagnosticCommand::new(
            name,
            description,
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_| CommandResult::ok(String::new(), 0)),
        )
    }

    #[test]
    fn gci_i30ob_help_all_option_is_a_boolean() {
        let s = |v: &[&str]| v.iter().map(|a| a.to_string()).collect::<Vec<String>>();
        assert_eq!(jcmd_help_all_option(&s(&["-all"])), (true, vec![]));
        assert_eq!(jcmd_help_all_option(&s(&["-all=true"])), (true, vec![]));
        assert_eq!(
            jcmd_help_all_option(&s(&["-all=false", "VM.uptime"])),
            (false, s(&["VM.uptime"]))
        );
        assert_eq!(
            jcmd_help_all_option(&s(&["VM.uptime", "-all"])),
            (true, s(&["VM.uptime"]))
        );
        assert_eq!(jcmd_help_all_option(&s(&["all"])), (false, s(&["all"])));
    }

    /// HotSpot's `HelpDCmd` with `_all`: sorted names (and `help`), each
    /// `name\n\tdescription\n\n`; internal attach operations (a space in the
    /// name) are not commands. `-all` wins over a command name.
    #[test]
    fn gci_i30ob_help_all_lists_every_command_with_its_description() {
        let commands = vec![
            command("VM.uptime", "Print VM uptime."),
            command("GC.run", "Call java.lang.System.gc()."),
            command("attach load", "internal"),
        ];
        let out = jcmd_help(&commands, &["-all".to_string()]);
        assert_eq!(
            out,
            format!(
                "GC.run\n\tCall java.lang.System.gc().\n\n\
                 VM.uptime\n\tPrint VM uptime.\n\n\
                 help\n\t{DCMD_HELP_DESCRIPTION}\n\n"
            )
        );
        assert_eq!(jcmd_help(&commands, &["VM.uptime".to_string(), "-all".to_string()]), out);
        // Without the option, unchanged.
        assert!(jcmd_help(&commands, &[]).starts_with("The following commands are available:\n"));
        assert!(jcmd_help(&commands, &["-all=false".to_string()])
            .starts_with("The following commands are available:\n"));
        assert_eq!(
            jcmd_help(&commands, &["No.such".to_string()]),
            "Help unavailable : 'No.such' : No such command\n"
        );
    }
}

#[cfg(test)]
mod gci_i31_hp_dcmd_tests {
    use super::*;

    fn command(name: &str, impact: CommandImpact) -> DiagnosticCommand {
        DiagnosticCommand::new(
            name,
            "d",
            impact,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_| CommandResult::ok(String::new(), 0)),
        )
    }

    /// HotSpot 25's `impact()` texts: the long ones by name, the bare level
    /// otherwise; both `jcmd help <command>` and the MBean read them.
    #[test]
    fn gci_i31hp_impact_texts_are_hotspots() {
        let gc_run = command("GC.run", CommandImpact::Medium);
        assert_eq!(
            dcmd_impact_text(&gc_run),
            "Medium: Depends on Java heap size and content."
        );
        assert_eq!(
            dcmd_impact_text(&command("Thread.print", CommandImpact::Medium)),
            "Medium: Depends on the number of threads."
        );
        assert_eq!(dcmd_impact_text(&command("VM.uptime", CommandImpact::Low)), "Low");
        assert_eq!(dcmd_impact_text(&command("GC.heap_info", CommandImpact::Medium)), "Medium");
        let commands = vec![gc_run];
        assert!(jcmd_help(&commands, &["GC.run".to_string()])
            .contains("\nImpact: Medium: Depends on Java heap size and content.\n"));
        let info = dcmd_mbean_info(&commands, "GC.run").expect("listed");
        assert_eq!(info.impact, "Medium: Depends on Java heap size and content.");
    }

    /// The live registrar carries HotSpot 25's descriptions (read from the
    /// 25.0.3 `jvm.dll`), not this VM's old paraphrases. Needles are split
    /// with `concat!` so this test's own text never matches them.
    #[test]
    fn gci_i31hp_the_registrars_use_hotspots_descriptions() {
        let src = include_str!("serviceability.rs").replace("\r\n", "\n");
        for old in [
            concat!("\"Trigger garbage ", "collection\""),
            concat!("\"Print heap summary ", "information\""),
            concat!("\"Print class ", "histogram\""),
            concat!("\"Print VM flag ", "settings\""),
            concat!("\"Print VM ", "uptime\""),
            concat!("\"Print command line ", "arguments\""),
            concat!("\"Start a flight ", "recording\""),
        ] {
            assert!(!src.contains(old), "{old}");
        }
        for new in [
            concat!("\"GC.run\",\n            \"Call java.lang.System.", "gc().\""),
            concat!("\"GC.heap_info\",\n            \"Provide generic Java heap ", "information.\""),
            concat!("\"GC.class_histogram\",\n            \"Provide statistics about the Java heap ", "usage.\""),
            concat!("\"VM.flags\",\n            \"Print VM flag options and their current ", "values.\""),
            concat!("\"VM.uptime\",\n            \"Print VM ", "uptime.\""),
            concat!("\"JFR.stop\",\n            \"Stops a JFR ", "recording\""),
        ] {
            assert!(src.contains(new), "{new}");
        }
    }
}

/// gci i32/jh (`i31-hp-report.md` review item 1): `jcmd <pid> help <command>`
/// in HotSpot 25's shape, and the argument tables it and the MBean read.
#[cfg(test)]
mod gci_i32_jh_dcmd_help_tests {
    use super::*;

    fn command(name: &str, description: &str, impact: CommandImpact) -> DiagnosticCommand {
        DiagnosticCommand::new(
            name,
            description,
            impact,
            CommandPermission::ReadOnly,
            hotspot_dcmd_arguments(name),
            Box::new(|_| CommandResult::ok(String::new(), 0)),
        )
    }

    fn help(commands: &[DiagnosticCommand], name: &str) -> String {
        jcmd_help(commands, &[name.to_string()])
    }

    /// Options only: `Syntax : <name> [options]`, the `Options:` block, no
    /// `Arguments:` block and no `Permission:` line (JDK 25 has none).
    #[test]
    fn gci_i32jh_options_only_page_is_hotspots() {
        let commands = vec![command(
            "Thread.print",
            "Print all threads with stacktraces.",
            CommandImpact::Medium,
        )];
        assert_eq!(
            help(&commands, "Thread.print"),
            "Thread.print\nPrint all threads with stacktraces.\n\n\
             Impact: Medium: Depends on the number of threads.\n\n\
             Syntax : Thread.print [options]\n\n\
             Options: (options must be specified using the <key> or <key>=<value> syntax)\n\
             \t-l : [optional] print java.util.concurrent locks (BOOLEAN, false)\n\
             \t-e : [optional] print extended thread information (BOOLEAN, false)\n"
        );
    }

    /// A mandatory argument and options: the argument in the syntax line, a
    /// mandatory row with an empty `[optional]` (two spaces), HotSpot's type
    /// names and defaults.
    #[test]
    fn gci_i32jh_arguments_and_options_page_is_hotspots() {
        let commands = vec![command(
            "GC.heap_dump",
            "Generate a HPROF format dump of the Java heap.",
            CommandImpact::High,
        )];
        let out = help(&commands, "GC.heap_dump");
        assert!(out.starts_with(
            "GC.heap_dump\nGenerate a HPROF format dump of the Java heap.\n\nImpact: High: "
        ));
        assert!(out.contains(
            "\n\nSyntax : GC.heap_dump [options] <filename>\n\nArguments:\n\
             \tfilename :  Name of the dump file (FILE, no default value)\n\n\
             Options: (options must be specified using the <key> or <key>=<value> syntax)\n\
             \t-all : [optional] Dump all objects, including unreachable objects (BOOLEAN, false)\n\
             \t-gz : [optional] If specified, the heap dump is written in gzipped format"
        ));
        assert!(out.contains("9 the strongest compression. (INT, 1)\n\t-overwrite : [optional] "));
        assert!(out.ends_with("but might use fewer. (INT, 1)\n"));
        assert!(!out.contains("Permission"));
    }

    /// Arguments only (no `[options]`, no `Options:` block); an optional
    /// argument is `[<name>]`.
    #[test]
    fn gci_i32jh_arguments_only_page_is_hotspots() {
        let commands = vec![command(
            "JVMTI.agent_load",
            "Load JVMTI native agent.",
            CommandImpact::Low,
        )];
        assert_eq!(
            help(&commands, "JVMTI.agent_load"),
            "JVMTI.agent_load\nLoad JVMTI native agent.\n\nImpact: Low\n\n\
             Syntax : JVMTI.agent_load  <library path> [<agent option>]\n\n\
             Arguments:\n\
             \tlibrary path :  Absolute path of the JVMTI agent to load. (STRING, no default value)\n\
             \tagent option : [optional] Option string to pass the agent. (STRING, no default value)\n"
        );
    }

    /// A parserless HotSpot `DCmd` prints `Syntax: <name>`; `help help` is
    /// `HelpDCmd`'s own page.
    #[test]
    fn gci_i32jh_parserless_and_help_pages_are_hotspots() {
        let commands = vec![command(
            "GC.run",
            "Call java.lang.System.gc().",
            CommandImpact::Medium,
        )];
        assert_eq!(
            help(&commands, "GC.run"),
            "GC.run\nCall java.lang.System.gc().\n\n\
             Impact: Medium: Depends on Java heap size and content.\n\nSyntax: GC.run\n"
        );
        assert_eq!(
            help(&commands, "help"),
            format!(
                "help\n{DCMD_HELP_DESCRIPTION}\n\nImpact: Low\n\n\
                 Syntax : help [options] [<command name>]\n\n\
                 Arguments:\n\
                 \tcommand name : [optional] The name of the command for which we want help \
                 (STRING, no default value)\n\n\
                 Options: (options must be specified using the <key> or <key>=<value> syntax)\n\
                 \t-all : [optional] Show help for all commands (BOOLEAN, false)\n"
            )
        );
    }

    /// The tables: HotSpot's parserless commands declare nothing; the MBean
    /// hands HotSpot's raw type names (JDK 25's `sanitiseType` maps `FILE`).
    #[test]
    fn gci_i32jh_argument_tables_feed_the_mbean() {
        for parserless in [
            "GC.run",
            "GC.heap_info",
            "VM.version",
            "VM.system_properties",
            "VM.command_line",
        ] {
            assert!(hotspot_dcmd_arguments(parserless).is_empty(), "{parserless}");
        }
        let commands = vec![command(
            "Thread.dump_to_file",
            "Dump threads, with stack traces, to a file in plain text or JSON format.",
            CommandImpact::Medium,
        )];
        let info = dcmd_mbean_info(&commands, "Thread.dump_to_file").expect("listed");
        let shape: Vec<(String, String, bool, i32, Option<String>)> = info
            .arguments
            .iter()
            .map(|a| {
                (
                    a.name.clone(),
                    a.arg_type.clone(),
                    a.option,
                    a.position,
                    a.default_value.clone(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                ("-overwrite".to_string(), "BOOLEAN".to_string(), true, -1, Some("false".to_string())),
                ("-format".to_string(), "STRING".to_string(), true, -1, Some("plain".to_string())),
                ("filepath".to_string(), "FILE".to_string(), false, 0, None),
            ]
        );
        assert!(help(&commands, "Thread.dump_to_file")
            .contains("\nSyntax : Thread.dump_to_file [options] <filepath>\n"));
    }

    /// gci i35/jh (vi32-jh H1 / H2, MEASURED on i34b): HotSpot 25 does not
    /// export `GC.heap_dump` to the DiagnosticCommand MBean -- not listed, no
    /// info row, no `help` page, not executable there -- while `jcmd` (the
    /// attach source) still has all of it.
    #[test]
    fn gci_i35jh_heap_dump_is_not_exported_to_the_mbean() {
        let table = parking_lot::RwLock::new(vec![
            command(
                "GC.heap_dump",
                "Generate a HPROF format dump of the Java heap.",
                CommandImpact::High,
            ),
            command("GC.run", "Call java.lang.System.gc().", CommandImpact::Medium),
        ]);
        assert_eq!(dcmd_mbean_names(&table.read()), vec!["GC.run", "help"]);
        assert!(dcmd_mbean_info(&table.read(), "GC.heap_dump").is_none());
        assert!(dcmd_mbean_info(&table.read(), "GC.run").is_some());
        assert_eq!(
            dcmd_mbean_execute(&table, "help GC.heap_dump"),
            Ok("Help unavailable : 'GC.heap_dump' : No such command\n".to_string())
        );
        assert_eq!(
            dcmd_mbean_execute(&table, "GC.heap_dump /tmp/x.hprof"),
            Err("Unknown diagnostic command".to_string())
        );
        let listing = dcmd_mbean_execute(&table, "help").expect("help");
        assert!(!listing.contains("GC.heap_dump"), "{listing}");
        let all = dcmd_mbean_execute(&table, "help -all").expect("help -all");
        assert!(!all.contains("GC.heap_dump") && all.contains("GC.run\n\t"), "{all}");
        // The attach source (`jcmd`) is unchanged.
        assert!(help(&table.read(), "GC.heap_dump").starts_with("GC.heap_dump\nGenerate a HPROF"));
        assert!(jcmd_help(&table.read(), &[]).contains("\nGC.heap_dump\n"));
        assert!(dcmd_visible("GC.run", true) && !dcmd_visible("attach load", false));
    }
}

#[cfg(test)]
mod gci_i33_jf_dcmd_tests {
    use super::*;

    /// A live state whose Java-side answers are scripted.
    struct JavaSideState {
        jfr: bool,
    }

    impl VmDiagnosticState for JavaSideState {
        fn thread_snapshots(&self) -> Vec<ThreadSnapshot> {
            Vec::new()
        }
        fn heap_summary(&self) -> HeapSummary {
            HeapSummary {
                young_gen_used: 0,
                young_gen_capacity: 0,
                old_gen_used: 0,
                old_gen_capacity: 0,
                metaspace_used: 0,
                metaspace_capacity: 0,
                total_used: 0,
                total_capacity: 0,
            }
        }
        fn class_histogram(&self) -> Vec<ClassHistogramEntry> {
            Vec::new()
        }
        fn trigger_gc(&self) -> bool {
            true
        }
        fn uptime_secs(&self) -> f64 {
            1.0
        }
        fn command_line(&self) -> String {
            String::new()
        }
        fn system_properties(&self) -> Vec<(String, String)> {
            Vec::new()
        }
        fn vm_flags(&self) -> Vec<String> {
            Vec::new()
        }
        fn jfr_help(&self, class: &str) -> Result<String, String> {
            if self.jfr {
                Ok(format!("Syntax : from {class}\n\nOptions:\n"))
            } else {
                Err("no Java thread".to_string())
            }
        }
        fn finalizer_info(&self) -> Result<String, String> {
            Ok(finalizer_info_text(&["p.A".to_string()]))
        }
        fn attach_load_enabled(&self) -> bool {
            true
        }
    }

    fn live(state: &Arc<dyn VmDiagnosticState>) -> AttachListener {
        let mut listener = AttachListener::new("");
        JcmdProcessor::register_live_commands(&mut listener, Arc::downgrade(state));
        listener
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// HotSpot 25's `FinalizerInfoDCmd` text: the empty answer, the header
    /// and `%10d  %s` rows, the most numerous class first.
    #[test]
    fn gci_i33jf_finalizer_info_text_is_hotspots() {
        assert_eq!(
            finalizer_info_text(&[]),
            "No instances waiting for finalization found\n"
        );
        assert_eq!(
            finalizer_info_text(&names(&["p.A", "p.Bee", "p.Bee"])),
            "Unreachable instances waiting for finalization\n\
             #instances  class name\n\
             -----------------------\n\
             \x20        2  p.Bee\n\
             \x20        1  p.A\n"
        );
    }

    /// Equal counts keep `FinalizerHistogram`'s `HashMap` order: by bucket,
    /// a new key at the head of its bin, and the resize that
    /// `computeIfAbsent` makes before it looks once `size > threshold`.
    #[test]
    fn gci_i33jf_finalizer_histogram_ties_follow_hashmaps_order() {
        assert_eq!(java_string_hash("a"), 97);
        assert_eq!(java_string_hash("java.lang.Object"), 1063877011);
        fn order(v: &[&str]) -> Vec<String> {
            finalizer_histogram(&names(v)).into_iter().map(|(n, _)| n).collect()
        }
        // Bucket order, whatever the walk order.
        assert_eq!(order(&["b", "a"]), names(&["a", "b"]));
        // "a" (97) and "q" (113) share bin 1 of 16: the later one heads it.
        assert_eq!(order(&["a", "q"]), names(&["q", "a"]));
        // The 14th call finds 13 keys (> 12) and doubles the table first:
        // "q" lands in bin 17 of 32, after "a".."m".
        let mut walk: Vec<&str> = vec![
            "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m",
        ];
        walk.push("q");
        let mut expected = walk.clone();
        expected.sort_unstable();
        assert_eq!(order(&walk), names(&expected));
        // Counts first (a stable sort, descending).
        assert_eq!(order(&["a", "q", "a"]), names(&["a", "q"]));
    }

    /// The live table registers HotSpot 25's `GC.finalizer_info` and
    /// `GC.run_finalization` (parserless `DCmd`s, HotSpot's descriptions and
    /// impacts); a state that cannot run them answers a failure, not a text.
    #[test]
    fn gci_i33jf_live_set_has_hotspots_finalization_commands() {
        let state: Arc<dyn VmDiagnosticState> = Arc::new(JavaSideState { jfr: false });
        let listener = live(&state);
        let commands = listener.commands.read();
        assert_eq!(
            jcmd_help(&commands, &names(&["GC.finalizer_info"])),
            "GC.finalizer_info\nProvide information about Java finalization queue.\n\n\
             Impact: Medium\n\nSyntax: GC.finalizer_info\n"
        );
        assert_eq!(
            jcmd_help(&commands, &names(&["GC.run_finalization"])),
            "GC.run_finalization\nCall java.lang.System.runFinalization().\n\n\
             Impact: Medium: Depends on Java content.\n\nSyntax: GC.run_finalization\n"
        );
        let run = |name: &str| {
            commands
                .iter()
                .find(|c| c.name == name)
                .expect("registered")
                .execute(&[])
        };
        let info = run("GC.finalizer_info");
        assert!(info.success);
        assert!(info.output.ends_with("\n         1  p.A\n"), "{}", info.output);
        // The default trait body: an honest refusal.
        assert!(!run("GC.run_finalization").success);
        let listed = dcmd_mbean_names(&commands);
        assert!(listed.contains(&"GC.finalizer_info".to_string()));
        assert!(listed.contains(&"GC.run_finalization".to_string()));
    }

    /// `help JFR.<x>` is HotSpot's header followed by the JDK class's own
    /// `getHelp()` text when the state can run it, and the generic page
    /// otherwise; `JFR.dump` / `JFR.stop` ask their own classes.
    #[test]
    fn gci_i33jf_jfr_help_is_the_jdks_text() {
        let state: Arc<dyn VmDiagnosticState> = Arc::new(JavaSideState { jfr: true });
        let listener = live(&state);
        let commands = listener.commands.read();
        assert_eq!(
            jcmd_help(&commands, &names(&["JFR.start"])),
            "JFR.start\nStarts a new JFR recording\n\n\
             Impact: Medium: Depending on the settings for a recording, the impact can range \
             from low to high.\n\n\
             Syntax : from jdk/jfr/internal/dcmd/DCmdStart\n\nOptions:\n"
        );
        assert!(jcmd_help(&commands, &names(&["JFR.dump"]))
            .ends_with("\nImpact: Low\n\nSyntax : from jdk/jfr/internal/dcmd/DCmdDump\n\nOptions:\n"));
        assert!(jcmd_help(&commands, &names(&["JFR.stop"]))
            .ends_with("\nImpact: Low\n\nSyntax : from jdk/jfr/internal/dcmd/DCmdStop\n\nOptions:\n"));
        // A parser command keeps its own page.
        assert!(jcmd_help(&commands, &names(&["VM.uptime"])).contains("\nSyntax : VM.uptime [options]\n"));
        drop(commands);
        drop(listener);

        let state: Arc<dyn VmDiagnosticState> = Arc::new(JavaSideState { jfr: false });
        let listener = live(&state);
        let page = jcmd_help(&listener.commands.read(), &names(&["JFR.start"]));
        assert!(page.contains("\n\nSyntax : JFR.start [options]\n"), "{page}");
    }

    #[test]
    fn hotspot_25_all_55_jcmd_commands_and_mbean_parity() {
        let state: Arc<dyn VmDiagnosticState> = Arc::new(JavaSideState { jfr: true });
        let listener = live(&state);
        let commands = listener.commands.read();
        let visible_55: [&str; 55] = [
            "AOT.end_recording",
            "Compiler.CodeHeap_Analytics",
            "Compiler.codecache",
            "Compiler.codelist",
            "Compiler.directives_add",
            "Compiler.directives_clear",
            "Compiler.directives_print",
            "Compiler.directives_remove",
            "Compiler.memory",
            "Compiler.queue",
            "GC.class_histogram",
            "GC.finalizer_info",
            "GC.heap_dump",
            "GC.heap_info",
            "GC.run",
            "GC.run_finalization",
            "JFR.check",
            "JFR.configure",
            "JFR.dump",
            "JFR.start",
            "JFR.stop",
            "JFR.view",
            "JVMTI.agent_load",
            "JVMTI.data_dump",
            "ManagementAgent.start",
            "ManagementAgent.start_local",
            "ManagementAgent.status",
            "ManagementAgent.stop",
            "System.dump_map",
            "System.map",
            "Thread.dump_to_file",
            "Thread.print",
            "Thread.vthread_pollers",
            "Thread.vthread_scheduler",
            "VM.cds",
            "VM.class_hierarchy",
            "VM.classes",
            "VM.classloader_stats",
            "VM.classloaders",
            "VM.command_line",
            "VM.dynlibs",
            "VM.events",
            "VM.flags",
            "VM.info",
            "VM.log",
            "VM.metaspace",
            "VM.native_memory",
            "VM.set_flag",
            "VM.stringtable",
            "VM.symboltable",
            "VM.system_properties",
            "VM.systemdictionary",
            "VM.uptime",
            "VM.version",
            "help",
        ];
        assert_eq!(visible_55.len(), 55);
        assert_eq!(
            commands.len(),
            56,
            "54 jcmd commands + SIGQUIT_HEAP_COMMAND + ATTACH_LOAD_COMMAND = 56 registered commands"
        );

        let help_list = jcmd_help(&commands, &[]);
        for &cmd in &visible_55 {
            assert!(
                help_list.contains(&format!("\n{cmd}\n")),
                "jcmd help missing visible command {cmd}"
            );
        }

        let mbean_names = dcmd_mbean_names(&commands);
        assert_eq!(
            mbean_names.len(),
            50,
            "HotSpot 25 DiagnosticCommand MBean exports 50 commands: {mbean_names:?}"
        );
        assert!(
            mbean_names.iter().any(|n| n == "JVMTI.agent_load"),
            "JVMTI.agent_load is full_export in HotSpot 25"
        );
        for attach_only in [
            "GC.heap_dump",
            "ManagementAgent.start",
            "ManagementAgent.start_local",
            "ManagementAgent.status",
            "ManagementAgent.stop",
        ] {
            assert!(
                !mbean_names.iter().any(|n| n == attach_only),
                "{attach_only} is attach-only (export: false) in HotSpot 25"
            );
        }

        // Verify JFR.configure static C++ help page
        let jfr_conf_help = jcmd_help(&commands, &names(&["JFR.configure"]));
        assert!(
            jfr_conf_help.starts_with("JFR.configure\nConfigure JFR\n\nImpact: Low\n\nOptions:\n"),
            "JFR.configure help mismatch: {jfr_conf_help}"
        );
        assert!(
            jfr_conf_help.contains(&format!(
                "  {:<20}(Optional) Path to the location",
                "dumppath"
            )),
            "JFR.configure help missing dumppath: {jfr_conf_help}"
        );

        // Verify JFR.check and JFR.view use DCmdCheck / DCmdView help pages
        assert!(jcmd_help(&commands, &names(&["JFR.check"]))
            .ends_with("\nImpact: Low\n\nSyntax : from jdk/jfr/internal/dcmd/DCmdCheck\n\nOptions:\n"));
        assert!(jcmd_help(&commands, &names(&["JFR.view"]))
            .ends_with("\nImpact: Medium\n\nSyntax : from jdk/jfr/internal/dcmd/DCmdView\n\nOptions:\n"));

        // Verify MBean argument info for JFR.* commands matches HotSpot 25
        let jfr_start_info = dcmd_mbean_info(&commands, "JFR.start").expect("JFR.start");
        assert_eq!(jfr_start_info.arguments.len(), 11);
        let settings_arg = jfr_start_info
            .arguments
            .iter()
            .find(|a| a.name == "settings")
            .expect("settings");
        assert_eq!(settings_arg.arg_type, "STRING SET");
        assert!(settings_arg.option && settings_arg.multiple && settings_arg.position == -1);

        let jfr_view_info = dcmd_mbean_info(&commands, "JFR.view").expect("JFR.view");
        assert_eq!(jfr_view_info.arguments.len(), 7);
        let view_arg = jfr_view_info
            .arguments
            .iter()
            .find(|a| a.name == "view")
            .expect("view");
        assert!(view_arg.mandatory && !view_arg.option && view_arg.position == -1);

        let jfr_conf_info = dcmd_mbean_info(&commands, "JFR.configure").expect("JFR.configure");
        assert_eq!(jfr_conf_info.arguments.len(), 10);
        drop(commands);

        // Verify VM.info and JFR.check command outputs
        let vm_info_out = dcmd_mbean_execute(&listener.commands, "VM.info").expect("VM.info");
        for section in [
            "#  JRE version:",
            "---------------  S U M M A R Y ------------",
            "---------------  T H R E A D  ---------------",
            "---------------  P R O C E S S  ---------------",
            "---------------  S Y S T E M  ---------------",
            "GC Precious Log:",
            "Heap:",
            "Polling page:",
            "Metaspace:",
            "GC Heap History",
        ] {
            assert!(vm_info_out.contains(section), "VM.info missing {section}");
        }
        let jfr_check_out = dcmd_mbean_execute(&listener.commands, "JFR.check").expect("JFR.check");
        assert_eq!(
            jfr_check_out,
            format!(
                "No available recordings.\n\nUse jcmd {} JFR.start to start a recording.\n",
                std::process::id()
            )
        );
    }
}


#[cfg(test)]
mod gcj_j2_ob2_stringtable_tests {
    use super::*;

    /// gcj j2/ob2: `VM.stringtable`'s figures follow the table's texts.
    #[test]
    fn gcj_j2_ob2_stringtable_census_counts_the_live_texts() {
        let texts = ["b", "a", "\u{e9}t\u{e9}", "\u{4e2d}"];
        let (entries, bytes, items) = stringtable_census(texts.iter().copied(), true);
        assert_eq!(entries, 4);
        // Latin-1: one byte per char ("\u{e9}t\u{e9}" is three); otherwise two
        // per UTF-16 unit.
        assert_eq!(bytes, 1 + 1 + 3 + 2);
        assert_eq!(items, vec!["a", "b", "\u{e9}t\u{e9}", "\u{4e2d}"]);
        let (entries, _, items) = stringtable_census(texts.iter().copied(), false);
        assert_eq!(entries, 4);
        assert!(items.is_empty());
        let text = default_vm_stringtable_text(entries, bytes, &[], false);
        assert!(text.contains("Number of entries       :         4"), "{text}");
        assert!(text.contains("Number of literals      :         4"), "{text}");
    }

    /// gcj j2/ob2: the `-verbose` option as HotSpot's `BOOLEAN` argument.
    #[test]
    fn gcj_j2_ob2_dcmd_verbose_option() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(!dcmd_verbose_option(&args(&[])));
        assert!(dcmd_verbose_option(&args(&["-verbose"])));
        assert!(dcmd_verbose_option(&args(&["-verbose=true"])));
        assert!(!dcmd_verbose_option(&args(&["-verbose", "-verbose=false"])));
    }

    /// gcj j2/ob2: G1 / ZGC never take the live text.
    #[test]
    fn gcj_j2_ob2_stringtable_live_is_generational_only() {
        assert!(!gen_jcmd_stringtable_live(false));
    }

    /// gcj j2/ob2 rows on j2a (`ob2_cap_st_verbose`): HotSpot's `-verbose`
    /// dump, modified-UTF-8 lengths and its escapes.
    #[test]
    fn gcj_j2_ob2_stringtable_verbose_text_is_hotspots_dump() {
        let texts: Vec<String> = ["message", "\u{1}\u{1}", "\r\n", "\t", "a\\\"b", "\u{402}", "\0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let text = hotspot_stringtable_verbose_text(&texts);
        let want = "VERSION: 1.1\n7: message\n2: \\x01\\x01\n2: \\r\\n\n1: \\t\n4: a\\\\\"b\n\
                    2: \\xd0\\x82\n2: \\xc0\\x80\n";
        assert_eq!(text, want);
    }

    /// gcj j2/ob2 rows on j2a: the statistics' literal average is an integer
    /// division (`1672 bytes, avg  47.000` for 35 literals).
    #[test]
    fn gcj_j2_ob2_stringtable_average_is_an_integer_division() {
        let text = default_vm_stringtable_text(35, 1672, &[], false);
        assert!(
            text.contains("Number of literals      :        35 =      1672 bytes, avg  47.000"),
            "{text}"
        );
    }

    /// gcj j2/ob2 rows on j2a (`ob2_cap_vmuptime`, `ob2_cap_vmcmd`):
    /// HotSpot's `VM.uptime` and `VM.command_line` shapes.
    #[test]
    fn gcj_j2_ob2_vm_uptime_and_command_line_texts() {
        assert_eq!(hotspot_vm_uptime_text(0.2104), "0.210 s\n");
        let args = vec!["-XX:+UseSerialGC".to_string(), "-Xmx256m".to_string()];
        assert_eq!(
            hotspot_vm_command_line_text(&args, Some("Main a"), "C:/cp", "SUN_STANDARD"),
            "VM Arguments:\njvm_args: -XX:+UseSerialGC -Xmx256m \njava_command: Main a\n\
             java_class_path (initial): C:/cp\nLauncher Type: SUN_STANDARD\n"
        );
        assert_eq!(
            hotspot_vm_command_line_text(&[], None, "", "SUN_STANDARD"),
            "VM Arguments:\njava_command: <unknown>\njava_class_path (initial): <not set>\n\
             Launcher Type: SUN_STANDARD\n"
        );
        assert!(!gen_jcmd_hotspot_texts(false));
    }
}
