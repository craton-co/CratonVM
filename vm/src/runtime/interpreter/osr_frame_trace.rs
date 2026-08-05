// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The literal form of the `osr-02` brief's item 2.
//!
//! > For an OSR exit at pc *p*, the interpreter must resume at *p* with the
//! > locals and stack the compiled frame held. Assert it: a test that enters
//! > OSR, forces an exit, and **compares the resumed frame against the frame an
//! > un-compiled run would have had at the same iteration count**.
//!
//! `osr-02-exit-and-recompile-RETIRED-20260804.md` closed that item with a
//! *behavioural* oracle — per-execution side effects and a per-iteration digest
//! of the loop-carried state — and named the gap it left in as many words:
//! **a divergence in a local the rest of the loop never reads would not be
//! seen.** This module closes that gap by comparing the frames themselves,
//! slot for slot, tag included.
//!
//! # Two records, one format
//!
//! Under `CRATONVM_DBG_OSR_FRAME_TRACE=<class-substring>` the VM emits three
//! kinds of line to stderr, and they are deliberately the same shape so a
//! checker can compare them without knowing which produced which:
//!
//! ```text
//! [osr-frame] A key=Probe.loop:(I)J bci=8 n=417 L=3:0000000000000000,1:00000000000001a1 S=
//! [osr-frame] E key=Probe.loop:(I)J bci=8 n=- L=3:0000000000000000,1:00000000000001a1 S=
//! [osr-frame] X key=Probe.loop:(I)J bci=8 n=- L=3:0000000000000000,1:0000000000000209 S=
//! ```
//!
//! * **`A`** — a back-edge **arrival**, taken at the one funnel every one of the
//!   fourteen back-edge sites goes through (`try_osr_with_backoff`), *before*
//!   any of its early returns, so the record does not depend on whether OSR is
//!   enabled, whether the thread is virtual, or where the backoff schedule is.
//!   `n` is the arrival index for this `(key, bci)`, counted by this module.
//! * **`E`** — the frame an OSR **entry** was taken with: the state compiled
//!   code starts from.
//! * **`X`** — the frame an OSR **exit** transferred into the live frame,
//!   emitted from `transfer_osr_exit_into_live_frame_checked` *after* the write
//!   and therefore describing what the interpreter will actually resume on.
//!   Neither carries an `n`: what index each *should* be is the question.
//!
//! # Why `E` exists — a hole a synthetic fixture found
//!
//! Without it the checker's rule ("map every record to its ground-truth index
//! and require the index to increase strictly") **does not catch a replay**,
//! and a hand-written fixture modelling the historical defect passed. The
//! reason is structural: compiled iterations produce no arrival records, so
//! "entered at frame 5, ran to frame 12, resumed at frame 5" and "entered at
//! frame 5 and resumed at frame 5 having advanced nothing" are the same
//! sequence. Both then continue 6, 7, 8… — strictly increasing, and wrong.
//!
//! Recording the ENTRY frame makes the advance measurable:
//! `index(X) - index(E)` is how many iterations the compiled body committed,
//! derived from the un-compiled run's own trajectory rather than from anything
//! the JIT claims. Under `CRATONVM_OSR_EXIT_AFTER=N` with `N >= 2` that
//! difference must be non-zero, and a replay drives it to zero.
//!
//! Zero is *correct* for the unconditional-at-header trigger
//! (`CRATONVM_OSR_EXIT_TEST`), which bails at iteration 0 where "reject" and
//! "transfer" coincide — so the floor is the checker's parameter, not a
//! constant.
//!
//! Each slot is `tag:word`, both hex, in JVM slot order — the same
//! `get_local_raw` / `get_local_tag` pair the OSR entry contract reads, so a
//! disagreement here is a disagreement in the words OSR itself acts on. The
//! operand stack comes from `ValueStack::snapshot_raw`.
//!
//! # What the checker does with them
//!
//! Two runs of the same program:
//!
//! * **Ground truth** — `--nojit`, so every back edge is interpreted and every
//!   arrival is recorded. This is "the frame an un-compiled run would have had
//!   at the same iteration count", as a sequence indexed by arrival.
//! * **Under test** — OSR armed with a forced exit.
//!
//! Then, for the run under test, map every record (`A` *and* `X`) to its index
//! in the ground-truth sequence by **exact frame equality**, and require that
//! index sequence to be **strictly increasing**. That single property is the
//! brief's assertion and the lane's defect in one:
//!
//! * a resumed frame that matches **no** ground-truth frame is a state the
//!   program can never be in — a corrupted local, however dead;
//! * an index that **repeats or goes backwards** is an iteration executing a
//!   second time, which is the `jit-osr-bail-reruns-loop-iterations` defect;
//! * a *gap* is not an error: compiled code legitimately runs the iterations
//!   between an entry and its exit without producing arrival records.
//!
//! # Cost, and why the filter is mandatory
//!
//! The flag's value is a substring matched against the frame's class name, and
//! it is **required** — an empty filter would trace every back edge in the JDK
//! and the trace would be both useless and enormous. With the flag unset this
//! module costs one `OnceLock` bool load per back edge, on a path that already
//! performs several cached environment reads.
//!
//! Records are capped per `(key, bci)` (see [`MAX_RECORDS_PER_SITE`]); the cap
//! is **reported** when it bites, because a truncated ground truth would make
//! later frames look unmatched and that must not read as a defect.

