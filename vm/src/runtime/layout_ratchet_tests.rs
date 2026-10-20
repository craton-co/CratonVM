//! The layout ratchet of the dispatch loop's hot structs (proposal
//! `i55-L7-proposal-a-layout-ratchet-for-the-dispatch-loop-and-its-hot-structs`,
//! interpreter round i2 wave 56, lane L7).
//!
//! Three times a wave has made `--nojit` loop and door rows 15-25% slower
//! with no change to the loop's source (round i1's "knife-edge" rows, and
//! `interpreter-orch-wave-53-door-and-loop-rows-are-slower-under-nojit-FIXED-20261007.md`):
//! one cause is the DATA the loop reads moving. Until wave 59 `JvmThread` was
//! one struct of well over a hundred fields that rustc orders by alignment,
//! so a field added anywhere moved every later field of its alignment class,
//! and with it which hot fields shared a cache line; a value type the site
//! caches copy (`CastSite`) grows and every hit copies more.
//!
//! This test freezes, exactly (as `native-builtins/tests/stub_ratchet.rs`
//! freezes the stub baseline), the size of each struct the loop reads per
//! bytecode or per call and the offset of the fields it reads, so a layout
//! move fails here, at commit time, instead of in a benchmark a wave later.
//!
//! Since wave 59 (lane L7, proposal
//! `docs/internal/fixed-bugs/interpreter-L7-proposal-one-layout-for-the-debugger-build-and-the-plain-build-FIXED-20261023.md`)
//! `JvmThread` and `FrameStack` are `#[repr(C)]` with their hot fields
//! declared first (`JvmThread` also 64-aligned, so an offset here is a cache
//! line), and `FrameStack`'s cold and debugger-only state is behind one
//! pointer (`FrameStackCold`). So:
//! * ONE table holds for the plain and the `experimental-debug` build (a
//!   workspace build enables that feature on `cratonvm-vm` for every binary,
//!   so either may be benchmarked): a field gated on a Cargo feature in a hot
//!   struct fails here in the debugger build;
//! * a field added below `JvmThread`'s hot head moves only
//!   `size_of JvmThread` (and only when it crosses a 64-byte step).
//!
//! When it fails:
//! * a COLD field you added (a raise mark, a hint, a debugger view, a
//!   diagnostic) goes below `JvmThread`'s head, or behind a `Box`
//!   (`Option<Box<_>>` is one word) in `FrameStack` / `Frame`, as wave 55
//!   boxed `served_native_raise`; then the numbers return to the frozen ones;
//! * a HOT field, or a deliberate layout change, updates the frozen row(s) in
//!   the SAME commit, with one line in the commit message saying why it is
//!   hot -- and the orchestrator's next host run measures it.
//!
//! The table is a property of the target and the toolchain: it is asserted
//! on 64-bit x86 only, and a toolchain bump that reorders fields refreezes it
//! (the failure prints the whole measured table, ready to paste). It lives
//! under `frame` (a child module) so it can name `Frame`'s private fields.

use super::*;
use crate::runtime::interpreter::site_cache::{
    CastSite, FastFieldSite, InterpSiteTables, MethodSiteInfo,
};
use crate::threading::JvmThread;
use std::mem::{align_of, offset_of, size_of};

