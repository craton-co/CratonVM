//! Signal handling, shutdown hooks, and helpful NullPointerException messages.
//!
//! - **Phase 16.1**: Signal registration, shutdown hook lifecycle, thread dumping.
//! - **Phase 16.2**: JEP 358 — bytecode-level NPE analysis producing HotSpot-style messages.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

// ── Signal constants ────────────────────────────────────────────────────────

pub const SIGINT: i32 = 2;
pub const SIGTERM: i32 = 15;
pub const SIGQUIT: i32 = 3;
pub const SIGHUP: i32 = 1;
pub const SIGUSR1: i32 = 10;
pub const SIGUSR2: i32 = 12;

// ── Signal types ────────────────────────────────────────────────────────────

/// Function pointer type for custom signal handlers.
pub type SignalHandlerFn = fn(i32);

/// Action to take when a signal is received.
#[derive(Clone)]
pub enum SignalAction {
    /// Use OS default behaviour.
    Default,
    /// Ignore the signal.
    Ignore,
    /// Invoke a user-supplied handler.
    Handle(SignalHandlerFn),
}

impl std::fmt::Debug for SignalAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => write!(f, "Default"),
            Self::Ignore => write!(f, "Ignore"),
            Self::Handle(_) => write!(f, "Handle(<fn>)"),
        }
    }
}

// ── Shutdown hooks ──────────────────────────────────────────────────────────

/// Lifecycle state of a shutdown hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookState {
    Registered,
    Running,
    Completed,
    Failed(String),
}

/// A registered shutdown hook.
#[derive(Debug, Clone)]
pub struct ShutdownHook {
    pub id: u64,
    pub name: String,
    pub priority: i32,
    pub state: HookState,
    action: Option<fn()>,
}

impl ShutdownHook {
    pub fn new(id: u64, name: String, priority: i32, action: fn()) -> Self {
        Self {
            id,
            name,
            priority,
            state: HookState::Registered,
            action: Some(action),
        }
    }

    /// Create a hook without an action (for testing / placeholder purposes).
    pub fn new_no_action(id: u64, name: String, priority: i32) -> Self {
        Self {
            id,
            name,
            priority,
            state: HookState::Registered,
            action: None,
        }
    }
}

/// Aggregate result of running all shutdown hooks.
#[derive(Debug, Clone)]
pub struct ShutdownResult {
    pub hooks_run: u32,
    pub hooks_failed: u32,
    pub total_time_ms: u64,
    pub errors: Vec<String>,
}

// ── SignalHandler ───────────────────────────────────────────────────────────

static NEXT_HOOK_ID: AtomicU64 = AtomicU64::new(1);

/// Central registry for signal actions and shutdown hooks.
pub struct SignalHandler {
    pub registered_signals: HashMap<i32, SignalAction>,
    pub shutdown_hooks: Vec<ShutdownHook>,
    pub shutdown_in_progress: AtomicBool,
}

impl SignalHandler {
    pub fn new() -> Self {
        Self {
            registered_signals: HashMap::new(),
            shutdown_hooks: Vec::new(),
            shutdown_in_progress: AtomicBool::new(false),
        }
    }

    /// Register (or replace) the action for a given signal number.
    pub fn register_signal(&mut self, signal: i32, action: SignalAction) {
        self.registered_signals.insert(signal, action);
    }

    /// Allocate a fresh hook id.
    pub fn next_hook_id() -> u64 {
        NEXT_HOOK_ID.fetch_add(1, Ordering::SeqCst)
    }

    /// Add a shutdown hook.  Fails if shutdown is already in progress.
    pub fn add_shutdown_hook(&mut self, hook: ShutdownHook) -> Result<(), String> {
        if self.shutdown_in_progress.load(Ordering::SeqCst) {
            return Err("Cannot add shutdown hook: shutdown in progress".into());
        }
        self.shutdown_hooks.push(hook);
        Ok(())
    }

    /// Remove a hook by id.  Returns `true` if found and removed.
    pub fn remove_shutdown_hook(&mut self, id: u64) -> bool {
        let before = self.shutdown_hooks.len();
        self.shutdown_hooks.retain(|h| h.id != id);
        self.shutdown_hooks.len() < before
    }

