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
//! unmapping passes through; it withdraws every region inside the buffer. Only the GDB sink acts on it: a perf map is
//! append-only (perf takes the latest entry covering an address), and jitdump
//! version 1 has no unload record (perf resolves a reused address by record
//! timestamp).
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