/// Every measured row: `(name, bytes)`. A row's name says what it is.
fn measured() -> Vec<(&'static str, usize)> {
    vec![
        // The thread: read through `&mut JvmThread` everywhere in the loop.
        ("size_of JvmThread", size_of::<JvmThread>()),
        ("align_of JvmThread", align_of::<JvmThread>()),
        ("JvmThread.frames", offset_of!(JvmThread, frames)),
        (
            "JvmThread.running_native",
            offset_of!(JvmThread, running_native),
        ),
        (
            "JvmThread.invoke_cache",
            offset_of!(JvmThread, invoke_cache),
        ),
        (
            "JvmThread.method_sites",
            offset_of!(JvmThread, method_sites),
        ),
        ("JvmThread.field_sites", offset_of!(JvmThread, field_sites)),
        // Since wave 83 (lane L7) the three tables the fused helpers read
        // live in one `InterpSiteTables` field; the rows keep their names.
        (
            "JvmThread.fast_field_sites",
            offset_of!(JvmThread, site_tables) + offset_of!(InterpSiteTables, fast_field_sites),
        ),
        (
            "JvmThread.static_field_sites",
            offset_of!(JvmThread, site_tables) + offset_of!(InterpSiteTables, static_field_sites),
        ),
        ("JvmThread.class_sites", offset_of!(JvmThread, class_sites)),
        ("JvmThread.cast_sites", offset_of!(JvmThread, cast_sites)),
        (
            "JvmThread.prim_const_sites",
            offset_of!(JvmThread, site_tables) + offset_of!(InterpSiteTables, prim_const_sites),
        ),
        (
            "JvmThread.iface_select_sites",
            offset_of!(JvmThread, iface_select_sites),
        ),
        ("JvmThread.indy_sites", offset_of!(JvmThread, indy_sites)),
        (
            "JvmThread.sig_poly_sites",
            offset_of!(JvmThread, sig_poly_sites),
        ),
        (
            "JvmThread.vh_exact_site_memo",
            offset_of!(JvmThread, vh_exact_site_memo),
        ),
        (
            "JvmThread.jit_monitor_block",
            offset_of!(JvmThread, jit_monitor_block),
        ),
        ("JvmThread.tlab", offset_of!(JvmThread, tlab)),
        (
            "JvmThread.shadow_stack",
            offset_of!(JvmThread, shadow_stack),
        ),
        (
            "JvmThread.gc_block_state",
            offset_of!(JvmThread, gc_block_state),
        ),
        ("JvmThread.thread_id", offset_of!(JvmThread, thread_id)),
        ("JvmThread.kind", offset_of!(JvmThread, kind)),
        // The first field below the hot head: where the head ends.
        ("JvmThread.name", offset_of!(JvmThread, name)),
        // The frame stack: `thread.frames[frame_idx]` on every access.
        ("size_of FrameStack", size_of::<FrameStack>()),
        ("FrameStack.buf", offset_of!(FrameStack, buf)),
        ("FrameStack.depth", offset_of!(FrameStack, depth)),
        ("FrameStack.code_moves", offset_of!(FrameStack, code_moves)),
        // The frame: built and installed per call, read per bytecode.
        ("size_of Frame", size_of::<Frame>()),
        ("Frame.class_id", offset_of!(Frame, class_id)),
        ("Frame.pc", offset_of!(Frame, pc)),
        ("Frame.last_instr_pc", offset_of!(Frame, last_instr_pc)),
        ("Frame.locals", offset_of!(Frame, locals)),
        ("Frame.local_kinds", offset_of!(Frame, local_kinds)),
        ("Frame.stack", offset_of!(Frame, stack)),
        ("Frame.code", offset_of!(Frame, code)),
        ("Frame.backward_count", offset_of!(Frame, backward_count)),
        ("Frame.osr_poll_at", offset_of!(Frame, osr_poll_at)),
        ("Frame.inner", offset_of!(Frame, inner)),
        ("size_of ValueStack", size_of::<ValueStack>()),
        // The site-cache values a hit reads (or, for the cast site's copying
        // protocol, copies).
        ("size_of CastSite", size_of::<CastSite>()),
        ("size_of FastFieldSite", size_of::<FastFieldSite>()),
        ("size_of MethodSiteInfo", size_of::<MethodSiteInfo>()),
    ]
}