    /// Run all shutdown hooks sorted by priority (lower runs first).
    pub fn run_shutdown_hooks(&mut self) -> ShutdownResult {
        let start = Instant::now();

        // Sort by priority — lower value = earlier execution.
        self.shutdown_hooks.sort_by_key(|h| h.priority);

        let mut hooks_run: u32 = 0;
        let mut hooks_failed: u32 = 0;
        let mut errors: Vec<String> = Vec::new();

        for hook in self.shutdown_hooks.iter_mut() {
            hook.state = HookState::Running;
            hooks_run += 1;

            if let Some(action) = hook.action {
                // In a real JVM the hook would be a Thread; here we just call the fn.
                let result = std::panic::catch_unwind(action);
                match result {
                    Ok(()) => {
                        hook.state = HookState::Completed;
                    }
                    Err(_) => {
                        let msg = format!("Hook '{}' (id={}) panicked", hook.name, hook.id);
                        hook.state = HookState::Failed(msg.clone());
                        errors.push(msg);
                        hooks_failed += 1;
                    }
                }
            } else {
                // No action — just mark completed.
                hook.state = HookState::Completed;
            }
        }

        let elapsed = start.elapsed();
        ShutdownResult {
            hooks_run,
            hooks_failed,
            total_time_ms: elapsed.as_millis() as u64,
            errors,
        }
    }

    /// Try to initiate shutdown.  Returns `true` if *this* call is the one
    /// that actually triggered it (CAS false→true).
    pub fn initiate_shutdown(&self) -> bool {
        self.shutdown_in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

impl Default for SignalHandler {
    fn default() -> Self {
        Self::new()
    }
}

// ── Thread dumper ───────────────────────────────────────────────────────────

/// A single frame on a thread's stack.
#[derive(Debug, Clone)]
pub struct StackFrame {
    pub class_name: String,
    pub method_name: String,
    pub file_name: Option<String>,
    pub line_number: i32,
}

/// Snapshot of one thread's state.
#[derive(Debug, Clone)]
pub struct ThreadInfo {
    pub id: u64,
    pub name: String,
    pub state: String,
    pub stack_frames: Vec<StackFrame>,
}

/// Formats thread dumps in HotSpot style.
pub struct ThreadDumper;

impl ThreadDumper {
    /// Produce a multi-thread dump string resembling HotSpot output.
    pub fn dump_all_threads(threads: &[ThreadInfo]) -> String {
        let mut out = String::new();
        for t in threads {
            out.push_str(&format!(
                "\"{}\" #{} daemon prio=5 os_prio=0 tid=0x{:016x} nid=0x{:x} [{}]\n",
                t.name, t.id, t.id, t.id, t.state
            ));
            for frame in &t.stack_frames {
                let location = match &frame.file_name {
                    Some(file) if frame.line_number > 0 => {
                        format!("({}:{})", file, frame.line_number)
                    }
                    Some(file) => format!("({})", file),
                    None => "(Unknown Source)".to_string(),
                };
                out.push_str(&format!(
                    "   at {}.{}{}\n",
                    frame.class_name, frame.method_name, location
                ));
            }
            out.push('\n');
        }
        out
    }
}

// ── Helpful NPE messages (JEP 358) ─────────────────────────────────────────

// Bytecode opcodes we care about for NPE analysis.
const GETFIELD: u8 = 0xB4;
const PUTFIELD: u8 = 0xB5;
const INVOKEVIRTUAL: u8 = 0xB6;
const INVOKEINTERFACE: u8 = 0xB9;
const ARRAYLENGTH: u8 = 0xBE;
const AALOAD: u8 = 0x32;
const IALOAD: u8 = 0x2E;
const FALOAD: u8 = 0x30;
const DALOAD: u8 = 0x31;
const LALOAD: u8 = 0x2F;
const BALOAD: u8 = 0x33;
const CALOAD: u8 = 0x34;
const SALOAD: u8 = 0x35;
const AASTORE: u8 = 0x53;
const IASTORE: u8 = 0x4F;
const FASTORE: u8 = 0x51;
const DASTORE: u8 = 0x52;
const LASTORE: u8 = 0x50;
const BASTORE: u8 = 0x54;
const CASTORE: u8 = 0x55;
const SASTORE: u8 = 0x56;
const ATHROW: u8 = 0xBF;
const MONITORENTER: u8 = 0xC2;

/// Category of entity involved in an NPE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NpeEntityType {
    Field,
    Method,
    ArrayAccess,
    ArrayLength,
    ArrayStore,
    Monitor,
    Throw,
    Unresolved,
}

/// Result of analysing a bytecode site for helpful NPE context.
#[derive(Debug, Clone)]
pub struct NpeAnalysisResult {
    pub entity_type: NpeEntityType,
    pub entity_name: Option<String>,
    pub class_name: Option<String>,
    pub reason: String,
}

/// Generates helpful NPE messages by inspecting the bytecode at the throw
/// site, similar to HotSpot's JEP 358 implementation.
pub struct NpeMessageGenerator;

impl NpeMessageGenerator {
    /// Given a method's bytecode, a bytecode index, and stack depth,
    /// produce a human-readable NPE message or `None` if the bytecode
    /// cannot be decoded.
    pub fn generate_message(
        bytecode: &[u8],
        bci: usize,
        _stack_depth: usize,
    ) -> Option<String> {
        let result = Self::analyze(bytecode, bci)?;
        Some(result.reason)
    }