use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Per-`(key, bci)` record cap. A counted loop of a few hundred thousand trips
/// is the shape this traces, and one line per arrival at ~80 bytes puts a
/// full run in the tens of megabytes — fine for a harness, not for a default.
/// Beyond the cap the site stops emitting and says so exactly once.
const MAX_RECORDS_PER_SITE: u64 = 200_000;

/// The class-name substring the trace is restricted to, or `None` when the
/// flag is unset. Read once: this is consulted on the back-edge path.
fn filter() -> Option<&'static str> {
    static F: OnceLock<Option<String>> = OnceLock::new();
    F.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_DBG_OSR_FRAME_TRACE")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
    .as_deref()
}

/// Is the frame trace armed at all? The predicate the hot path checks first.
#[inline]
pub(super) fn enabled() -> bool {
    filter().is_some()
}

/// Arrival counters, keyed by `(method key, bci)`.
///
/// A `Mutex<HashMap>` rather than anything cleverer: this is only ever touched
/// under a diagnostic flag, and a lock-free counter would have to be per-site
/// storage threaded through the interpreter for no benefit to any real run.
fn arrivals() -> &'static Mutex<HashMap<(String, usize), u64>> {
    static A: OnceLock<Mutex<HashMap<(String, usize), u64>>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How many sites hit [`MAX_RECORDS_PER_SITE`]. Reported once per site.
static TRUNCATED: AtomicU64 = AtomicU64::new(0);

/// `tag:word` for every local slot, then for every operand-stack entry.
///
/// Locals go through the same `get_local_raw` / `get_local_tag` pair the OSR
/// entry contract reads (`Frame::get_local_tag` exists "for JIT/OSR interop"),
/// so what this compares is what OSR itself acts on rather than a re-derived
/// view of it.
/// One slot, as `tag:word` — except for a **reference**, which renders as
/// `tag:null` or `tag:ref`.
///
/// A reference slot holds a raw heap address, and addresses are not stable
/// across processes. The comparison this feeds runs two separate runs of one
/// program, so rendering the pointer would make every frame containing any
/// object reference compare unequal — which is exactly what happened the first
/// time this was run: `report(Ljava/lang/String;J)V` failed with
/// `L=4:0000020ef1b11330,…` against a ground truth holding a different address
/// for the same `String`.
///
/// Dropping the identity is the honest trade, and it is a real loss, stated
/// here rather than in a comment nobody reads: **a reference retargeted to a
/// different non-null object is invisible to this comparison.** Null-vs-non-null
/// still shows, which is the shape a mis-seeded or clobbered reference usually
/// takes (the recorded OSR dead-local defects surfaced as null/garbage
/// pointers). A comparison that catches every integral and FP divergence exactly
/// and reference *nullness* is worth more than one that is red on every frame.
fn slot(tag: u8, word: u64) -> String {
    match tag {
        cratonvm_types::VTAG_OBJECT => {
            if word == 0 {
                format!("{tag:x}:null")
            } else {
                format!("{tag:x}:ref")
            }
        }
        cratonvm_types::VTAG_NULL => format!("{tag:x}:null"),
        _ => format!("{tag:x}:{word:016x}"),
    }
}

