// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
//! `docs/feature-designs/concurrent-gc-maturation.md`.

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
mod tests;