    /// Full structured analysis.
    pub fn analyze(bytecode: &[u8], bci: usize) -> Option<NpeAnalysisResult> {
        if bci >= bytecode.len() {
            return None;
        }
        let opcode = bytecode[bci];
        match opcode {
            GETFIELD => Self::analyze_field_access(bytecode, bci, false),
            PUTFIELD => Self::analyze_field_access(bytecode, bci, true),
            INVOKEVIRTUAL | INVOKEINTERFACE => Self::analyze_invoke(bytecode, bci),
            ARRAYLENGTH => Some(NpeAnalysisResult {
                entity_type: NpeEntityType::ArrayLength,
                entity_name: None,
                class_name: None,
                reason: "Cannot read the array length because \"<local>\" is null".into(),
            }),
            AALOAD | IALOAD | FALOAD | DALOAD | LALOAD | BALOAD | CALOAD | SALOAD => {
                Some(NpeAnalysisResult {
                    entity_type: NpeEntityType::ArrayAccess,
                    entity_name: None,
                    class_name: None,
                    reason: "Cannot load from array because \"<local>\" is null".into(),
                })
            }
            AASTORE | IASTORE | FASTORE | DASTORE | LASTORE | BASTORE | CASTORE | SASTORE => {
                Some(NpeAnalysisResult {
                    entity_type: NpeEntityType::ArrayStore,
                    entity_name: None,
                    class_name: None,
                    reason: "Cannot store to array because \"<local>\" is null".into(),
                })
            }
            ATHROW => Some(NpeAnalysisResult {
                entity_type: NpeEntityType::Throw,
                entity_name: None,
                class_name: None,
                reason: "Cannot throw exception because \"<local>\" is null".into(),
            }),
            MONITORENTER => Some(NpeAnalysisResult {
                entity_type: NpeEntityType::Monitor,
                entity_name: None,
                class_name: None,
                reason: "Cannot enter synchronized block because \"<local>\" is null"
                    .into(),
            }),
            _ => None,
        }
    }

    // -- private helpers --------------------------------------------------

    /// Read a big-endian u16 index from bytecode at `offset`.
    fn read_u16(bytecode: &[u8], offset: usize) -> Option<u16> {
        if offset + 1 >= bytecode.len() {
            return None;
        }
        Some(((bytecode[offset] as u16) << 8) | bytecode[offset + 1] as u16)
    }

    /// Analyse GETFIELD / PUTFIELD.
    fn analyze_field_access(
        bytecode: &[u8],
        bci: usize,
        is_put: bool,
    ) -> Option<NpeAnalysisResult> {
        let cp_index = Self::read_u16(bytecode, bci + 1)?;
        let field_name = format!("field_{}", cp_index);
        let verb = if is_put {
            "Cannot assign field"
        } else {
            "Cannot read field"
        };
        Some(NpeAnalysisResult {
            entity_type: NpeEntityType::Field,
            entity_name: Some(field_name.clone()),
            class_name: None,
            reason: format!(
                "{} \"{}\" because \"objectRef\" is null",
                verb, field_name
            ),
        })
    }

