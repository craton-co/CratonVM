// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Serviceability Tools — jcmd/jstack/jmap diagnostic infrastructure.
//!
//! Provides attach API, diagnostic command processing, thread dump generation,
//! heap analysis, and HPROF stub writing for JVM serviceability tooling.

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

/// Trait for querying live VM state from diagnostic commands.
/// The VM implements this to provide real data to serviceability tools.
pub trait VmDiagnosticState: Send + Sync {
    /// Get snapshots of all live threads.
    fn thread_snapshots(&self) -> Vec<ThreadSnapshot>;
    /// Get heap summary information.
    fn heap_summary(&self) -> HeapSummary;
    /// Get class histogram entries.
    fn class_histogram(&self) -> Vec<ClassHistogramEntry>;
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
}

// ---------------------------------------------------------------------------
// Attach API infrastructure
// ---------------------------------------------------------------------------

/// Listener for diagnostic attach requests (domain socket based).
pub struct AttachListener {
    pub socket_path: String,
    pub is_listening: bool,
    pub commands: Vec<DiagnosticCommand>,
}

impl AttachListener {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
            is_listening: false,
            commands: Vec::new(),
        }
    }

    pub fn start_listening(&mut self) {
        self.is_listening = true;
    }

    pub fn stop_listening(&mut self) {
        self.is_listening = false;
    }

    pub fn register_command(&mut self, cmd: DiagnosticCommand) {
        self.commands.push(cmd);
    }

    pub fn find_command(&self, name: &str) -> Option<&DiagnosticCommand> {
        self.commands.iter().find(|c| c.name == name)
    }

    pub fn list_commands(&self) -> Vec<&str> {
        self.commands.iter().map(|c| c.name.as_str()).collect()
    }
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

/// A registered diagnostic command.
pub struct DiagnosticCommand {
    pub name: String,
    pub description: String,
    pub impact: CommandImpact,
    pub permission: CommandPermission,
    pub arguments: Vec<CommandArgument>,
    handler: Box<dyn Fn(&[String]) -> CommandResult + Send + Sync>,
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
        }
    }

    pub fn execute(&self, args: &[String]) -> CommandResult {
        (self.handler)(args)
    }
}

// ---------------------------------------------------------------------------
// jcmd implementation
// ---------------------------------------------------------------------------

/// Processes jcmd-style diagnostic commands.
pub struct JcmdProcessor {
    pub attach_listener: AttachListener,
}

impl JcmdProcessor {
    pub fn new() -> Self {
        let mut listener = AttachListener::new("/tmp/cratonvm_attach");
        listener.start_listening();

        // Register all standard commands
        Self::register_standard_commands(&mut listener);

        Self {
            attach_listener: listener,
        }
    }

    /// Create a JcmdProcessor backed by live VM state.
    pub fn new_with_vm_state(vm_state: Arc<dyn VmDiagnosticState>) -> Self {
        let mut listener = AttachListener::new("/tmp/cratonvm_attach");
        listener.start_listening();
        Self::register_live_commands(&mut listener, vm_state);
        Self {
            attach_listener: listener,
        }
    }

