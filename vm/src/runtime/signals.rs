// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Signal action registry, thread dumping, and helpful NullPointerException
//! messages.
//!
//! - **Phase 16.1**: Signal registration and thread dumping.
//! - **Phase 16.2**: JEP 358 — bytecode-level NPE analysis producing HotSpot-style messages.
//!
//! # There is no shutdown-hook mechanism in this file, and there used to be
//!
//! Until 2026-08-17 `SignalHandler` also carried `shutdown_hooks`,
//! `add_shutdown_hook`, `remove_shutdown_hook`, `run_shutdown_hooks`,
//! `initiate_shutdown`, a `ShutdownHook` struct with a priority and a
//! `HookState`, and a `ShutdownResult`. **Nothing in the tree ever called any
//! of it** — the only callers were the unit tests in this file, which passed,
//! so the whole thing read as a working, tested shutdown facility. W7-92 §9.3
//! filed it as "wire it or delete it", and it is deleted here rather than
//! wired, for three reasons:
//!
//! * it was `fn()`-valued and ran hooks INLINE on the caller's thread. Java
//!   shutdown hooks are `Thread`s that run CONCURRENTLY — MEASURED on Temurin
//!   25.0.3+9 (`HookProbe crosswait`: one hook blocks on a `CountDownLatch`
//!   another hook counts down, and it is released). An inline runner
//!   deadlocks that shape, so this was not an unfinished version of the real
//!   thing; it was a different, wrong thing.
//! * it ran hooks in PRIORITY order. The JDK has no hook priority and no
//!   ordering guarantee at all — SOURCE-VERIFIED in
//!   `ApplicationShutdownHooks.runHooks` (start every hook, then join every
//!   hook), MEASURED as 2, 1, 4, 0, 3 for five hooks registered 0..4.
//! * `add_shutdown_hook` refused with the string `"Cannot add shutdown hook:
//!   shutdown in progress"`; HotSpot throws
//!   `IllegalStateException("Shutdown in progress")`.
//!
//! The real registry, and the only one, is `SHUTDOWN_HOOKS` in
//! `native-builtins/src/lang_system.rs`, drained by
//! `lang_system::run_shutdown_hooks`.
//!
//! What survives here is the SIGNAL half, `registered_signals` plus
//! `register_signal`, and it is honest about being a table and nothing more:
//! no OS handler is installed from it and no signal in this VM is converted
//! into a call to `lang_system::run_shutdown_hooks`. That door is still shut —
//! see the NOMINATION on `native-builtins/src/lib.rs`'s
//! `jdk/internal/misc/Signal.handle0` comment in
//! `docs/known-issues/jdk-only/G11-1-shutdown-hooks-and-the-process-cluster-20260817.md`.

use std::collections::HashMap;

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

// ── SignalHandler ───────────────────────────────────────────────────────────

/// Registry of the action chosen for each signal number.
///
/// A TABLE, and only a table. Building one of these installs no OS handler,
/// and no signal delivered to this process is routed through it. See the
/// module header for what used to be bolted onto this struct and why it is
/// gone.
pub struct SignalHandler {
    pub registered_signals: HashMap<i32, SignalAction>,
}

impl SignalHandler {
    pub fn new() -> Self {
        Self {
            registered_signals: HashMap::new(),
        }
    }

    /// Register (or replace) the action for a given signal number.
    pub fn register_signal(&mut self, signal: i32, action: SignalAction) {
        self.registered_signals.insert(signal, action);
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
    pub fn generate_message(bytecode: &[u8], bci: usize, _stack_depth: usize) -> Option<String> {
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
                reason: "Cannot enter synchronized block because \"<local>\" is null".into(),
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
            reason: format!("{} \"{}\" because \"objectRef\" is null", verb, field_name),
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
        format!(
            "{} \"{}\" because \"{}\" is null",
            verb, field_name, object_ref
        )
    }

    /// Produce a message for a virtual/interface invoke NPE with known names.
    pub fn invoke_npe_message(class_name: &str, method_name: &str, object_ref: &str) -> String {
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
        assert_eq!(msg, "Cannot read the array length because \"arr\" is null");
    }

    #[test]
    fn array_load_npe_msg() {
        let msg = NpeMessageGenerator::array_load_npe_message("data");
        assert_eq!(msg, "Cannot load from array because \"data\" is null");
    }

    #[test]
    fn array_store_npe_msg() {
        let msg = NpeMessageGenerator::array_store_npe_message("buf");
        assert_eq!(msg, "Cannot store to array because \"buf\" is null");
    }

    #[test]
    fn throw_npe_msg() {
        let msg = NpeMessageGenerator::throw_npe_message("ex");
        assert_eq!(msg, "Cannot throw exception because \"ex\" is null");
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
