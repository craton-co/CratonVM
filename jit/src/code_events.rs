// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Publication and retirement of executable JIT code, as seen by external
//! profilers and debuggers.
//!
//! Three sinks consume these events, each behind its own default-OFF flag:
//!
//! | sink | flag | platforms | module |
//! |---|---|---|---|
//! | `perf-<pid>.map` | `CRATONVM_JIT_PERF_MAP` | all (`/tmp` or `%TEMP%`) | [`crate::perf_map`] |
//! | `jit-<pid>.dump` | `CRATONVM_JIT_JITDUMP` | Linux | [`crate::jitdump`] |
//! | GDB JIT interface | `CRATONVM_JIT_GDB` | Linux | [`crate::gdb_jit`] |
//!
//! # Where the events come from
//!
//! Every executable region is an [`crate::ExecutableBuffer`], but a buffer has
//! no name and no final length when it is allocated, so publication is hooked
//! where the name is known:
//!
//! * `JitCache::put` — method-entry bodies from every backend (single-pass,
//!   IR, aarch64);
//! * `JitCache::put_osr` — OSR bodies;
//! * `lambda_adapter` — lambda adapter thunks;
//! * `osr_trampoline` — OSR entry trampolines, when a freshly emitted one is
//!   about to be cached (a racing loser is dropped, and retired, at once).
//!
//! Deopt, bounds-check, null-check and local-handler stubs are emitted INSIDE
//! the method body's buffer, so they are covered by the body's range and have
//! no separate record.
//!
//! Retirement has exactly one hook, `ExecutableBuffer::drop`, which every
//! unmapping passes through; it withdraws every region inside the buffer. Only
//! the GDB sink acts on it: a perf map is append-only (and perf does not
//! guarantee that the latest of two overlapping lines wins — see
//! [`crate::perf_map`]), and jitdump version 1 has no unload record (perf
//! resolves a reused address by record timestamp).
//!
//! # Cost when off
//!
//! [`publish`] reads three cached booleans and returns before the name closure
//! runs. [`retire`] reads one atomic and returns unless a GDB entry was ever
//! registered, so the drop path never reads a flag.

/// Which kind of code a published region holds, for its name suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeTier {
    /// Single-pass (baseline) method-entry body.
    C1,
    /// Optimizing IR method-entry body.
    C2,
    /// OSR body; the bytecode pc it was compiled for, when recorded.
    Osr(Option<usize>),
    /// Out-of-body stub, named by kind (`osr-trampoline`, `lambda-adapter`).
    Stub(&'static str),
}

/// `java/lang/String.hashCode()I [c1]`, `... [c2]`, `... [osr@<bci>]`, or
/// `<label> [stub:<kind>]`.
pub fn method_name(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    tier: CodeTier,
) -> String {
    let base = format!("{class_name}.{method_name}{descriptor}");
    with_tier_suffix(&base, tier)
}

/// Append the tier suffix [`method_name`] uses to an already-formed label.
pub fn with_tier_suffix(label: &str, tier: CodeTier) -> String {
    match tier {
        CodeTier::C1 => format!("{label} [c1]"),
        CodeTier::C2 => format!("{label} [c2]"),
        CodeTier::Osr(Some(bci)) => format!("{label} [osr@{bci}]"),
        CodeTier::Osr(None) => format!("{label} [osr]"),
        CodeTier::Stub(kind) => format!("{label} [stub:{kind}]"),
    }
}

/// Make a name safe for every sink: no line breaks (a perf map is
/// line-oriented) and no NUL (jitdump and ELF string tables are
/// NUL-terminated). Spaces are kept; perf accepts them.
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '\n' | '\r' | '\0'))
        .collect()
}

/// Shared parse for the three opt-in flags: `1`/`true`/`on`/`yes` enable,
/// anything else (and unset) leaves the sink off. The same accepted words as
/// `aarch64_backend::arm64_jit_enabled`.
pub(crate) fn flag_is_on(value: Result<String, std::env::VarError>) -> bool {
    matches!(
        value
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Ok("1") | Ok("true") | Ok("on") | Ok("yes")
    )
}