/// The frozen table (x86_64), for the plain AND the `experimental-debug`
/// build. Measured on interpreter round i2 wave 59, lane L7's branch (the
/// one-layout commit), on Windows, in both builds; first frozen on the
/// wave-56 base (`3b1ac2e592`). Every inline field type here has the same
/// size on x86_64 Linux (no `std::sync` lock, an `Instant` 16 bytes on both).
/// See the module docs before changing a row.
const FROZEN: &[(&str, usize)] = &[
    // gci round (2026-10-13): 3136 -> 3200 (one 64-byte line), cold state
    // added by the GC round (i8/dm and i9/cs grew `name_override_memo`'s
    // `NameOverrideMemo`; i9/jw and i9/ds per-thread publish cells); no hot
    // offset below moved.
    ("size_of JvmThread", 3200),
    ("align_of JvmThread", 64),
    ("JvmThread.frames", 0),
    ("JvmThread.running_native", 1112),
    ("JvmThread.invoke_cache", 128),
    // Wave 83 (lane L7): `site_tables` (72 bytes: the fast-field, static
    // and constant tables, 24 each) replaced `fast_field_sites` (224) and
    // `static_field_sites` (248) in place and absorbed `prim_const_sites`
    // (368): the constant table moves 368 -> 272, the 24 bytes it adds there
    // push `trivial_getter_verdicts`, `method_sites`, `class_sites` and
    // `cast_sites` down 24 each, and from `iface_select_sites` (392) on
    // nothing moves (the 24 bytes it vacated at 368 are reused).
    ("JvmThread.method_sites", 320),
    ("JvmThread.field_sites", 200),
    ("JvmThread.fast_field_sites", 224),
    ("JvmThread.static_field_sites", 248),
    ("JvmThread.class_sites", 344),
    ("JvmThread.cast_sites", 368),
    ("JvmThread.prim_const_sites", 272),
    ("JvmThread.iface_select_sites", 392),
    ("JvmThread.indy_sites", 416),
    ("JvmThread.sig_poly_sites", 440),
    // 3000 -> 3032, 2026-10-11: dev added a 32-byte COLD field ahead of it; every hot row and
    // size_of JvmThread are unchanged. 3032 -> 3056 (gci round, 2026-10-13): gci i8/dm and i9/cs
    // grew the COLD `name_override_memo` (`NameOverrideMemo`, declared ahead of it in the cold
    // tail) by 24 bytes -- the same growth the 3136 -> 3200 row above records; the memo itself is
    // cold (read only at a thin VarHandle helper's slow path), so no hot row moved.
    // Interpreter round i5 wave 110 (lane L6, `vt_slice_started`, cold): 3056 -> 3072;
    // size_of JvmThread unchanged.
    ("JvmThread.vh_exact_site_memo", 3072),
    ("JvmThread.jit_monitor_block", 992),
    ("JvmThread.tlab", 672),
    ("JvmThread.shadow_stack", 952),
    ("JvmThread.gc_block_state", 1080),
    ("JvmThread.thread_id", 1144),
    ("JvmThread.kind", 1152),
    ("JvmThread.name", 1160),
    ("size_of FrameStack", 128),
    ("FrameStack.buf", 0),
    ("FrameStack.depth", 24),
    ("FrameStack.code_moves", 36),
    ("size_of Frame", 240),
    ("Frame.class_id", 224),
    ("Frame.pc", 144),
    ("Frame.last_instr_pc", 152),
    ("Frame.locals", 48),
    ("Frame.local_kinds", 64),
    ("Frame.stack", 80),
    ("Frame.code", 128),
    ("Frame.backward_count", 228),
    ("Frame.osr_poll_at", 232),
    ("Frame.inner", 0),
    ("size_of ValueStack", 48),
    ("size_of CastSite", 72),
    ("size_of FastFieldSite", 20),
    ("size_of MethodSiteInfo", 24),
];

#[test]
fn the_dispatch_loops_hot_structs_keep_their_layout() {
    let measured = measured();
    let table: String = measured
        .iter()
        .map(|(name, bytes)| format!("    (\"{name}\", {bytes}),\n"))
        .collect();
    eprintln!(
        "[layout-ratchet] measured (experimental-debug: {}):\n{table}",
        cfg!(feature = "experimental-debug")
    );
    if !cfg!(target_arch = "x86_64") {
        return; // frozen for x86_64 only
    }
    let moved: Vec<String> = measured
        .iter()
        .zip(FROZEN.iter().map(Some).chain(std::iter::repeat(None)))
        .filter_map(|((name, bytes), frozen)| match frozen {
            Some((fname, fbytes)) if fname == name && fbytes == bytes => None,
            Some((fname, fbytes)) => Some(format!("{name}: {bytes} (frozen {fname}: {fbytes})")),
            None => Some(format!("{name}: {bytes} (no frozen row)")),
        })
        .collect();
    assert!(
        moved.is_empty() && measured.len() == FROZEN.len(),
        "the dispatch loop's hot structs moved{}:\n  {}\n\
         One table holds for the plain and the experimental-debug build: a field \
         gated on a Cargo feature does not belong in a hot struct (FrameStack's \
         go in FrameStackCold). Declare a COLD JvmThread field below the hot \
         head, or box it (`Option<Box<_>>`); for a HOT field or a deliberate \
         change, update FROZEN in the same commit and say why in the message. \
         Measured table:\n{table}",
        if cfg!(feature = "experimental-debug") {
            " in the experimental-debug build"
        } else {
            ""
        },
        moved.join("\n  ")
    );
}