/// Is local slot `i` nothing but the reserved upper half of a category-2 value?
///
/// A `long`/`double` at slot `N` reserves `N+1`, and **`lload N` reads the full
/// value from `N`** — the reserved slot is never read. The two arms disagree
/// about what is in it, legitimately and by design: the interpreter's own
/// `lstore` leaves the tag `LONG` there, while the OSR-exit transfer writes the
/// snapshot's `Undefined` through `fv_to_value` as `Int(0)`. `deopt_resume.rs`
/// argues that at length and calls it "the correct two-slot JVM layout".
///
/// Comparing it anyway made every resumed frame differ from every ground-truth
/// frame — the first real run of this comparator failed on exactly that, in a
/// slot the JVM guarantees nobody reads. Excluding it is what keeps the
/// comparison sensitive to the slots that matter.
/// Which local slots are nothing but the reserved upper half of a category-2
/// value, as a forward scan.
///
/// It has to be a forward scan, not a look-behind at slot `i-1`. The
/// interpreter leaves the tag `LONG` in the reserved slot itself, so
/// "slot `i-1` is tagged `LONG` ⇒ `i` is a high half" **cascades**: `acc`'s
/// reserved slot marks the next real local as reserved too, and so on down the
/// frame. That is not a hypothetical — it swallowed `mix` on the first run,
/// which is how the rule got written this way.
///
/// A base is a `LONG`/`DOUBLE` slot that is not itself already claimed.
fn cat2_high_halves(frame: &Frame) -> Vec<bool> {
    let n = frame.locals_len();
    let mut hi = vec![false; n];
    let mut i = 0;
    while i < n {
        if !hi[i]
            && matches!(
                frame.get_local_tag(i),
                cratonvm_types::VTAG_LONG | cratonvm_types::VTAG_DOUBLE
            )
            && i + 1 < n
        {
            hi[i + 1] = true;
            i += 2;
            continue;
        }
        i += 1;
    }
    hi
}

fn render(frame: &Frame) -> (String, String) {
    let hi = cat2_high_halves(frame);
    let mut locals = String::new();
    for i in 0..frame.locals_len() {
        if i > 0 {
            locals.push(',');
        }
        if hi[i] {
            locals.push_str("hi");
            continue;
        }
        locals.push_str(&slot(frame.get_local_tag(i), frame.get_local_raw(i)));
    }
    let (words, tags) = frame.stack.snapshot_raw();
    let mut stack = String::new();
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            stack.push(',');
        }
        stack.push_str(&slot(tags.get(i).copied().unwrap_or(0), *w));
    }
    (locals, stack)
}

/// `"<class>.<method><descriptor>"` — the key both record kinds are grouped by.
fn key_of(frame: &Frame) -> String {
    format!(
        "{}.{}{}",
        frame.class_name(),
        frame.method_name(),
        frame.method_descriptor()
    )
}

/// Record a back-edge **arrival**: the interpreter is standing at `bci` with
/// zero bytes of the next iteration executed.
///
/// Called from `try_osr_with_backoff` before every one of its early returns, so
/// the ground-truth run (`--nojit`, OSR never fires) and the run under test
/// produce records from the same site under the same conditions. A trace whose
/// two arms were taken at different points would compare two different things.
pub(crate) fn record_arrival(frame: &Frame, bci: usize) {
    let Some(f) = filter() else { return };
    if !frame.class_name().contains(f) {
        return;
    }
    let key = key_of(frame);
    let n = {
        let mut map = arrivals().lock().unwrap_or_else(|e| e.into_inner());
        let c = map.entry((key.clone(), bci)).or_insert(0);
        *c += 1;
        *c - 1
    };
    if n >= MAX_RECORDS_PER_SITE {
        if n == MAX_RECORDS_PER_SITE {
            TRUNCATED.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "[osr-frame] TRUNCATED key={key} bci={bci} after {MAX_RECORDS_PER_SITE} \
                 arrivals — a checker must treat later frames as UNKNOWN, not unmatched"
            );
        }
        return;
    }
    let (l, s) = render(frame);
    eprintln!("[osr-frame] A key={key} bci={bci} n={n} L={l} S={s}");
}

/// Record the frame an OSR **entry** is about to be taken with.
///
/// Emitted after the entry validates and before the trampoline runs, so it is
/// the state compiled code actually starts from. Paired with the next `X` at
/// the same site, it makes the compiled body's advance measurable — see the
/// module note for the replay a fixture slipped past without it.
pub(crate) fn record_entry(frame: &Frame, bci: usize) {
    let Some(f) = filter() else { return };
    if !frame.class_name().contains(f) {
        return;
    }
    let (l, s) = render(frame);
    eprintln!(
        "[osr-frame] E key={} bci={bci} n=- L={l} S={s}",
        key_of(frame)
    );
}

/// Record the frame an OSR **exit** transferred into the live frame.
///
/// Emitted after the write, so it describes what the interpreter will actually
/// resume on — not what the reconstruction proposed. The two differ whenever a
/// slot is deliberately left at its live value (an `Unsupported` source slot),
/// and it is the resumed frame the brief asks about.
pub(crate) fn record_exit(frame: &Frame, bci: usize) {
    let Some(f) = filter() else { return };
    if !frame.class_name().contains(f) {
        return;
    }
    let (l, s) = render(frame);
    eprintln!(
        "[osr-frame] X key={} bci={bci} n=- L={l} S={s}",
        key_of(frame)
    );
}