/// Whether any sink wants publication events.
pub fn any_enabled() -> bool {
    crate::perf_map::enabled() || crate::jitdump::enabled() || crate::gdb_jit::enabled()
}

/// `[start, start+len)` became executable and reachable under `name`.
///
/// `name` is only evaluated when a sink is on. The region must be mapped and
/// readable for the duration of the call: the jitdump sink copies its bytes.
pub fn publish(start: usize, len: usize, name: impl FnOnce() -> String) {
    if start == 0 || len == 0 || !any_enabled() {
        return;
    }
    let name = sanitize_name(&name());
    crate::perf_map::record(start, len, &name);
    crate::jitdump::record_load(start, len, &name);
    crate::gdb_jit::register(start, len, &name);
}

/// The buffer `[start, start+len)` is about to be unmapped. Every region
/// published inside it is withdrawn, so a body whose entry is not the first
/// byte of its buffer is not left behind naming reused memory.
pub fn retire(start: usize, len: usize) {
    if start == 0 {
        return;
    }
    crate::gdb_jit::unregister_range(start, len.max(1));
    if LIVE_ANY.load(std::sync::atomic::Ordering::Relaxed) {
        retire_live(start, len.max(1));
    }
}

// ---------------------------------------------------------------------------
// The JVMTI sink (interpreter round i2 wave 63, lane L1)
// ---------------------------------------------------------------------------
//
// The C JVMTI table's `CompiledMethodLoad` / `CompiledMethodUnload` /
// `DynamicCodeGenerated` and `GenerateEvents` (`vm` crate,
// `jvmti::native_env::events_more`): every method body and stub published
// with its identity is kept here while it lives ([`live_code`]), and handed
// to the sink the VM installs while an agent listens ([`set_jvmti_sink`]).

/// A code event for JVMTI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JvmtiCodeEvent {
    /// A compiled body of a method became reachable.
    MethodLoad {
        class: String,
        name: String,
        descriptor: String,
        start: usize,
        len: usize,
    },
    /// A compiled body was withdrawn (its buffer unmapped).
    MethodUnload {
        class: String,
        name: String,
        descriptor: String,
        start: usize,
    },
    /// A stub (an OSR trampoline, a lambda adapter) was generated.
    Stub {
        name: String,
        start: usize,
        len: usize,
    },
}

/// The sink the VM installs.
pub type JvmtiSink = fn(&JvmtiCodeEvent);

static JVMTI_SINK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Set once anything was kept in [`LIVE`]: [`retire`]'s one load.
static LIVE_ANY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The live method bodies and stubs, as their load events.
static LIVE: std::sync::Mutex<Vec<JvmtiCodeEvent>> = std::sync::Mutex::new(Vec::new());

/// Install (or remove, `None`) the JVMTI sink.
pub fn set_jvmti_sink(sink: Option<JvmtiSink>) {
    JVMTI_SINK.store(
        sink.map_or(0, |f| f as usize),
        std::sync::atomic::Ordering::Release,
    ); // Cast: the fn pointer's address
}

fn jvmti_sink() -> Option<JvmtiSink> {
    let raw = JVMTI_SINK.load(std::sync::atomic::Ordering::Acquire);
    // SAFETY: only `set_jvmti_sink` stores a non-zero value, a `JvmtiSink`.
    (raw != 0).then(|| unsafe { std::mem::transmute::<usize, JvmtiSink>(raw) })
}