    /// Analyse INVOKEVIRTUAL / INVOKEINTERFACE.
    fn analyze_invoke(bytecode: &[u8], bci: usize) -> Option<NpeAnalysisResult> {
        let cp_index = Self::read_u16(bytecode, bci + 1)?;
        let method_name = format!("method_{}", cp_index);
        let class_name = format!("Class_{}", cp_index);
        Some(NpeAnalysisResult {
            entity_type: NpeEntityType::Method,
            entity_name: Some(method_name.clone()),
            class_name: Some(class_name.clone()),
            reason: format!(
                "Cannot invoke \"{}.{}()\" because \"objectRef\" is null",
                class_name, method_name
            ),
        })
    }

    /// Produce a message for a field access NPE with known names.
    pub fn field_npe_message(field_name: &str, object_ref: &str, is_put: bool) -> String {
        let verb = if is_put {
            "Cannot assign field"
        } else {
            "Cannot read field"
        };
        format!("{} \"{}\" because \"{}\" is null", verb, field_name, object_ref)
    }

    /// Produce a message for a virtual/interface invoke NPE with known names.
    pub fn invoke_npe_message(
        class_name: &str,
        method_name: &str,
        object_ref: &str,
    ) -> String {
        format!(
            "Cannot invoke \"{}.{}()\" because \"{}\" is null",
            class_name, method_name, object_ref
        )
    }

    /// Produce a message for an array-length NPE with known local name.
    pub fn array_length_npe_message(local: &str) -> String {
        format!("Cannot read the array length because \"{}\" is null", local)
    }

    /// Produce a message for an array load NPE.
    pub fn array_load_npe_message(local: &str) -> String {
        format!("Cannot load from array because \"{}\" is null", local)
    }

    /// Produce a message for an array store NPE.
    pub fn array_store_npe_message(local: &str) -> String {
        format!("Cannot store to array because \"{}\" is null", local)
    }

    /// Produce a message for a throw NPE.
    pub fn throw_npe_message(local: &str) -> String {
        format!("Cannot throw exception because \"{}\" is null", local)
    }

