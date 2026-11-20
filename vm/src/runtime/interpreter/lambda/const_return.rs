use std::sync::atomic::{AtomicU64, Ordering};

/// The constant an int-category method body returns unconditionally, if it is
/// one of those bodies.
///
/// Only the shapes javac emits for `return <literal>;` — a push and an
/// `ireturn`, nothing else in the method. `CompletionStages.alwaysTrue` is
/// `iconst_1; ireturn`, two bytes. Deliberately narrow: the point is a body
/// whose correct answer is knowable WITHOUT running it, so anything requiring
/// analysis is not a candidate.
///
/// `ireturn` covers the whole int category — `boolean`, `byte`, `char`,
/// `short`, `int` — which is where the functional interfaces that matter here
/// (`IntPredicate`, `Predicate`, `BiPredicate`) return.
pub(crate) fn const_int_return_of(code: &[u8], has_handlers: bool) -> Option<i32> {
    const IRETURN: u8 = 0xAC;
    if has_handlers {
        // A handler can transfer control to a bci past the `ireturn`, so the
        // prefix below would no longer describe every path through the body.
        return None;
    }
    // A PREFIX match, not an exact-length one: this VM's `code` slice is
    // zero-padded (`alwaysTrue` arrives as `[04, ac, 00, 00]` — `iconst_1;
    // ireturn; nop; nop`), and an exact `[push, IRETURN]` pattern silently
    // matched nothing at all on the very method this probe was written for.
    //
    // Sound because the prefix ENDS in an unconditional return and contains no
    // branch: with no handlers, nothing can reach a later bci, so whatever
    // follows cannot execute.
    match *code {
        // iconst_m1 .. iconst_5
        [op @ 0x02..=0x08, IRETURN, ..] => Some(i32::from(op) - 0x03),
        // bipush n
        [0x10, n, IRETURN, ..] => Some(i32::from(n as i8)),
        // sipush n
        [0x11, hi, lo, IRETURN, ..] => Some(i32::from(i16::from_be_bytes([hi, lo]))),
        _ => None,
    }
}

/// Calls screened by the constant-return probe, and what they found.
///
/// UNGATED atomics rather than a debug-only structure: a correctness counter
/// that only exists when a debug variable is set cannot answer "did this ever
/// happen" after the fact, and the whole value of this probe is that ONE wrong
/// call is proof.
static CONST_SCREENED: AtomicU64 = AtomicU64::new(0);
static CONST_MISMATCH: AtomicU64 = AtomicU64::new(0);
static CONST_DIRTY_HIGH: AtomicU64 = AtomicU64::new(0);
static CONST_OPAQUE: AtomicU64 = AtomicU64::new(0);

/// Count one SITE whose calls this probe cannot see — a constant-returning
/// impl that was given an emitted inline-cache thunk. The thunk tail-jumps to
/// the impl and returns straight to its compiled caller, so no Rust runs on
/// that path and every call it serves goes unscreened.
///
/// A site count, not a call count: the calls are exactly what cannot be
/// counted here.
///
/// This is the probe's honesty counter. A run with `site_const_mismatch=0` and
/// a large `site_const_opaque` has not cleared the impl of anything; it has
/// only failed to look. `CRATONVM_JIT_LAMBDA_CONST_PROBE=strict` refuses those
/// thunks so the number goes to zero — at the cost of the fast path, and of
/// changing the very codegen under suspicion.
pub(crate) fn const_probe_note_opaque() {
    CONST_OPAQUE.fetch_add(1, Ordering::Relaxed);
}

/// Screen one observed return value against the constant its body must return.
///
/// `raw` is the full JIT-ABI register word. The Java value is its low 32 bits,
/// so that is what decides a MISMATCH; a correct low half carried in a word
/// with junk above it is counted separately, because a caller that reads the
/// register at the wrong width would see the junk and not the value.
pub(crate) fn const_probe_screen(
    expected: i32,
    raw: i64,
    impl_class: &str,
    impl_name: &str,
    arm: &str,
) {
    CONST_SCREENED.fetch_add(1, Ordering::Relaxed);
    let observed = raw as i32;
    if observed != expected {
        let n = CONST_MISMATCH.fetch_add(1, Ordering::Relaxed);
        if n < 32 {
            eprintln!(
                "[cratonvm-lambda-const] WRONG ANSWER from a constant body: \
                 {impl_class}.{impl_name} must return {expected}, observed {observed} \
                 (raw={raw:#x}) via {arm}"
            );
        }
        return;
    }
    if (raw as u64) >> 32 != 0 {
        let n = CONST_DIRTY_HIGH.fetch_add(1, Ordering::Relaxed);
        if n < 8 {
            eprintln!(
                "[cratonvm-lambda-const] {impl_class}.{impl_name} returned the right \
                 int ({expected}) in a word with a dirty upper half (raw={raw:#x}) via {arm}"
            );
        }
    }
}

/// `(screened, mismatched, dirty-upper-half, opaque)` — the constant-return
/// probe's counters, for tests and for the census line.
pub(crate) fn lambda_const_probe_counts() -> (u64, u64, u64, u64) {
    (
        CONST_SCREENED.load(Ordering::Relaxed),
        CONST_MISMATCH.load(Ordering::Relaxed),
        CONST_DIRTY_HIGH.load(Ordering::Relaxed),
        CONST_OPAQUE.load(Ordering::Relaxed),
    )
}

/// Screen an interpreted/one-shot SAM result against the constant its body must
/// return. The `Value`-typed twin of `jit::helpers::screen_const_return`.
///
/// A non-int `Value` from a body whose bytecode ends in `ireturn` is itself
/// wrong, and is reported as such rather than skipped: this arm reconstructs
/// the return value, so a coercion that lost the type is exactly the kind of
/// defect worth catching here.
pub(crate) fn screen_const_return_value(
    cached: &CachedBytecodeMethod,
    value: Option<&Value>,
    arm: &str,
) {
    if !crate::runtime::env_cache::jit_lambda_const_probe() {
        return;
    }
    let Some(expected) = const_int_return_of(&cached.code, !cached.exception_table.is_empty())
    else {
        return;
    };
    let raw = match value {
        Some(Value::Int(v)) => i64::from(*v),
        // Anything else is already a mismatch. `i64::from(expected) ^ 1` can
        // never equal the expectation, so the screen reports it as the wrong
        // answer it is instead of silently agreeing.
        _ => i64::from(expected) ^ 1,
    };
    const_probe_screen(expected, raw, &cached.class_name, &cached.method_name, arm);
}