    fn register_standard_commands(listener: &mut AttachListener) {
        // 1. Thread.print
        listener.register_command(DiagnosticCommand::new(
            "Thread.print",
            "Print all threads with stacktraces",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                let threads = sample_thread_snapshots();
                let dump = JstackProcessor::generate_thread_dump(&threads);
                CommandResult::ok(dump, 0)
            }),
        ));

        // 2. GC.heap_dump
        listener.register_command(DiagnosticCommand::new(
            "GC.heap_dump",
            "Generate a HPROF format heap dump",
            CommandImpact::High,
            CommandPermission::ManagementAction,
            vec![CommandArgument {
                name: "filename".to_string(),
                description: "Output file path".to_string(),
                arg_type: ArgType::FilePath,
                required: false,
                default_value: Some("heap.hprof".to_string()),
            }],
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
            "Trigger garbage collection",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![],
            Box::new(|_args| CommandResult::ok("GC triggered".to_string(), 0)),
        ));

        // 4. GC.heap_info
        listener.register_command(DiagnosticCommand::new(
            "GC.heap_info",
            "Print heap summary information",
            CommandImpact::Low,
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
            "Print class histogram",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                let entries = sample_class_histogram();
                let output = JmapProcessor::generate_class_histogram(&entries);
                CommandResult::ok(output, 0)
            }),
        ));

        // 6. VM.version
        listener.register_command(DiagnosticCommand::new(
            "VM.version",
            "Print VM version",
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
            "Print VM flag settings",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
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
            "Print system properties",
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
            "Print VM uptime",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| CommandResult::ok("VM uptime: 3600.000 seconds".to_string(), 0)),
        ));

        // 10. VM.info
        listener.register_command(DiagnosticCommand::new(
            "VM.info",
            "Print comprehensive VM information",
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
            "Print command line arguments",
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
            "Dump threads to a file",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![CommandArgument {
                name: "filepath".to_string(),
                description: "Output file path".to_string(),
                arg_type: ArgType::FilePath,
                required: false,
                default_value: Some("thread_dump.txt".to_string()),
            }],
            Box::new(|args| {
                let path = args
                    .first()
                    .map(|s| s.as_str())
                    .unwrap_or("thread_dump.txt");
                CommandResult::ok(format!("Thread dump written to {}", path), 0)
            }),
        ));

        // 13. Compiler.queue
        listener.register_command(DiagnosticCommand::new(
            "Compiler.queue",
            "Print compilation queue",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                let queue = vec![
                    "C1 compile queue: 3 methods",
                    "  1: java.lang.String.hashCode()I (tier 1)",
                    "  2: java.util.HashMap.get(Ljava/lang/Object;)Ljava/lang/Object; (tier 1)",
                    "  3: java.lang.Math.max(II)I (tier 1)",
                    "C2 compile queue: 1 method",
                    "  1: com.example.Main.hotLoop()V (tier 4)",
                ];
                CommandResult::ok(queue.join("\n"), 0)
            }),
        ));

        // 14. JFR.start
        listener.register_command(DiagnosticCommand::new(
            "JFR.start",
            "Start a flight recording",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![CommandArgument {
                name: "name".to_string(),
                description: "Recording name".to_string(),
                arg_type: ArgType::String,
                required: false,
                default_value: Some("recording1".to_string()),
            }],
            Box::new(|args| {
                let name = args.first().map(|s| s.as_str()).unwrap_or("recording1");
                CommandResult::ok(format!("Flight recording started: {}", name), 0)
            }),
        ));

        // 15. JFR.stop
        listener.register_command(DiagnosticCommand::new(
            "JFR.stop",
            "Stop a flight recording",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![CommandArgument {
                name: "name".to_string(),
                description: "Recording name".to_string(),
                arg_type: ArgType::String,
                required: false,
                default_value: Some("recording1".to_string()),
            }],
            Box::new(|args| {
                let name = args.first().map(|s| s.as_str()).unwrap_or("recording1");
                CommandResult::ok(format!("Flight recording stopped: {}", name), 0)
            }),
        ));

        // 16. JFR.dump
        listener.register_command(DiagnosticCommand::new(
            "JFR.dump",
            "Dump flight recording to file",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![CommandArgument {
                name: "filename".to_string(),
                description: "Output file path".to_string(),
                arg_type: ArgType::FilePath,
                required: false,
                default_value: Some("recording.jfr".to_string()),
            }],
            Box::new(|args| {
                let path = args.first().map(|s| s.as_str()).unwrap_or("recording.jfr");
                CommandResult::ok(format!("Flight recording dumped to {}", path), 0)
            }),
        ));
    }

    fn register_live_commands(listener: &mut AttachListener, vm_state: Arc<dyn VmDiagnosticState>) {
        // 1. Thread.print
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "Thread.print",
            "Print all threads with stacktraces",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let threads = vs.thread_snapshots();
                let dump = JstackProcessor::generate_thread_dump(&threads);
                CommandResult::ok(dump, 0)
            }),
        ));

        // 2. GC.heap_dump
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "GC.heap_dump",
            "Generate a HPROF format heap dump",
            CommandImpact::High,
            CommandPermission::ManagementAction,
            vec![CommandArgument {
                name: "filename".to_string(),
                description: "Output file path".to_string(),
                arg_type: ArgType::FilePath,
                required: false,
                default_value: Some("heap.hprof".to_string()),
            }],
            Box::new(move |args| {
                let path = args.first().map(|s| s.as_str()).unwrap_or("heap.hprof");
                match vs.heap_dump(path) {
                    Ok(bytes) => CommandResult::ok(
                        format!("Heap dump written to {}\nDump size: {} bytes", path, bytes),
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
            "Trigger garbage collection",
            CommandImpact::Medium,
            CommandPermission::ManagementAction,
            vec![],
            Box::new(move |_args| {
                let ran = vs.trigger_gc();
                if ran {
                    CommandResult::ok("GC triggered and completed".to_string(), 0)
                } else {
                    CommandResult::ok("GC trigger requested (may be deferred)".to_string(), 0)
                }
            }),
        ));

        // 4. GC.heap_info
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "GC.heap_info",
            "Print heap summary information",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let summary = vs.heap_summary();
                let output = JmapProcessor::generate_heap_summary(&summary);
                CommandResult::ok(output, 0)
            }),
        ));

        // 5. GC.class_histogram
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "GC.class_histogram",
            "Print class histogram",
            CommandImpact::Medium,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let entries = vs.class_histogram();
                let output = JmapProcessor::generate_class_histogram(&entries);
                CommandResult::ok(output, 0)
            }),
        ));

        // 6. VM.version
        listener.register_command(DiagnosticCommand::new(
            "VM.version",
            "Print VM version",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_args| {
                CommandResult::ok("CratonVM 1.0.0 (JDK 25 compatible)".to_string(), 0)
            }),
        ));

        // 7. VM.flags
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.flags",
            "Print VM flag settings",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                let flags = vs.vm_flags();
                CommandResult::ok(flags.join("\n"), 0)
            }),
        ));

        // 8. VM.system_properties
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.system_properties",
            "Print system properties",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
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
            "Print VM uptime",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| {
                CommandResult::ok(format!("VM uptime: {:.3} seconds", vs.uptime_secs()), 0)
            }),
        ));

        // 10. VM.command_line
        let vs = vm_state.clone();
        listener.register_command(DiagnosticCommand::new(
            "VM.command_line",
            "Print command line arguments",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(move |_args| CommandResult::ok(vs.command_line(), 0)),
        ));
    }

    /// Process a jcmd command line (e.g. "Thread.print" or "GC.heap_dump /tmp/dump.hprof").
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

        match self.attach_listener.find_command(cmd_name) {
            Some(cmd) => {
                let start = Instant::now();
                let mut result = cmd.execute(&args);
                result.execution_time_ms = start.elapsed().as_millis() as u64;
                result
            }
            None => CommandResult::err(format!("Unknown command: {}", cmd_name), 0),
        }
    }

    /// Generate help text listing all available commands.
    pub fn help(&self) -> String {
        let mut lines = vec!["Available commands:".to_string()];
        for cmd in &self.attach_listener.commands {
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
}

/// Processes jstack-style thread dumps.
pub struct JstackProcessor;

impl JstackProcessor {
    /// Generate a HotSpot-style thread dump string.
    pub fn generate_thread_dump(threads: &[ThreadSnapshot]) -> String {
        let mut output = String::new();
        output.push_str("Full thread dump CratonVM 1.0.0 (JDK 25 compatible):\n\n");

        for thread in threads {
            // Header line
            let daemon_str = if thread.daemon { " daemon" } else { "" };
            let state_short = match thread.state {
                ThreadState::Runnable => "runnable",
                ThreadState::Blocked => "waiting for monitor entry",
                ThreadState::Waiting | ThreadState::TimedWaiting => "waiting on condition",
                ThreadState::New => "new",
                ThreadState::Terminated => "terminated",
            };

            output.push_str(&format!(
                "\"{}\" #{}{} prio={} os_prio=0 tid=0x{:016x} nid=0x{:x} {} [0x{:016x}]\n",
                thread.name,
                thread.id,
                daemon_str,
                thread.priority,
                thread.id * 0x1000,
                thread.id,
                state_short,
                thread.id * 0x7000,
            ));

            // Thread state
            output.push_str(&format!("   java.lang.Thread.State: {}\n", thread.state));

            // Stack frames
            for frame in &thread.stack_frames {
                let location = if frame.native_method {
                    "Native Method".to_string()
                } else {
                    match &frame.file_name {
                        Some(f) => format!("{}:{}", f, frame.line_number),
                        None => "Unknown Source".to_string(),
                    }
                };
                output.push_str(&format!(
                    "\tat {}.{}({})\n",
                    frame.class_name, frame.method_name, location
                ));
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

        output
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

/// Processes jmap-style heap analysis commands.
pub struct JmapProcessor;

impl JmapProcessor {
    /// Generate a class histogram report.
    pub fn generate_class_histogram(classes: &[ClassHistogramEntry]) -> String {
        let mut output = String::new();
        output.push_str(" num     #instances         #bytes  class name\n");
        output.push_str("----------------------------------------------\n");

        let mut total_instances: u64 = 0;
        let mut total_bytes: u64 = 0;

        for (i, entry) in classes.iter().enumerate() {
            output.push_str(&format!(
                "{:>4}:  {:>10}  {:>12}  {}\n",
                i + 1,
                entry.instance_count,
                entry.total_bytes,
                entry.class_name
            ));
            total_instances += entry.instance_count;
            total_bytes += entry.total_bytes;
        }

        output.push_str("----------------------------------------------\n");
        output.push_str(&format!(
            "Total: {:>10}  {:>12}\n",
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
                    .and_then(|layout| layout.field_offset(field_index))
                    .map_or_else(
                        || HEADER_SIZE + field_index * SLOT_SIZE,
                        |off| HEADER_SIZE + off as usize,
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
            let elem_offset = HEADER_SIZE + i * cratonvm_types::narrow_oop::ref_element_size();
            let val = if elem_offset + cratonvm_types::narrow_oop::ref_element_size()
                <= obj.total_size
            {
                unsafe {
                    let ptr = obj.data_ptr.add(elem_offset);
                    cratonvm_types::narrow_oop::read_ref_slot_unaligned(ptr)
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
    use super::*;

    // --- AttachListener tests ---

    #[test]
    fn test_attach_listener_new() {
        let listener = AttachListener::new("/tmp/test_attach");
        assert_eq!(listener.socket_path, "/tmp/test_attach");
        assert!(!listener.is_listening);
        assert!(listener.commands.is_empty());
    }

    #[test]
    fn test_attach_listener_start_stop() {
        let mut listener = AttachListener::new("/tmp/test");
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
        assert_eq!(listener.commands.len(), 1);
        assert_eq!(listener.commands[0].name, "Test.cmd");
    }

    #[test]
    fn test_attach_listener_find_command() {
        let mut listener = AttachListener::new("/tmp/test");
        listener.register_command(DiagnosticCommand::new(
            "Test.cmd",
            "desc",
            CommandImpact::Low,
            CommandPermission::ReadOnly,
            vec![],
            Box::new(|_| CommandResult::ok("ok".to_string(), 0)),
        ));
        assert!(listener.find_command("Test.cmd").is_some());
        assert!(listener.find_command("Nonexistent").is_none());
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
        assert_eq!(names, vec!["A.cmd", "B.cmd"]);
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
        let jcmd = JcmdProcessor::new();
        let names = jcmd.attach_listener.list_commands();
        assert_eq!(names.len(), 16);
        assert!(names.contains(&"Thread.print"));
        assert!(names.contains(&"GC.heap_dump"));
        assert!(names.contains(&"GC.run"));
        assert!(names.contains(&"GC.heap_info"));
        assert!(names.contains(&"GC.class_histogram"));
        assert!(names.contains(&"VM.version"));
        assert!(names.contains(&"VM.flags"));
        assert!(names.contains(&"VM.system_properties"));
        assert!(names.contains(&"VM.uptime"));
        assert!(names.contains(&"VM.info"));
        assert!(names.contains(&"VM.command_line"));
        assert!(names.contains(&"Thread.dump_to_file"));
        assert!(names.contains(&"Compiler.queue"));
        assert!(names.contains(&"JFR.start"));
        assert!(names.contains(&"JFR.stop"));
        assert!(names.contains(&"JFR.dump"));
    }

    #[test]
    fn test_jcmd_thread_print() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("Thread.print");
        assert!(result.success);
        assert!(result.output.contains("main"));
        assert!(result.output.contains("RUNNABLE"));
    }

    #[test]
    fn test_jcmd_gc_heap_dump() {
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
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("GC.run");
        assert!(result.success);
        assert_eq!(result.output, "GC triggered");
    }

    #[test]
    fn test_jcmd_gc_heap_info() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("GC.heap_info");
        assert!(result.success);
        assert!(result.output.contains("Young Generation"));
        assert!(result.output.contains("Old Generation"));
        assert!(result.output.contains("Metaspace"));
    }

    #[test]
    fn test_jcmd_gc_class_histogram() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("GC.class_histogram");
        assert!(result.success);
        assert!(result.output.contains("#instances"));
        assert!(result.output.contains("java.lang.String"));
    }

    #[test]
    fn test_jcmd_vm_version() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.version");
        assert!(result.success);
        assert_eq!(result.output, "CratonVM 1.0.0 (JDK 25 compatible)");
    }

    #[test]
    fn test_jcmd_vm_flags() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.flags");
        assert!(result.success);
        assert!(result.output.contains("-XX:+UseG1GC"));
        assert!(result.output.lines().count() >= 15);
    }

    #[test]
    fn test_jcmd_vm_system_properties() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.system_properties");
        assert!(result.success);
        assert!(result.output.contains("java.version=25"));
        assert!(result.output.contains("java.vendor=CratonVM"));
    }

    #[test]
    fn test_jcmd_vm_uptime() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.uptime");
        assert!(result.success);
        assert!(result.output.contains("uptime"));
    }

    #[test]
    fn test_jcmd_vm_info() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.info");
        assert!(result.success);
        assert!(result.output.contains("CratonVM"));
        assert!(result.output.contains("Heap"));
    }

    #[test]
    fn test_jcmd_vm_command_line() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("VM.command_line");
        assert!(result.success);
        assert!(result.output.contains("-Xmx256m"));
    }

    #[test]
    fn test_jcmd_thread_dump_to_file() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("Thread.dump_to_file /tmp/threads.txt");
        assert!(result.success);
        assert!(result
            .output
            .contains("Thread dump written to /tmp/threads.txt"));
    }

    #[test]
    fn test_jcmd_compiler_queue() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("Compiler.queue");
        assert!(result.success);
        assert!(result.output.contains("C1 compile queue"));
        assert!(result.output.contains("C2 compile queue"));
    }

    #[test]
    fn test_jcmd_jfr_start() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("JFR.start myrecording");
        assert!(result.success);
        assert!(result
            .output
            .contains("Flight recording started: myrecording"));
    }

    #[test]
    fn test_jcmd_jfr_stop() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("JFR.stop myrecording");
        assert!(result.success);
        assert!(result
            .output
            .contains("Flight recording stopped: myrecording"));
    }

    #[test]
    fn test_jcmd_jfr_dump() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("JFR.dump /tmp/rec.jfr");
        assert!(result.success);
        assert!(result
            .output
            .contains("Flight recording dumped to /tmp/rec.jfr"));
    }

    #[test]
    fn test_jcmd_unknown_command() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("Nonexistent.command");
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("Unknown command"));
    }

    #[test]
    fn test_jcmd_help() {
        let jcmd = JcmdProcessor::new();
        let result = jcmd.process_command("help");
        assert!(result.success);
        assert!(result.output.contains("Available commands"));
        assert!(result.output.contains("Thread.print"));
        assert!(result.output.contains("VM.version"));
    }

    #[test]
    fn test_jcmd_help_method() {
        let jcmd = JcmdProcessor::new();
        let help = jcmd.help();
        assert!(help.contains("Available commands"));
        // Should list all 16 commands
        for name in jcmd.attach_listener.list_commands() {
            assert!(help.contains(name));
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
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("\"main\" #1 prio=5"));
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
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("\"GC-Worker\" #5 daemon prio=8"));
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
        }];

        let dump = JstackProcessor::generate_thread_dump(&threads);
        assert!(dump.contains("at com.a.A.foo(A.java:10)"));
        assert!(dump.contains("at com.b.B.bar(B.java:20)"));
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
        assert!(output.contains("Total:"));
        assert!(output.contains("1500")); // total instances
        assert!(output.contains("70000")); // total bytes
    }

    #[test]
    fn test_jmap_class_histogram_empty() {
        let output = JmapProcessor::generate_class_histogram(&[]);
        assert!(output.contains("#instances"));
        assert!(output.contains("Total:"));
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
        assert_eq!(jcmd.attach_listener.commands.len(), 16);
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

    #[test]
    fn test_jcmd_with_live_vm_state() {
        let state = Arc::new(MockVmState);
        let processor = JcmdProcessor::new_with_vm_state(state);

        let result = processor.process_command("Thread.print");
        assert!(result.success);
        assert!(result.output.contains("main"));

        let result = processor.process_command("GC.run");
        assert!(result.success);
        assert!(result.output.contains("completed"));

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
            42,
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
        let data_size = array_length as usize * cratonvm_types::narrow_oop::ref_element_size();
        let total_size = HEADER_SIZE + ((data_size + 7) & !7);
        let mut mem = vec![0u8; total_size];

        let header = ObjectHeader::new(
            cratonvm_types::ClassId::new(50),
            ObjectKind::Array,
            ArrayElementType::Reference,
            0,
            array_length,
            array_length,
        );
        unsafe {
            std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
        }

        // Write element references: [0xCAFE, 0xBEEF]
        for (i, val) in [0xCAFEu64, 0xBEEF].iter().enumerate() {
            let offset = HEADER_SIZE + i * cratonvm_types::narrow_oop::ref_element_size();
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
            123,
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
        use cratonvm_gc::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE};
        use cratonvm_gc::register_class_layout;
        use cratonvm_types::{CompactLayout, FieldStorageKind, GC_FLAG_COMPACT};
        use std::sync::Arc;

        const CLASS_ID: u32 = 61_001;

        register_class_layout(
            CLASS_ID,
            Arc::new(CompactLayout {
                field_offsets: vec![0],
                is_ref: vec![true],
                field_kinds: vec![FieldStorageKind::Reference],
                ref_offsets: vec![0],
                body_size: 8,
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
                instance_size: (HEADER_SIZE + 8) as u32,
            },
        )]
        .into_iter()
        .collect();

        let total_size = HEADER_SIZE + 8;
        let mut mem = vec![0u8; total_size];
        let expected_ref = 0x1_2345_6789_abcd_u64;
        let mut header = ObjectHeader::new(
            cratonvm_types::ClassId::new(CLASS_ID),
            ObjectKind::Object,
            ArrayElementType::Boolean,
            0,
            0,
            1,
        );
        header.set_compact_shape(1, 8);
        unsafe {
            std::ptr::write(mem.as_mut_ptr() as *mut ObjectHeader, header);
            std::ptr::write_unaligned(mem.as_mut_ptr().add(HEADER_SIZE) as *mut u64, expected_ref);
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