/// Keep `event` while its code lives and hand it to the sink.
fn note_load(event: JvmtiCodeEvent) {
    if let Some(sink) = jvmti_sink() {
        sink(&event);
    }
    if let Ok(mut live) = LIVE.lock() {
        live.push(event);
        LIVE_ANY.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A compiled body of `class.name descriptor` at `[start, start+len)` became
/// reachable: kept for `GenerateEvents`, and `CompiledMethodLoad` through the
/// sink. Where [`publish`] names a method body.
pub fn publish_method(start: usize, len: usize, class: &str, name: &str, descriptor: &str) {
    if start == 0 || len == 0 {
        return;
    }
    note_load(JvmtiCodeEvent::MethodLoad {
        class: class.to_string(),
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        start,
        len,
    });
}

/// A stub named `name` at `[start, start+len)` was generated:
/// `DynamicCodeGenerated`. Where [`publish`] names a stub.
pub fn publish_stub(start: usize, len: usize, name: &str) {
    if start == 0 || len == 0 {
        return;
    }
    note_load(JvmtiCodeEvent::Stub {
        name: name.to_string(),
        start,
        len,
    });
}

/// The buffer `[start, start+len)` is unmapped: its bodies leave [`LIVE`],
/// each method's as a `CompiledMethodUnload` through the sink.
fn retire_live(start: usize, len: usize) {
    let removed: Vec<JvmtiCodeEvent> = match LIVE.lock() {
        Ok(mut live) => {
            let inside = |at: usize| at >= start && at < start.saturating_add(len);
            let (gone, kept): (Vec<_>, Vec<_>) = live.drain(..).partition(|e| match e {
                JvmtiCodeEvent::MethodLoad { start: s, .. }
                | JvmtiCodeEvent::Stub { start: s, .. } => inside(*s),
                JvmtiCodeEvent::MethodUnload { .. } => false,
            });
            *live = kept;
            gone
        }
        Err(_) => return,
    };
    let Some(sink) = jvmti_sink() else {
        return;
    };
    for event in removed {
        if let JvmtiCodeEvent::MethodLoad {
            class,
            name,
            descriptor,
            start,
            ..
        } = event
        {
            sink(&JvmtiCodeEvent::MethodUnload {
                class,
                name,
                descriptor,
                start,
            });
        }
    }
}

/// The live code, as load events: the stubs (`stubs`) or the method bodies.
pub fn live_code(stubs: bool) -> Vec<JvmtiCodeEvent> {
    LIVE.lock()
        .map(|live| {
            live.iter()
                .filter(|e| matches!(e, JvmtiCodeEvent::Stub { .. }) == stubs)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_carry_the_tier_suffix() {
        assert_eq!(
            method_name("java/lang/String", "hashCode", "()I", CodeTier::C1),
            "java/lang/String.hashCode()I [c1]"
        );
        assert_eq!(
            method_name("java/lang/String", "hashCode", "()I", CodeTier::C2),
            "java/lang/String.hashCode()I [c2]"
        );
        assert_eq!(
            method_name("a/B", "loop", "([I)J", CodeTier::Osr(Some(17))),
            "a/B.loop([I)J [osr@17]"
        );
        assert_eq!(
            method_name("a/B", "loop", "([I)J", CodeTier::Osr(None)),
            "a/B.loop([I)J [osr]"
        );
        assert_eq!(
            with_tier_suffix("lambda-adapter->0x10", CodeTier::Stub("lambda-adapter")),
            "lambda-adapter->0x10 [stub:lambda-adapter]"
        );
    }

    #[test]
    fn sanitizing_strips_line_breaks_and_nul_but_keeps_spaces() {
        assert_eq!(sanitize_name("a\nb\r\nc\0d e"), "abcd e");
    }

    #[test]
    fn flag_words() {
        assert!(flag_is_on(Ok("1".into())));
        assert!(flag_is_on(Ok(" TRUE ".into())));
        assert!(flag_is_on(Ok("yes".into())));
        assert!(!flag_is_on(Ok("0".into())));
        assert!(!flag_is_on(Ok("".into())));
        assert!(!flag_is_on(Err(std::env::VarError::NotPresent)));
    }
}