    /// Produce a message for a monitor-enter NPE.
    pub fn monitor_npe_message(local: &str) -> String {
        format!(
            "Cannot enter synchronized block because \"{}\" is null",
            local
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── SignalHandler basics ────────────────────────────────────────────

    #[test]
    fn signal_handler_new_is_empty() {
        let sh = SignalHandler::new();
        assert!(sh.registered_signals.is_empty());
        assert!(sh.shutdown_hooks.is_empty());
        assert!(!sh.shutdown_in_progress.load(Ordering::SeqCst));
    }

    #[test]
    fn register_signal_default() {
        let mut sh = SignalHandler::new();
        sh.register_signal(SIGINT, SignalAction::Default);
        assert!(sh.registered_signals.contains_key(&SIGINT));
    }

    #[test]
    fn register_signal_ignore() {
        let mut sh = SignalHandler::new();
        sh.register_signal(SIGTERM, SignalAction::Ignore);
        assert!(matches!(
            sh.registered_signals.get(&SIGTERM),
            Some(SignalAction::Ignore)
        ));
    }

    #[test]
    fn register_signal_handle() {
        fn my_handler(_sig: i32) {}
        let mut sh = SignalHandler::new();
        sh.register_signal(SIGUSR1, SignalAction::Handle(my_handler));
        assert!(matches!(
            sh.registered_signals.get(&SIGUSR1),
            Some(SignalAction::Handle(_))
        ));
    }

    #[test]
    fn register_signal_replaces_existing() {
        let mut sh = SignalHandler::new();
        sh.register_signal(SIGINT, SignalAction::Default);
        sh.register_signal(SIGINT, SignalAction::Ignore);
        assert!(matches!(
            sh.registered_signals.get(&SIGINT),
            Some(SignalAction::Ignore)
        ));
    }

    #[test]
    fn register_multiple_signals() {
        let mut sh = SignalHandler::new();
        sh.register_signal(SIGINT, SignalAction::Default);
        sh.register_signal(SIGTERM, SignalAction::Ignore);
        sh.register_signal(SIGQUIT, SignalAction::Default);
        assert_eq!(sh.registered_signals.len(), 3);
    }

    // ── Shutdown hooks ──────────────────────────────────────────────────

    #[test]
    fn add_shutdown_hook_ok() {
        let mut sh = SignalHandler::new();
        let hook = ShutdownHook::new_no_action(1, "h1".into(), 0);
        assert!(sh.add_shutdown_hook(hook).is_ok());
        assert_eq!(sh.shutdown_hooks.len(), 1);
    }

    #[test]
    fn add_shutdown_hook_fails_during_shutdown() {
        let mut sh = SignalHandler::new();
        sh.shutdown_in_progress.store(true, Ordering::SeqCst);
        let hook = ShutdownHook::new_no_action(1, "h1".into(), 0);
        let res = sh.add_shutdown_hook(hook);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("shutdown in progress"));
    }

    #[test]
    fn remove_shutdown_hook_found() {
        let mut sh = SignalHandler::new();
        sh.add_shutdown_hook(ShutdownHook::new_no_action(42, "h".into(), 0))
            .unwrap();
        assert!(sh.remove_shutdown_hook(42));
        assert!(sh.shutdown_hooks.is_empty());
    }

    #[test]
    fn remove_shutdown_hook_not_found() {
        let mut sh = SignalHandler::new();
        assert!(!sh.remove_shutdown_hook(99));
    }

    #[test]
    fn run_shutdown_hooks_empty() {
        let mut sh = SignalHandler::new();
        let result = sh.run_shutdown_hooks();
        assert_eq!(result.hooks_run, 0);
        assert_eq!(result.hooks_failed, 0);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn run_shutdown_hooks_no_action() {
        let mut sh = SignalHandler::new();
        sh.add_shutdown_hook(ShutdownHook::new_no_action(1, "a".into(), 0))
            .unwrap();
        sh.add_shutdown_hook(ShutdownHook::new_no_action(2, "b".into(), 0))
            .unwrap();
        let result = sh.run_shutdown_hooks();
        assert_eq!(result.hooks_run, 2);
        assert_eq!(result.hooks_failed, 0);
    }

    #[test]
    fn run_shutdown_hooks_with_action() {
        static CALLED: AtomicBool = AtomicBool::new(false);
        fn action() {
            CALLED.store(true, Ordering::SeqCst);
        }
        let mut sh = SignalHandler::new();
        sh.add_shutdown_hook(ShutdownHook::new(1, "act".into(), 0, action))
            .unwrap();
        let result = sh.run_shutdown_hooks();
        assert_eq!(result.hooks_run, 1);
        assert_eq!(result.hooks_failed, 0);
        assert!(CALLED.load(Ordering::SeqCst));
    }

    #[test]
    fn run_shutdown_hooks_respects_priority() {
        let mut sh = SignalHandler::new();
        sh.add_shutdown_hook(ShutdownHook::new_no_action(1, "low".into(), 10))
            .unwrap();
        sh.add_shutdown_hook(ShutdownHook::new_no_action(2, "high".into(), -5))
            .unwrap();
        sh.add_shutdown_hook(ShutdownHook::new_no_action(3, "mid".into(), 0))
            .unwrap();
        sh.run_shutdown_hooks();
        // After sort the order should be high (-5), mid (0), low (10).
        assert_eq!(sh.shutdown_hooks[0].name, "high");
        assert_eq!(sh.shutdown_hooks[1].name, "mid");
        assert_eq!(sh.shutdown_hooks[2].name, "low");
    }

    #[test]
    fn run_shutdown_hooks_marks_completed() {
        let mut sh = SignalHandler::new();
        sh.add_shutdown_hook(ShutdownHook::new_no_action(1, "h".into(), 0))
            .unwrap();
        sh.run_shutdown_hooks();
        assert_eq!(sh.shutdown_hooks[0].state, HookState::Completed);
    }

    #[test]
    fn initiate_shutdown_first_call_returns_true() {
        let sh = SignalHandler::new();
        assert!(sh.initiate_shutdown());
    }

    #[test]
    fn initiate_shutdown_second_call_returns_false() {
        let sh = SignalHandler::new();
        assert!(sh.initiate_shutdown());
        assert!(!sh.initiate_shutdown());
    }

    #[test]
    fn next_hook_id_increments() {
        let a = SignalHandler::next_hook_id();
        let b = SignalHandler::next_hook_id();
        assert!(b > a);
    }

    #[test]
    fn signal_handler_default_trait() {
        let sh = SignalHandler::default();
        assert!(sh.registered_signals.is_empty());
    }

    #[test]
    fn signal_action_debug_format() {
        let d = format!("{:?}", SignalAction::Default);
        assert_eq!(d, "Default");
        let i = format!("{:?}", SignalAction::Ignore);
        assert_eq!(i, "Ignore");
        fn noop(_: i32) {}
        let h = format!("{:?}", SignalAction::Handle(noop));
        assert_eq!(h, "Handle(<fn>)");
    }

    #[test]
    fn hook_state_equality() {
        assert_eq!(HookState::Registered, HookState::Registered);
        assert_ne!(HookState::Running, HookState::Completed);
        assert_eq!(
            HookState::Failed("x".into()),
            HookState::Failed("x".into())
        );
    }

    // ── ThreadDumper ────────────────────────────────────────────────────

    #[test]
    fn dump_empty_threads() {
        let dump = ThreadDumper::dump_all_threads(&[]);
        assert!(dump.is_empty());
    }

    #[test]
    fn dump_single_thread_no_frames() {
        let t = ThreadInfo {
            id: 1,
            name: "main".into(),
            state: "RUNNABLE".into(),
            stack_frames: vec![],
        };
        let dump = ThreadDumper::dump_all_threads(&[t]);
        assert!(dump.contains("\"main\" #1"));
        assert!(dump.contains("[RUNNABLE]"));
    }

    #[test]
    fn dump_thread_with_frames() {
        let t = ThreadInfo {
            id: 7,
            name: "worker-1".into(),
            state: "WAITING".into(),
            stack_frames: vec![
                StackFrame {
                    class_name: "com.example.App".into(),
                    method_name: "doWork".into(),
                    file_name: Some("App.java".into()),
                    line_number: 42,
                },
                StackFrame {
                    class_name: "com.example.App".into(),
                    method_name: "run".into(),
                    file_name: Some("App.java".into()),
                    line_number: 10,
                },
            ],
        };
        let dump = ThreadDumper::dump_all_threads(&[t]);
        assert!(dump.contains("at com.example.App.doWork(App.java:42)"));
        assert!(dump.contains("at com.example.App.run(App.java:10)"));
    }

    #[test]
    fn dump_thread_frame_unknown_source() {
        let t = ThreadInfo {
            id: 2,
            name: "t".into(),
            state: "BLOCKED".into(),
            stack_frames: vec![StackFrame {
                class_name: "X".into(),
                method_name: "m".into(),
                file_name: None,
                line_number: -1,
            }],
        };
        let dump = ThreadDumper::dump_all_threads(&[t]);
        assert!(dump.contains("(Unknown Source)"));
    }

    #[test]
    fn dump_thread_frame_file_no_line() {
        let t = ThreadInfo {
            id: 3,
            name: "t".into(),
            state: "RUNNABLE".into(),
            stack_frames: vec![StackFrame {
                class_name: "A".into(),
                method_name: "b".into(),
                file_name: Some("A.java".into()),
                line_number: 0,
            }],
        };
        let dump = ThreadDumper::dump_all_threads(&[t]);
        assert!(dump.contains("(A.java)"));
        assert!(!dump.contains("(A.java:0)"));
    }

    #[test]
    fn dump_multiple_threads() {
        let threads = vec![
            ThreadInfo {
                id: 1,
                name: "main".into(),
                state: "RUNNABLE".into(),
                stack_frames: vec![],
            },
            ThreadInfo {
                id: 2,
                name: "gc".into(),
                state: "WAITING".into(),
                stack_frames: vec![],
            },
        ];
        let dump = ThreadDumper::dump_all_threads(&threads);
        assert!(dump.contains("\"main\""));
        assert!(dump.contains("\"gc\""));
    }

    // ── NpeMessageGenerator — bytecode analysis ─────────────────────────

    #[test]
    fn npe_getfield() {
        // GETFIELD with cp index 0x0005
        let bytecode = vec![GETFIELD, 0x00, 0x05];
        let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
        assert!(msg.contains("Cannot read field"));
        assert!(msg.contains("is null"));
    }

    #[test]
    fn npe_putfield() {
        let bytecode = vec![PUTFIELD, 0x00, 0x0A];
        let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
        assert!(msg.contains("Cannot assign field"));
    }

    #[test]
    fn npe_invokevirtual() {
        let bytecode = vec![INVOKEVIRTUAL, 0x00, 0x03];
        let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
        assert!(msg.contains("Cannot invoke"));
        assert!(msg.contains("is null"));
    }

    #[test]
    fn npe_invokeinterface() {
        let bytecode = vec![INVOKEINTERFACE, 0x00, 0x07, 0x02, 0x00];
        let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
        assert!(msg.contains("Cannot invoke"));
    }

    #[test]
    fn npe_arraylength() {
        let bytecode = vec![ARRAYLENGTH];
        let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
        assert_eq!(
            msg,
            "Cannot read the array length because \"<local>\" is null"
        );
    }

    #[test]
    fn npe_aaload() {
        let bytecode = vec![AALOAD];
        let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
        assert!(msg.contains("Cannot load from array"));
    }

    #[test]
    fn npe_iaload() {
        let bytecode = vec![IALOAD];
        let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
        assert!(msg.contains("Cannot load from array"));
    }

    #[test]
    fn npe_aastore() {
        let bytecode = vec![AASTORE];
        let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
        assert!(msg.contains("Cannot store to array"));
    }

    #[test]
    fn npe_iastore() {
        let bytecode = vec![IASTORE];
        let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
        assert!(msg.contains("Cannot store to array"));
    }

    #[test]
    fn npe_athrow() {
        let bytecode = vec![ATHROW];
        let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
        assert!(msg.contains("Cannot throw exception"));
    }

    #[test]
    fn npe_monitorenter() {
        let bytecode = vec![MONITORENTER];
        let msg = NpeMessageGenerator::generate_message(&bytecode, 0, 0).unwrap();
        assert!(msg.contains("Cannot enter synchronized block"));
    }

    #[test]
    fn npe_unknown_opcode_returns_none() {
        let bytecode = vec![0x00]; // nop
        assert!(NpeMessageGenerator::generate_message(&bytecode, 0, 0).is_none());
    }

    #[test]
    fn npe_bci_out_of_bounds() {
        let bytecode = vec![GETFIELD, 0x00, 0x01];
        assert!(NpeMessageGenerator::generate_message(&bytecode, 10, 0).is_none());
    }

    #[test]
    fn npe_empty_bytecode() {
        assert!(NpeMessageGenerator::generate_message(&[], 0, 0).is_none());
    }

    #[test]
    fn npe_getfield_truncated() {
        // GETFIELD but only one operand byte — should return None (can't read u16).
        let bytecode = vec![GETFIELD, 0x01];
        assert!(NpeMessageGenerator::generate_message(&bytecode, 0, 0).is_none());
    }

    #[test]
    fn npe_analyze_returns_entity_type_field() {
        let bytecode = vec![GETFIELD, 0x00, 0x01];
        let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
        assert_eq!(r.entity_type, NpeEntityType::Field);
        assert!(r.entity_name.is_some());
    }

    #[test]
    fn npe_analyze_returns_entity_type_method() {
        let bytecode = vec![INVOKEVIRTUAL, 0x00, 0x02];
        let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
        assert_eq!(r.entity_type, NpeEntityType::Method);
        assert!(r.class_name.is_some());
    }

    #[test]
    fn npe_analyze_array_length_entity() {
        let bytecode = vec![ARRAYLENGTH];
        let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
        assert_eq!(r.entity_type, NpeEntityType::ArrayLength);
    }

    #[test]
    fn npe_analyze_array_store_entity() {
        let bytecode = vec![AASTORE];
        let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
        assert_eq!(r.entity_type, NpeEntityType::ArrayStore);
    }

    #[test]
    fn npe_analyze_monitor_entity() {
        let bytecode = vec![MONITORENTER];
        let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
        assert_eq!(r.entity_type, NpeEntityType::Monitor);
    }

    #[test]
    fn npe_analyze_throw_entity() {
        let bytecode = vec![ATHROW];
        let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
        assert_eq!(r.entity_type, NpeEntityType::Throw);
    }

    // ── Convenience message builders ────────────────────────────────────

    #[test]
    fn field_npe_message_get() {
        let msg = NpeMessageGenerator::field_npe_message("value", "this.obj", false);
        assert_eq!(
            msg,
            "Cannot read field \"value\" because \"this.obj\" is null"
        );
    }

    #[test]
    fn field_npe_message_put() {
        let msg = NpeMessageGenerator::field_npe_message("x", "ref", true);
        assert_eq!(msg, "Cannot assign field \"x\" because \"ref\" is null");
    }

    #[test]
    fn invoke_npe_message_format() {
        let msg = NpeMessageGenerator::invoke_npe_message("java.util.List", "size", "list");
        assert_eq!(
            msg,
            "Cannot invoke \"java.util.List.size()\" because \"list\" is null"
        );
    }

    #[test]
    fn array_length_npe_msg() {
        let msg = NpeMessageGenerator::array_length_npe_message("arr");
        assert_eq!(
            msg,
            "Cannot read the array length because \"arr\" is null"
        );
    }

    #[test]
    fn array_load_npe_msg() {
        let msg = NpeMessageGenerator::array_load_npe_message("data");
        assert_eq!(
            msg,
            "Cannot load from array because \"data\" is null"
        );
    }

    #[test]
    fn array_store_npe_msg() {
        let msg = NpeMessageGenerator::array_store_npe_message("buf");
        assert_eq!(
            msg,
            "Cannot store to array because \"buf\" is null"
        );
    }

    #[test]
    fn throw_npe_msg() {
        let msg = NpeMessageGenerator::throw_npe_message("ex");
        assert_eq!(
            msg,
            "Cannot throw exception because \"ex\" is null"
        );
    }

    #[test]
    fn monitor_npe_msg() {
        let msg = NpeMessageGenerator::monitor_npe_message("lock");
        assert_eq!(
            msg,
            "Cannot enter synchronized block because \"lock\" is null"
        );
    }

    // ── Signal constants ────────────────────────────────────────────────

    #[test]
    fn signal_constants_values() {
        assert_eq!(SIGINT, 2);
        assert_eq!(SIGTERM, 15);
        assert_eq!(SIGQUIT, 3);
        assert_eq!(SIGHUP, 1);
        assert_eq!(SIGUSR1, 10);
        assert_eq!(SIGUSR2, 12);
    }

    // ── NpeEntityType ───────────────────────────────────────────────────

    #[test]
    fn npe_entity_type_debug() {
        assert_eq!(format!("{:?}", NpeEntityType::Field), "Field");
        assert_eq!(format!("{:?}", NpeEntityType::Unresolved), "Unresolved");
    }

    // ── Bytecode at non-zero BCI ────────────────────────────────────────

    #[test]
    fn npe_getfield_at_nonzero_bci() {
        // Some prefix bytes, then GETFIELD at bci=3
        let bytecode = vec![0x00, 0x01, 0x02, GETFIELD, 0x00, 0x0A];
        let msg = NpeMessageGenerator::generate_message(&bytecode, 3, 0).unwrap();
        assert!(msg.contains("Cannot read field"));
        assert!(msg.contains("field_10"));
    }

    #[test]
    fn npe_additional_array_load_opcodes() {
        for &op in &[FALOAD, DALOAD, LALOAD, BALOAD, CALOAD, SALOAD] {
            let bytecode = vec![op];
            let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
            assert_eq!(r.entity_type, NpeEntityType::ArrayAccess);
        }
    }

    #[test]
    fn npe_additional_array_store_opcodes() {
        for &op in &[FASTORE, DASTORE, LASTORE, BASTORE, CASTORE, SASTORE] {
            let bytecode = vec![op];
            let r = NpeMessageGenerator::analyze(&bytecode, 0).unwrap();
            assert_eq!(r.entity_type, NpeEntityType::ArrayStore);
        }
    }
}
