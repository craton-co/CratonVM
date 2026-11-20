use super::*;
use std::collections::HashMap;

/// A [`ZBarrierContext`] backed by plain maps, so a test can state exactly
/// what the collector "knows" and then assert on what the barrier did.
///
/// Note that `marking` / `relocating` are plain `bool`s set at
/// construction: the tests share this by `&` across threads and never
/// mutate a phase mid-run, so interior mutability would only add noise.
/// `forwarding` and `marked` need it and have it.
///
/// **Its masks are bare color bits with no [`Z_COLORED_TAG`], on purpose.**
/// These tests exercise the barrier's *mechanism* against abstract masks,
/// which is the whole point of the [`ZBarrierContext`] seam, and keeping the
/// tag out of them keeps every expected word readable as
/// `address | color`. The tag clause of the implementor contract is covered
/// separately by [`VaddrShapedContext`] below, which uses the real
/// `zgc::vaddr` encoding end to end.
struct TestBarrierContext {
    good: u64,
    heal: u64,
    marking: bool,
    relocating: bool,
    forwarding: Mutex<HashMap<u64, u64>>,
    marked: Mutex<Vec<u64>>,
    stats: ZBarrierStats,
    /// When set, `forward` answers `None` — the error channel added with
    /// finding N4. Set at construction like `marking` / `relocating`.
    forward_fails: bool,
}

impl TestBarrierContext {
    /// Marking phase: good color is MARKED0, bad is everything else.
    fn marking() -> Self {
        Self {
            good: Z_MARKED0,
            heal: Z_MARKED0,
            marking: true,
            relocating: false,
            forwarding: Mutex::new(HashMap::new()),
            marked: Mutex::new(Vec::new()),
            stats: ZBarrierStats::new(),
            forward_fails: false,
        }
    }

    /// Relocation phase: good color is REMAPPED.
    fn relocating() -> Self {
        Self {
            good: Z_REMAPPED,
            heal: Z_REMAPPED,
            marking: false,
            relocating: true,
            forwarding: Mutex::new(HashMap::new()),
            marked: Mutex::new(Vec::new()),
            stats: ZBarrierStats::new(),
            forward_fails: false,
        }
    }

    /// Relocation phase whose `forward` cannot answer. Marking is left ON
    /// so the "nothing was marked" assertion means something.
    fn relocating_with_failing_forward() -> Self {
        Self {
            good: Z_REMAPPED,
            heal: Z_REMAPPED,
            marking: true,
            relocating: true,
            forwarding: Mutex::new(HashMap::new()),
            marked: Mutex::new(Vec::new()),
            stats: ZBarrierStats::new(),
            forward_fails: true,
        }
    }

    fn forward_to(&self, from: u64, to: u64) {
        self.forwarding.lock().insert(from, to);
    }

    fn marked_addrs(&self) -> Vec<u64> {
        self.marked.lock().clone()
    }
}

impl ZBarrierContext for TestBarrierContext {
    fn good_mask(&self) -> u64 {
        self.good
    }
    fn heal_color(&self) -> u64 {
        self.heal
    }
    fn is_marking(&self) -> bool {
        self.marking
    }
    fn is_relocating(&self) -> bool {
        self.relocating
    }
    fn forward(&self, addr: u64) -> Option<u64> {
        if self.forward_fails {
            // Finding N4's error channel: "the collector could not answer".
            return None;
        }
        let map = self.forwarding.lock();
        Some(map.get(&addr).copied().unwrap_or(addr))
    }
    fn mark_live(&self, addr: u64) {
        self.marked.lock().push(addr);
    }
    fn stats(&self) -> &ZBarrierStats {
        &self.stats
    }
}

// -----------------------------------------------------------------------
// Fast path
// -----------------------------------------------------------------------

/// This module must not carry a second copy of the encoding. Pin the
/// re-exports to `zgc::vaddr`'s definitions so a future divergence is a
/// failing test rather than a silently mis-colored heap.
///
/// The values themselves are asserted in `vaddr`'s own
/// `matches_openjdk_bit_positions`; what is asserted here is *identity*.
#[test]
fn the_encoding_constants_are_vaddrs_and_not_a_local_copy() {
    use crate::zgc::vaddr;
    assert_eq!(Z_MARKED0, vaddr::Z_MARKED0);
    assert_eq!(Z_MARKED1, vaddr::Z_MARKED1);
    assert_eq!(Z_REMAPPED, vaddr::Z_REMAPPED);
    assert_eq!(Z_FINALIZABLE, vaddr::Z_FINALIZABLE);
    assert_eq!(Z_METADATA_MASK, vaddr::Z_METADATA_MASK);
    assert_eq!(Z_OFFSET_MASK, vaddr::Z_OFFSET_MASK);
    assert_eq!(Z_COLORED_TAG, vaddr::Z_COLORED_TAG);
    assert_eq!(Z_NULL, vaddr::Z_NULL);

    // And they are OpenJDK's, not the simulation's. The simulation in
    // `zgc.rs` puts REMAPPED at 1<<42 and MARKED0 at 1<<43; mixing the two
    // encodings in one word is the bug this reconciliation removed.
    assert_eq!(Z_MARKED0, 1u64 << 42);
    assert_eq!(Z_MARKED1, 1u64 << 43);
    assert_eq!(Z_REMAPPED, 1u64 << 44);
    assert_eq!(Z_FINALIZABLE, 1u64 << 45);
    assert_ne!(Z_REMAPPED, 1u64 << 42, "this is the simulation's REMAPPED");
}

#[test]
fn classify_good_bad_and_null() {
    assert_eq!(
        classify(0x1000 | Z_MARKED0, Z_MARKED0),
        ZFastPath::Good(0x1000)
    );
    assert_eq!(
        classify(0x1000 | Z_REMAPPED, Z_MARKED0),
        ZFastPath::Bad(0x1000 | Z_REMAPPED)
    );
    // Null is good regardless of mask: nothing to mark, nowhere to move.
    assert_eq!(classify(0, Z_MARKED0), ZFastPath::Good(0));
}

#[test]
fn the_bad_mask_form_agrees_with_the_good_mask_form() {
    // The two forms must agree for every well-formed colored word — that
    // is the premise under which `z_barrier` switched to the bad form.
    let bad_mask = Z_MARKED0 ^ Z_METADATA_MASK;
    for c in [Z_MARKED0, Z_MARKED1, Z_REMAPPED, Z_FINALIZABLE] {
        let word = Z_COLORED_TAG | c | 0x1000;
        assert_eq!(
            classify_bad(word, bad_mask),
            classify(word, Z_MARKED0),
            "forms disagreed for color {c:#x}"
        );
    }
    // Null: both forms say Good, and the bad form needs no null test.
    assert_eq!(classify_bad(Z_NULL, bad_mask), ZFastPath::Good(0));
    assert_eq!(
        Z_NULL & bad_mask,
        0,
        "null must AND to zero — that IS the test"
    );
}

/// The regression `zgc::vaddr`'s tag bit introduced, and the reason the
/// null test is now on the whole word.
///
/// A colored word for an object at heap **offset 0** has a zero address
/// payload but is emphatically not null: it carries `Z_COLORED_TAG` and a
/// color bit. The original `raw & address_mask == 0` null test classified
/// it `Good(0)` — handing the mutator a null for a live object, which is
/// the silent half of a use-after-free.
#[test]
fn an_object_at_heap_offset_zero_is_never_mistaken_for_null() {
    use crate::zgc::vaddr;

    let at_offset_zero = vaddr::color(0, vaddr::ZColor::Remapped);
    assert_ne!(at_offset_zero, Z_NULL);
    assert_eq!(at_offset_zero & Z_OFFSET_MASK, 0, "payload IS zero");

    // Fast path, good color: the word is good, and it is not null.
    assert_eq!(
        classify(at_offset_zero, Z_REMAPPED),
        ZFastPath::Good(0),
        "offset 0 with a good color resolves to offset 0"
    );
    // Fast path, bad color: it must reach the slow path, not be waved
    // through as null.
    let bad_mask = Z_MARKED0 ^ Z_METADATA_MASK;
    assert_eq!(
        classify_bad(at_offset_zero, bad_mask),
        ZFastPath::Bad(at_offset_zero),
        "a stale word for offset 0 must take the slow path"
    );

    // ... and the slow path must heal it rather than short-circuit to null.
    let ctx = TestBarrierContext::marking();
    let slot = AtomicU64::new(at_offset_zero);
    assert_eq!(z_load(&slot, &ctx), 0);
    assert_eq!(
        ctx.stats().null_slow_paths.load(Ordering::Relaxed),
        0,
        "offset 0 is not the null observation"
    );
    assert_eq!(ctx.stats().heal_cas_wins.load(Ordering::Relaxed), 1);
    assert_eq!(
        ctx.marked_addrs(),
        vec![0],
        "the object at offset 0 is live"
    );
}

// -----------------------------------------------------------------------
// The real `zgc::vaddr` encoding, tag bit included
// -----------------------------------------------------------------------

/// A [`ZBarrierContext`] whose masks are the actual `zgc::vaddr` ones —
/// including [`Z_COLORED_TAG`] in `heal_color`, which is the clause of the
/// implementor contract the mask-only test contexts above do not exercise.
struct VaddrShapedContext {
    good: u64,
    heal: u64,
    stats: ZBarrierStats,
}

impl ZBarrierContext for VaddrShapedContext {
    fn good_mask(&self) -> u64 {
        self.good
    }
    fn heal_color(&self) -> u64 {
        self.heal
    }
    fn is_marking(&self) -> bool {
        false
    }
    fn is_relocating(&self) -> bool {
        false
    }
    fn forward(&self, addr: u64) -> Option<u64> {
        Some(addr)
    }
    fn mark_live(&self, _addr: u64) {}
    fn stats(&self) -> &ZBarrierStats {
        &self.stats
    }
}

/// A heal must produce a word `vaddr` calls well-formed — which means it
/// must carry the tag. This is the positive half of the contract clause.
#[test]
fn a_heal_with_a_tagged_heal_color_produces_a_well_formed_colored_word() {
    use crate::zgc::vaddr;

    let ctx = VaddrShapedContext {
        good: Z_MARKED0,
        heal: Z_MARKED0 | Z_COLORED_TAG,
        stats: ZBarrierStats::new(),
    };
    let stale = vaddr::color(0x3000, vaddr::ZColor::Remapped);
    let slot = AtomicU64::new(stale);

    assert_eq!(
        z_load(&slot, &ctx),
        0x3000,
        "the barrier returns the OFFSET"
    );

    let healed = slot.load(Ordering::Relaxed);
    assert_eq!(healed, vaddr::color(0x3000, vaddr::ZColor::Marked0));
    assert!(vaddr::is_well_formed(healed));
    assert!(vaddr::is_colored_word(healed));
    assert!(
        !cratonvm_types::plausible_heap_pointer(healed),
        "a healed word must still trip the escaped-colored-word tripwire"
    );
}

/// The negative half: an **explicitly untagged** `heal_color` produces a
/// word `vaddr` rejects, and this test exists so that fact is a visible,
/// named property rather than a footnote nobody reads.
///
/// # Premise changed 2026-08-07 — this used to be the trait's default
///
/// The doc here read "the trait's DEFAULT `heal_color` (the good mask) is
/// not sufficient under `vaddr`". That was true and is no longer: the
/// default is now `Z_COLORED_TAG | good_mask()`
/// ([`ZBarrierContext::heal_color`] carries the reasoning). So this context
/// has to *override* `heal_color` to reach the bad state, which is exactly
/// the point — the damage is still real, it is just no longer what an
/// implementor gets for free. The positive counterpart is
/// [`the_default_heal_color_carries_vaddrs_tag_bit`].
#[test]
fn an_untagged_heal_color_produces_a_word_vaddr_rejects() {
    use crate::zgc::vaddr;

    let ctx = VaddrShapedContext {
        good: Z_MARKED0,
        heal: Z_MARKED0, // deliberately missing Z_COLORED_TAG
        stats: ZBarrierStats::new(),
    };
    let slot = AtomicU64::new(vaddr::color(0x3000, vaddr::ZColor::Remapped));

    let _ = z_load(&slot, &ctx);

    let healed = slot.load(Ordering::Relaxed);
    assert!(
        !vaddr::is_well_formed(healed),
        "an untagged heal must be detectable as malformed"
    );
    assert!(
        cratonvm_types::plausible_heap_pointer(healed),
        "and this is the damage: the healed word now passes the tripwire \
         bit 63 exists to trip, so it can be dereferenced as a wild pointer"
    );
}

/// A [`ZBarrierContext`] that overrides nothing it does not have to — in
/// particular it takes the **default** `heal_color`, `bad_mask` and
/// `address_mask`.
///
/// This is the shape of the first real implementor: someone wiring
/// `ZgcRealHeap` writes the four required methods and lets the defaults
/// carry the encoding. Everything the defaults get wrong, this context gets
/// wrong, which is why the two tests below use it rather than a context that
/// spells the answer out.
struct DefaultsOnlyContext {
    good: u64,
    stats: ZBarrierStats,
}

impl ZBarrierContext for DefaultsOnlyContext {
    fn good_mask(&self) -> u64 {
        self.good
    }
    fn is_marking(&self) -> bool {
        false
    }
    fn is_relocating(&self) -> bool {
        false
    }
    fn forward(&self, addr: u64) -> Option<u64> {
        Some(addr)
    }
    fn mark_live(&self, _addr: u64) {}
    fn stats(&self) -> &ZBarrierStats {
        &self.stats
    }
}

/// FINDING C (2026-08-07). The trait's **default** `heal_color` must produce
/// a word `vaddr` calls well-formed.
///
/// Before the fix the default was plain `good_mask()`, so an implementor who
/// took it healed every slot it touched into a word missing
/// [`Z_COLORED_TAG`]. Two things went wrong at once, and the second is the
/// one that bites:
///
/// * [`vaddr::is_well_formed`] and [`vaddr::is_colored_word`] both reject
///   such a word, so every "is this a colored word?" audit in the tree reads
///   the barrier's own output as a plain machine pointer; and
/// * `cratonvm_types::plausible_heap_pointer` **accepts** it — bit 63 is
///   what pushes a colored word out of the 47-bit window that check assumes.
///   So the escaped-colored-word tripwire does not trip, and the word is
///   dereferenceable.
///
/// This test would have caught it: it uses the defaults and asserts the
/// healed word against `vaddr` end to end.
#[test]
fn the_default_heal_color_carries_vaddrs_tag_bit() {
    use crate::zgc::vaddr;

    let ctx = DefaultsOnlyContext {
        good: Z_MARKED0,
        stats: ZBarrierStats::new(),
    };
    assert_eq!(
        ctx.heal_color(),
        Z_COLORED_TAG | Z_MARKED0,
        "the default heal_color must carry the tag, not just the colour",
    );

    let slot = AtomicU64::new(vaddr::color(0x7000, vaddr::ZColor::Remapped));
    assert_eq!(z_load(&slot, &ctx), 0x7000);

    let healed = slot.load(Ordering::Relaxed);
    assert_eq!(
        healed,
        vaddr::color(0x7000, vaddr::ZColor::Marked0),
        "the default heal must build exactly the word vaddr::color would",
    );
    assert!(
        vaddr::is_well_formed(healed),
        "a context that overrides nothing must still heal to a well-formed \
         vaddr word",
    );
    assert!(vaddr::is_colored_word(healed));
    assert!(
        !cratonvm_types::plausible_heap_pointer(healed),
        "the healed word must still trip the escaped-colored-word tripwire; \
         this is the assertion the old default failed",
    );

    // And the healed slot hits the fast path, which is the whole point of
    // healing: the tag must not make the word bad.
    assert_eq!(
        classify(healed, ctx.good_mask()),
        ZFastPath::Good(0x7000),
        "the tag must live outside good/bad classification",
    );
    assert_eq!(ctx.stats().slow_path_entries.load(Ordering::Relaxed), 1);
    let _ = z_load(&slot, &ctx);
    assert_eq!(
        ctx.stats().slow_path_entries.load(Ordering::Relaxed),
        1,
        "a slot healed with the default colour must not re-enter the slow path",
    );

    // The trait's own overlap invariant still holds for the new default.
    assert_eq!(
        ctx.heal_color() & ctx.address_mask(),
        0,
        "heal_color must not overlap address_mask",
    );
}

/// FINDING B (2026-08-07). The barrier's address domain is the 42-bit heap
/// **offset**, and the default `address_mask` is forced by the encoding
/// rather than being a stand-in for a wider machine address.
///
/// The cross-module sweep read the 42-bit default as a gap against `vaddr`'s
/// `Z_MAX_ADDRESS` (47 bits) and `page.rs`'s real process addresses. It is
/// not: `Z_MAX_ADDRESS` bounds where the *reservation* may sit, while
/// `Z_OFFSET_MASK` bounds what a *slot* holds. Comparing them is a category
/// error, and this test is the record of which constant governs which domain.
#[test]
fn the_default_address_mask_is_the_offset_domain_and_cannot_be_widened() {
    use crate::zgc::vaddr;

    let ctx = DefaultsOnlyContext {
        good: Z_MARKED0,
        stats: ZBarrierStats::new(),
    };
    assert_eq!(ctx.address_mask(), Z_OFFSET_MASK);
    assert_eq!(Z_OFFSET_BITS, 42);

    // Forced, part 1: the metadata field begins exactly where the offset
    // field ends, so there is not one spare bit to widen into.
    assert_eq!(
        Z_METADATA_SHIFT, Z_OFFSET_BITS,
        "the colour bits start at bit 42; widening the address mask past 42 \
         bits would overlap them",
    );
    assert_eq!(Z_OFFSET_MASK & Z_METADATA_MASK, 0);
    assert_eq!(Z_OFFSET_MASK & Z_COLORED_TAG, 0);

    // Forced, part 2: a mask wide enough for a machine address would break
    // the trait's own "heal_color must not overlap address_mask" invariant,
    // which is asserted on every slow path.
    let machine_width_mask = vaddr::Z_MAX_ADDRESS;
    assert_ne!(
        machine_width_mask & Z_METADATA_MASK,
        0,
        "a 47-bit address mask overlaps the metadata field; healing would \
         corrupt the colour of every slot it touched",
    );

    // The two constants are bounds on two DIFFERENT domains. This is the
    // relationship, stated so nobody re-files it as a defect.
    assert!(
        Z_OFFSET_MASK < vaddr::Z_MAX_ADDRESS,
        "Z_OFFSET_MASK (what a slot holds) is deliberately narrower than \
         Z_MAX_ADDRESS (where the reservation may sit); they are not \
         comparable quantities and neither bounds the other",
    );
    assert_eq!(vaddr::Z_MAX_HEAP_SIZE, Z_OFFSET_MASK + 1);

    // And the barrier behaves accordingly: a colored word yields its offset,
    // wherever the heap happens to live.
    let word = vaddr::color(0x1_2345_6780, vaddr::ZColor::Marked0);
    assert_eq!(
        classify(word, ctx.good_mask()),
        ZFastPath::Good(0x1_2345_6780),
        "the fast path yields the OFFSET, never a machine pointer",
    );
}

/// FINDING B (2026-08-07), the runtime half: the check that catches a
/// `forward` implementation answering in the wrong domain.
///
/// Asserted on the predicate rather than by driving [`load_barrier_slow`]
/// into its `debug_assert`, deliberately: a `#[should_panic]` test here
/// would pass in debug and fail in release, and cfg-dependent test
/// behaviour is a documented trap in this repo. The predicate is the same
/// one the slow path evaluates, in both profiles.
#[test]
fn is_bare_offset_separates_a_heap_offset_from_a_machine_pointer() {
    use crate::zgc::vaddr;

    // Offsets: anything the fast path can produce, by construction.
    assert!(
        is_bare_offset(0, Z_OFFSET_MASK),
        "offset 0 is a real location"
    );
    assert!(is_bare_offset(0x3000, Z_OFFSET_MASK));
    assert!(
        is_bare_offset(Z_OFFSET_MASK, Z_OFFSET_MASK),
        "the top of vaddr's 4 TiB heap is still a bare offset",
    );

    // A Linux heap reservation. `Vec<u8>` lands near 0x7f… there, and this
    // is the value a `forward` that returned a machine pointer would hand
    // back. It must be rejected — on Windows, where the same reservation is
    // placed low, nothing would look wrong at all.
    // (~47 bits, just under Z_MAX_ADDRESS — a legal reservation address by
    // vaddr's own rule, and five bits past the 42-bit offset field.)
    const LINUX_SHAPED_HEAP_ADDRESS: u64 = 0x7f12_3456_7890;
    assert!(
        !is_bare_offset(LINUX_SHAPED_HEAP_ADDRESS, Z_OFFSET_MASK),
        "a machine pointer must not pass the address-domain check",
    );
    assert!(!is_bare_offset(vaddr::Z_MAX_ADDRESS, Z_OFFSET_MASK));

    // A still-coloured word is the other way to get this wrong: `forward`
    // must strip, not pass through.
    let coloured = vaddr::color(0x3000, vaddr::ZColor::Remapped);
    assert!(
        !is_bare_offset(coloured, Z_OFFSET_MASK),
        "a coloured word carries the tag and a metadata bit; both are \
         outside the address mask",
    );
    assert!(is_bare_offset(vaddr::offset_of(coloured), Z_OFFSET_MASK));

    // The predicate is exactly what the slow path's healed word depends on:
    // stray bits would land in the metadata field.
    assert_ne!(
        LINUX_SHAPED_HEAP_ADDRESS & Z_METADATA_MASK,
        0,
        "this is the damage the check prevents — the stray bits of a \
         machine pointer fall in the colour field",
    );
}

#[test]
fn fast_path_on_good_slot_does_not_touch_the_slot() {
    let ctx = TestBarrierContext::marking();
    let initial = 0x2000 | Z_MARKED0;
    let slot = AtomicU64::new(initial);

    let got = z_load(&slot, &ctx);

    assert_eq!(got, 0x2000, "fast path returns the stripped address");
    assert_eq!(
        slot.load(Ordering::Relaxed),
        initial,
        "fast path must not write the slot"
    );
    assert_eq!(
        ctx.stats().slow_path_entries.load(Ordering::Relaxed),
        0,
        "good color must not enter the slow path"
    );
    assert!(ctx.marked_addrs().is_empty(), "fast path must not mark");
}

#[test]
fn null_slot_takes_the_fast_path() {
    let ctx = TestBarrierContext::marking();
    let slot = AtomicU64::new(0);

    assert_eq!(z_load(&slot, &ctx), 0);
    assert_eq!(slot.load(Ordering::Relaxed), 0);
    assert_eq!(ctx.stats().slow_path_entries.load(Ordering::Relaxed), 0);
}

// -----------------------------------------------------------------------
// Self-healing
// -----------------------------------------------------------------------

#[test]
fn slow_path_heals_the_slot_in_place() {
    let ctx = TestBarrierContext::marking();
    let bad = 0x3000 | Z_REMAPPED; // REMAPPED is not good while marking
    let slot = AtomicU64::new(bad);

    let got = z_load(&slot, &ctx);

    assert_eq!(got, 0x3000);
    let after = slot.load(Ordering::Relaxed);
    assert_ne!(
        after, bad,
        "the slot value must have CHANGED — this is the heal"
    );
    assert_eq!(after, 0x3000 | Z_MARKED0);
    assert_eq!(
        classify(after, ctx.good_mask()),
        ZFastPath::Good(0x3000),
        "the healed slot must now hit the fast path"
    );

    let s = ctx.stats().snapshot();
    assert_eq!(s.slow_path_entries, 1);
    assert_eq!(s.heal_cas_wins, 1);
    assert_eq!(s.heal_cas_losses, 0);
    assert_eq!(ctx.marked_addrs(), vec![0x3000]);
}

#[test]
fn healed_slot_is_only_slow_once() {
    let ctx = TestBarrierContext::marking();
    let slot = AtomicU64::new(0x4000 | Z_REMAPPED);

    for _ in 0..8 {
        assert_eq!(z_load(&slot, &ctx), 0x4000);
    }

    assert_eq!(
        ctx.stats().slow_path_entries.load(Ordering::Relaxed),
        1,
        "self-healing means the slow path is paid per SLOT, not per LOAD"
    );
}

#[test]
fn slow_path_follows_forwarding() {
    let ctx = TestBarrierContext::relocating();
    ctx.forward_to(0x5000, 0x8800);
    let slot = AtomicU64::new(0x5000 | Z_MARKED0); // MARKED0 is bad now

    let got = z_load(&slot, &ctx);

    assert_eq!(got, 0x8800, "must return the RELOCATED address");
    assert_eq!(slot.load(Ordering::Relaxed), 0x8800 | Z_REMAPPED);
    assert_eq!(ctx.stats().forwards_followed.load(Ordering::Relaxed), 1);
}

#[test]
fn forwarding_is_skipped_when_not_relocating() {
    let ctx = TestBarrierContext::marking();
    // A stale entry that must NOT be consulted: relocation is off.
    ctx.forward_to(0x6000, 0xDEAD);
    let slot = AtomicU64::new(0x6000 | Z_REMAPPED);

    assert_eq!(z_load(&slot, &ctx), 0x6000);
    assert_eq!(ctx.stats().forwards_followed.load(Ordering::Relaxed), 0);
}

#[test]
fn lost_cas_returns_the_right_address_and_does_not_clobber() {
    // The "exactly one attempt" contract, made deterministic: hand the
    // slow path a STALE comparand while the slot already holds a value a
    // racing mutator stored. The CAS must fail, the mutator's value must
    // survive, and the barrier must still answer correctly for the value
    // it was given.
    let ctx = TestBarrierContext::marking();
    let mutator_value = 0x7777 | Z_MARKED0;
    let slot = AtomicU64::new(mutator_value);
    let stale_observation = 0x1111 | Z_REMAPPED;

    let got = load_barrier_slow(&slot, stale_observation, &ctx, ZBarrierKind::Load);

    assert_eq!(got, 0x1111, "the barrier answers for the value IT loaded");
    assert_eq!(
        slot.load(Ordering::Relaxed),
        mutator_value,
        "a retry loop here would resurrect 0x1111 over the mutator's store"
    );
    let s = ctx.stats().snapshot();
    assert_eq!(s.heal_cas_wins, 0);
    assert_eq!(s.heal_cas_losses, 1);
}

// -----------------------------------------------------------------------
// N4 (2026-08-07): `forward`'s error channel, and what the slow path does
// with it.
// -----------------------------------------------------------------------

/// N4. A `forward` that answers `None` must **not** be papered over.
///
/// Before this finding, `forward` returned a plain `u64`, so the only body
/// an implementor over `ZRelocate::forward_offset` could write mapped its
/// `Err` arm back to the input offset — the fallback that method's own docs
/// forbid in the paragraph immediately above the code block that showed it.
///
/// # Why `#[should_panic]` is safe here and was not for the domain check
///
/// The divergence is `ZBarrierContext::on_forward_failure`, a **default
/// trait method that panics unconditionally**, not a `debug_assert!`. It
/// therefore behaves identically in debug and release, so this test means
/// the same thing in both profiles — unlike a test that drives a
/// `debug_assert`, which is the trap
/// [`is_bare_offset_separates_a_heap_offset_from_a_machine_pointer`]
/// documents.
#[test]
#[should_panic(expected = "on_forward_failure")]
fn a_forward_that_cannot_answer_does_not_return_a_from_space_reference() {
    let ctx = TestBarrierContext::relocating_with_failing_forward();
    let slot = AtomicU64::new(0x2000 | Z_MARKED0); // bad: good is REMAPPED
    let _ = z_load(&slot, &ctx);
}

/// The observable half of the same event, asserted without unwinding past
/// it: the slot is **not healed**, nothing is marked, and the failure is
/// counted.
///
/// Healing is the part worth pinning. A heal here would stamp the current
/// good colour onto a reference the collector has just said is unusable, so
/// every future load of that slot would take the *fast* path straight to a
/// from-space object — turning a one-shot failure into a permanent,
/// barrier-invisible dangling pointer.
#[test]
fn a_failed_forward_neither_heals_the_slot_nor_marks() {
    let ctx = TestBarrierContext::relocating_with_failing_forward();
    let original = 0x2000 | Z_MARKED0;
    let slot = AtomicU64::new(original);

    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| z_load(&slot, &ctx)));
    assert!(
        caught.is_err(),
        "the default on_forward_failure must diverge"
    );

    assert_eq!(
        slot.load(Ordering::Relaxed),
        original,
        "a failed forward must NOT heal: a good colour over an unusable \
         reference makes every later load take the fast path to it"
    );
    assert!(
        ctx.marked_addrs().is_empty(),
        "marking is on, and still nothing may be marked"
    );
    let s = ctx.stats().snapshot();
    assert_eq!(s.forward_failures, 1, "counted before the hook diverges");
    assert_eq!(s.heal_cas_wins, 0);
    assert_eq!(s.heal_cas_losses, 0);
    assert_eq!(s.forwards_followed, 0);
}

/// An implementor may replace the divergence with its own fatal path, but
/// the `!` return type means it cannot replace it with a *value* — which is
/// the whole point of the hook. This pins that the override is reached at
/// all.
#[test]
fn on_forward_failure_is_overridable_and_still_cannot_return_a_value() {
    struct FatalCtx {
        stats: ZBarrierStats,
    }
    impl ZBarrierContext for FatalCtx {
        fn good_mask(&self) -> u64 {
            Z_REMAPPED
        }
        fn is_marking(&self) -> bool {
            false
        }
        fn is_relocating(&self) -> bool {
            true
        }
        fn forward(&self, _addr: u64) -> Option<u64> {
            None
        }
        fn mark_live(&self, _addr: u64) {}
        fn on_forward_failure(&self, stale_offset: u64) -> ! {
            // A real VM would call its crash-log/abort path here. The
            // signature admits no third option.
            panic!("vm fatal error path reached for {stale_offset:#x}");
        }
        fn stats(&self) -> &ZBarrierStats {
            &self.stats
        }
    }

    let ctx = FatalCtx {
        stats: ZBarrierStats::new(),
    };
    let slot = AtomicU64::new(0x4400 | Z_MARKED0);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| z_load(&slot, &ctx)));
    let msg = *caught
        .expect_err("the override must diverge")
        .downcast::<String>()
        .expect("panic payload is a formatted String");
    assert!(
        msg.contains("vm fatal error path reached for 0x4400"),
        "the override must be what ran, and it must see the stale offset: {msg}"
    );
    assert_eq!(ctx.stats().snapshot().forward_failures, 1);
}

/// N3, barrier side. A `forward` answering in the **wrong domain** is a
/// `forward` failure too, not a warning followed by business as usual.
///
/// The previous code logged "healing will corrupt the metadata field of the
/// slot" and then healed, so the message was a prediction rather than a
/// description — and it went on to hand the machine pointer to `mark_live`,
/// where `zgc::mark` discards it as a wild pointer and the object is swept.
#[test]
fn a_forward_answering_with_a_machine_pointer_is_a_failure_not_a_warning() {
    // ~47 bits: a legal Linux reservation address, five bits past the
    // 42-bit offset field, and invisible on the Windows dev host.
    const LINUX_SHAPED_HEAP_ADDRESS: u64 = 0x7f12_3456_7890;
    assert!(!is_bare_offset(LINUX_SHAPED_HEAP_ADDRESS, Z_OFFSET_MASK));

    let ctx = TestBarrierContext::relocating();
    ctx.forward_to(0x5000, LINUX_SHAPED_HEAP_ADDRESS);
    let original = 0x5000 | Z_MARKED0;
    let slot = AtomicU64::new(original);

    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| z_load(&slot, &ctx)));
    assert!(
        caught.is_err(),
        "an out-of-domain forward must not be survivable"
    );
    assert_eq!(
        slot.load(Ordering::Relaxed),
        original,
        "the stray bits must never reach the healed word's metadata field"
    );
    assert!(ctx.marked_addrs().is_empty());
    assert_eq!(ctx.stats().snapshot().forward_failures, 1);
}

/// N5 (2026-08-07). The module header used to name `ZRelocate::encode_to` /
/// `decode_to` with line anchors and assert that the offset-domain adapter
/// "does not exist yet in `src`". It does. This is the compile-time witness
/// for the two symbols the rewritten header now names, so that removing or
/// renaming them breaks the build instead of quietly rotting the doc again.
///
/// Deliberately an *existence* witness rather than a signature or line-band
/// assertion: line anchors in this file have drifted repeatedly, and
/// spelling out a signature owned by another module would make this test
/// fail for changes that are none of its business.
#[test]
fn the_headers_relocate_entry_points_exist() {
    use crate::zgc::relocate::ZRelocate;
    let _ = ZRelocate::forward_offset;
    let _ = ZRelocate::forward_lookup_offset;
    // And the absolute-domain pair the header contrasts them with, which is
    // what an implementor must NOT reach for.
    let _ = ZRelocate::forward;
    let _ = ZRelocate::forward_lookup;
}

/// N3, the seam the header now names on the `mark.rs` side. Same reasoning
/// as above: an existence witness, so the header's instruction ("route
/// through `mark_live_offset`, never `mark_live`") cannot outlive the
/// method it names.
#[test]
fn the_headers_mark_offset_entry_points_exist() {
    use crate::zgc::mark::ZMarkHandle;
    let _ = ZMarkHandle::mark_live_offset;
    let _ = ZMarkHandle::mark_live_buffered_offset;
    let _ = ZMarkHandle::mark_live;
}

#[test]
fn direct_slow_path_with_null_is_a_no_op() {
    let ctx = TestBarrierContext::marking();
    let slot = AtomicU64::new(0);
    assert_eq!(load_barrier_slow(&slot, 0, &ctx, ZBarrierKind::Load), 0);
    assert_eq!(slot.load(Ordering::Relaxed), 0);
    assert_eq!(ctx.stats().null_slow_paths.load(Ordering::Relaxed), 1);
    assert!(ctx.marked_addrs().is_empty());
}

// -----------------------------------------------------------------------
// The slow-path accounting identity
// -----------------------------------------------------------------------

/// PINS the identity documented on [`ZBarrierStats`] and tabulated on
/// [`load_barrier_slow`]:
///
/// ```text
///   heal_cas_wins + heal_cas_losses + heal_skipped == slow_path_entries
/// ```
///
/// **This test walks all four exits deliberately, one per stats block.** A
/// test that only drove the healthy path would pass against the very defect
/// this pins: `wins + losses == entries` was the stated identity for three
/// passes *after* the null guard and the two `forward`-failure exits made it
/// false, and it stayed stated because nothing exercised a skipping exit and
/// the CAS outcome in the same breath. The whole value here is the
/// enumeration — if a future pass adds a fifth exit and does not extend this
/// test, the exit that vanishes silently is the one this test was written to
/// make impossible.
///
/// Exits 2 and 3 diverge, so they are driven under `catch_unwind`; the point
/// of asserting the counters *after* catching is that the buckets are bumped
/// **before** `on_forward_failure`, which is what makes the identity readable
/// from a crash dump rather than only from a survivable run.
#[test]
fn every_slow_path_exit_lands_in_exactly_one_heal_bucket() {
    fn identity(stats: &ZBarrierStats) -> (u64, u64) {
        let s = stats.snapshot();
        (
            s.heal_cas_wins + s.heal_cas_losses + s.heal_skipped,
            s.slow_path_entries,
        )
    }

    // -- Exit 1: the whole-word Z_NULL guard --------------------------
    // There is no correct colour for null, so this exit heals nothing. It
    // is a legitimate no-CAS exit, and before `heal_skipped` existed it was
    // an entrant with no outcome at all.
    let ctx = TestBarrierContext::marking();
    let slot = AtomicU64::new(Z_NULL);
    assert_eq!(
        load_barrier_slow(&slot, Z_NULL, &ctx, ZBarrierKind::Load),
        0
    );
    let s = ctx.stats().snapshot();
    assert_eq!(s.slow_path_entries, 1);
    assert_eq!(s.null_slow_paths, 1, "the reason counter");
    assert_eq!(s.heal_skipped, 1, "and the roll-up bucket");
    assert_eq!(
        s.heal_cas_wins + s.heal_cas_losses,
        0,
        "no CAS was attempted"
    );
    let (accounted, entries) = identity(ctx.stats());
    assert_eq!(accounted, entries, "exit 1 (null) must account for itself");

    // -- Exit 2: `forward` answered `None` ----------------------------
    let ctx = TestBarrierContext::relocating_with_failing_forward();
    let slot = AtomicU64::new(0x7000 | Z_MARKED0); // bad: good is REMAPPED
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| z_load(&slot, &ctx)));
    assert!(caught.is_err(), "a `None` forward must not be survivable");
    let s = ctx.stats().snapshot();
    assert_eq!(s.slow_path_entries, 1);
    assert_eq!(s.forward_failures, 1, "the reason counter");
    assert_eq!(
        s.heal_skipped, 1,
        "bumped BEFORE on_forward_failure diverges, so a crash dump balances"
    );
    assert_eq!(s.heal_cas_wins + s.heal_cas_losses, 0);
    let (accounted, entries) = identity(ctx.stats());
    assert_eq!(
        accounted, entries,
        "exit 2 (forward -> None) must account for itself"
    );

    // -- Exit 3: `forward` answered outside the address domain --------
    // ~47 bits: a legal Linux reservation address, and invisible on the
    // Windows dev host. In a debug build the `debug_assert!` fires first and
    // in a release build `on_forward_failure` does; the counters are bumped
    // ahead of both, so this assertion is profile-independent.
    const LINUX_SHAPED_HEAP_ADDRESS: u64 = 0x7f12_3456_7890;
    let ctx = TestBarrierContext::relocating();
    ctx.forward_to(0x8000, LINUX_SHAPED_HEAP_ADDRESS);
    let slot = AtomicU64::new(0x8000 | Z_MARKED0);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| z_load(&slot, &ctx)));
    assert!(
        caught.is_err(),
        "an out-of-domain forward must not be survivable"
    );
    let s = ctx.stats().snapshot();
    assert_eq!(s.slow_path_entries, 1);
    assert_eq!(s.forward_failures, 1);
    assert_eq!(s.heal_skipped, 1);
    assert_eq!(s.heal_cas_wins + s.heal_cas_losses, 0);
    let (accounted, entries) = identity(ctx.stats());
    assert_eq!(
        accounted, entries,
        "exit 3 (forward out of domain) must account for itself"
    );

    // -- Exit 5: a weak load while marking ----------------------------
    // Added 2026-09-20 with the exit itself. It is the only no-CAS exit that
    // is neither a refusal nor a defect: the barrier answered correctly and
    // deliberately declined to write the answer back, because the colour it
    // would write is the one that hides an unmarked referent from a strong
    // load. See exit 5 on `load_barrier_slow`.
    let ctx = TestBarrierContext::marking();
    let slot = AtomicU64::new(0xA500 | Z_REMAPPED); // bad: good is MARKED0
    assert_eq!(z_weak_load(&slot, &ctx), 0xA500);
    assert_eq!(
        slot.load(Ordering::Relaxed),
        0xA500 | Z_REMAPPED,
        "the slot must be untouched"
    );
    let s = ctx.stats().snapshot();
    assert_eq!(s.slow_path_entries, 1);
    assert_eq!(s.weak_slow_paths, 1, "the reason counter");
    assert_eq!(s.heal_skipped, 1, "and the roll-up bucket");
    assert_eq!(s.heal_cas_wins + s.heal_cas_losses, 0);
    assert_eq!(s.marks_enqueued, 0);
    let (accounted, entries) = identity(ctx.stats());
    assert_eq!(
        accounted, entries,
        "exit 5 (weak load while marking) must account for itself"
    );

    // -- Exit 4: the CAS, both outcomes, on ONE stats block -----------
    let ctx = TestBarrierContext::marking();
    let slot = AtomicU64::new(0x9000 | Z_REMAPPED); // bad: good is MARKED0
                                                    // (a) win — `observed` is what the slot actually holds.
    assert_eq!(z_load(&slot, &ctx), 0x9000);
    assert_eq!(slot.load(Ordering::Relaxed), 0x9000 | Z_MARKED0);
    // (b) loss — hand the slow path a comparand the slot does NOT hold.
    // That is exactly the shape of losing the race to another healer, minus
    // the thread scheduling, so it is deterministic.
    assert_eq!(
        load_barrier_slow(&slot, 0x9000 | Z_MARKED1, &ctx, ZBarrierKind::Load),
        0x9000,
        "a lost CAS still returns the correct address for the value I loaded"
    );
    let s = ctx.stats().snapshot();
    assert_eq!(s.slow_path_entries, 2);
    assert_eq!(s.heal_cas_wins, 1);
    assert_eq!(s.heal_cas_losses, 1);
    assert_eq!(
        s.heal_skipped, 0,
        "both entrants attempted a CAS, so neither may land in the skip bucket"
    );
    let (accounted, entries) = identity(ctx.stats());
    assert_eq!(
        accounted, entries,
        "exit 4 (the CAS) must account for itself"
    );
}

/// The counterpart claim, and the one the cross-module suite got wrong:
/// **`slow_path_entries` is not a thread count.**
///
/// A thread whose load lands after another thread's heal hits the *fast*
/// path and never becomes a slow-path entrant — that is the entire point of
/// self-healing. So an assertion of the form
/// `wins + losses == <number of threads>` asserts that healing did not work,
/// and it fails exactly when the barrier is working best. Assert against
/// `slow_path_entries`.
#[test]
fn a_healed_slot_makes_later_racers_fast_path_hits_not_slow_path_entrants() {
    let ctx = TestBarrierContext::marking();
    let slot = AtomicU64::new(0xA100 | Z_REMAPPED); // bad: good is MARKED0

    // Sequential rather than threaded on purpose: this pins the *mechanism*
    // that produced the cross-module suite's "2 of 4" reading, with no
    // scheduling left to chance. Four loads, one slow-path entrant.
    for _ in 0..4 {
        assert_eq!(z_load(&slot, &ctx), 0xA100);
    }

    let s = ctx.stats().snapshot();
    assert_eq!(
        s.slow_path_entries, 1,
        "the first load heals; the other three are fast-path hits"
    );
    assert_eq!(s.heal_cas_wins, 1);
    assert_eq!(
        s.heal_cas_wins + s.heal_cas_losses + s.heal_skipped,
        s.slow_path_entries,
        "and the identity is against ENTRIES, never against the load count"
    );
}

// -----------------------------------------------------------------------
// THE test: concurrent self-heal convergence
// -----------------------------------------------------------------------

#[test]
fn concurrent_barriers_on_one_bad_slot_all_agree_and_the_slot_ends_good() {
    // N threads race the same bad-colored slot. Every one must come back
    // with the SAME relocated address, exactly one CAS may win, the losers
    // must not retry, and the slot must be left good-colored so the next
    // load is free. This is the property the whole design rests on.
    const THREADS: usize = 8;
    const OLD: u64 = 0x1_0000;
    const NEW: u64 = 0x9_0000;

    let ctx = TestBarrierContext::relocating();
    ctx.forward_to(OLD, NEW);

    let slot = AtomicU64::new(OLD | Z_MARKED0); // bad: good is REMAPPED
    let gate = std::sync::Barrier::new(THREADS);
    let results: Mutex<Vec<u64>> = Mutex::new(Vec::new());

    std::thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                // Release every thread into the barrier at once so the
                // CAS race is real rather than sequentialised by spawn
                // latency.
                gate.wait();
                let got = z_load(&slot, &ctx);
                results.lock().push(got);
            });
        }
    });

    let results = results.into_inner();
    assert_eq!(results.len(), THREADS);
    for (i, r) in results.iter().enumerate() {
        assert_eq!(*r, NEW, "thread {i} disagreed about the healed address");
    }

    let final_value = slot.load(Ordering::Relaxed);
    assert_eq!(final_value, NEW | Z_REMAPPED);
    assert_eq!(
        classify(final_value, ctx.good_mask()),
        ZFastPath::Good(NEW),
        "the slot must end good-colored"
    );

    let st = ctx.stats().snapshot();
    assert_eq!(
        st.heal_cas_wins, 1,
        "only one CAS can match the original bad word"
    );
    assert_eq!(
        st.heal_skipped, 0,
        "no exit here skips the heal: not null, and `forward` answers"
    );
    assert_eq!(
        st.heal_cas_wins + st.heal_cas_losses + st.heal_skipped,
        st.slow_path_entries,
        "every slow path attempts exactly one CAS — no retries"
    );
    assert!(
        st.slow_path_entries <= THREADS as u64,
        "no thread may enter the slow path twice — and note this is `<=`, \
         not `==`: a thread that loads after another thread's heal hits the \
         FAST path and is not an entrant at all"
    );
    assert!(st.forwards_followed >= 1);
}

// -----------------------------------------------------------------------
// Barrier variants
// -----------------------------------------------------------------------

/// A weak load during marking marks nothing AND heals nothing, and the
/// second half is the load-bearing one.
///
/// # What this test used to assert, and why that was a use-after-free
///
/// It was called `weak_load_does_not_mark_but_still_heals` and it asserted
/// `slot == 0xA000 | Z_MARKED0` with `heal_cas_wins == 1` — "weak loads
/// still self-heal, only the marking differs". That heal is the bug. With
/// `Z_MARKED0` in the slot, the very next ORDINARY load of it classifies
/// `Good` on the fast path and never reaches `mark_live`, so a referent the
/// application has read stays unmarked and is swept. The assertion below is
/// the same interleaving, run to its conclusion.
///
/// The `z_load` at the end is not a bonus check; it is the whole point. A
/// test that stopped at "the slot was not healed" would pass against a heal
/// colour that merely happened to be harmless.
#[test]
fn a_weak_load_while_marking_neither_marks_nor_heals() {
    let ctx = TestBarrierContext::marking();
    let slot = AtomicU64::new(0xA000 | Z_REMAPPED);

    let got = z_weak_load(&slot, &ctx);

    assert_eq!(got, 0xA000);
    assert!(
        ctx.marked_addrs().is_empty(),
        "a weak load that marks its referent is a WeakReference that never clears"
    );
    assert_eq!(
        slot.load(Ordering::Relaxed),
        0xA000 | Z_REMAPPED,
        "the slot must keep its BAD colour: healing it to the mark colour \
         would make every later strong load take the fast path past the \
         mark_live this barrier deliberately skipped"
    );
    let s = ctx.stats().snapshot();
    assert_eq!(s.weak_slow_paths, 1);
    assert_eq!(s.marks_enqueued, 0);
    assert_eq!(s.heal_cas_wins, 0, "no CAS is attempted on exit 5");
    assert_eq!(s.heal_skipped, 1, "and the entrant is still accounted for");
    assert_eq!(
        s.heal_cas_wins + s.heal_cas_losses + s.heal_skipped,
        s.slow_path_entries,
        "the accounting identity must survive the new exit"
    );

    // THE CONSEQUENCE. A strong load of the same slot must still reach the
    // slow path and mark, which is exactly what the old healing behaviour
    // made impossible.
    assert_eq!(z_load(&slot, &ctx), 0xA000);
    assert_eq!(
        ctx.marked_addrs(),
        vec![0xA000],
        "a strong load after a weak one must still keep the referent alive"
    );
    assert_eq!(
        slot.load(Ordering::Relaxed),
        0xA000 | Z_MARKED0,
        "and the strong load heals, so the cost of not healing the weak one \
         is bounded by the first strong load of that slot"
    );
}

/// Outside a mark cycle a weak load heals exactly as it always did.
///
/// The fix above is deliberately scoped to the marking phase: healing is
/// what stops every load re-walking the forwarding table during relocation,
/// and there is no marker to hide a referent from. Pinning it here so the
/// scope cannot be widened by accident into "weak loads never heal", which
/// would be a real throughput loss on the phase where weak loads are common.
#[test]
fn a_weak_load_outside_marking_still_heals() {
    let ctx = TestBarrierContext::relocating(); // marking: false
    ctx.forward_to(0xA100, 0xA200);
    let slot = AtomicU64::new(0xA100 | Z_MARKED0); // bad: good is REMAPPED

    assert_eq!(z_weak_load(&slot, &ctx), 0xA200);
    assert!(ctx.marked_addrs().is_empty(), "weak never marks");
    assert_eq!(
        slot.load(Ordering::Relaxed),
        0xA200 | Z_REMAPPED,
        "with no mark cycle running the good colour is not a mark colour, so \
         healing cannot hide anything from a marker"
    );
    let s = ctx.stats().snapshot();
    assert_eq!(s.heal_cas_wins, 1);
    assert_eq!(s.heal_skipped, 0);
    assert_eq!(s.forwards_followed, 1);
}

// -----------------------------------------------------------------------
// The weak-specific masks (2026-09-21)
// -----------------------------------------------------------------------

/// [`TestBarrierContext`] plus OpenJDK's weak mask and weak heal colour —
/// the configuration `gap-a-weak-bad-mask-unconsumed.md` says the barrier
/// had no way to express.
///
/// `weak_bad = (good | Remapped | Finalizable) ^ metadata`, and a weak slow
/// path heals to `Remapped`, which is good under `weak_bad` and still **bad**
/// under the strong `bad_mask` while `good` is a mark colour. That asymmetry
/// is the whole content of the divergence, and it is the reason exit 5 is
/// written as a property rather than as `kind == Weak`.
struct WeakMaskedContext(TestBarrierContext);

impl ZBarrierContext for WeakMaskedContext {
    fn good_mask(&self) -> u64 {
        self.0.good_mask()
    }
    fn heal_color(&self) -> u64 {
        self.0.heal_color()
    }
    fn weak_bad_mask(&self) -> u64 {
        (self.good_mask() | Z_REMAPPED | Z_FINALIZABLE) ^ Z_METADATA_MASK
    }
    fn weak_heal_color(&self) -> u64 {
        Z_REMAPPED
    }
    fn is_marking(&self) -> bool {
        self.0.is_marking()
    }
    fn is_relocating(&self) -> bool {
        self.0.is_relocating()
    }
    fn forward(&self, addr: u64) -> Option<u64> {
        self.0.forward(addr)
    }
    fn mark_live(&self, addr: u64) {
        self.0.mark_live(addr)
    }
    fn stats(&self) -> &ZBarrierStats {
        self.0.stats()
    }
}

/// The default `weak_bad_mask` must be the strong one, or every existing
/// implementor silently changes behaviour the moment the method is added.
#[test]
fn the_default_weak_masks_are_the_strong_ones() {
    let ctx = TestBarrierContext::marking();
    assert_eq!(ctx.weak_bad_mask(), ctx.bad_mask());
    assert_eq!(ctx.weak_heal_color(), ctx.heal_color());
}

/// A phase selects the weak mask for a weak load and the strong one for
/// everything else. This is the only place the selection is made.
#[test]
fn a_phase_selects_its_kinds_mask() {
    let ctx = WeakMaskedContext(TestBarrierContext::marking());
    assert_ne!(
        ctx.weak_bad_mask(),
        ctx.bad_mask(),
        "the fixture is pointless if the two masks agree"
    );
    for kind in [
        ZBarrierKind::Load,
        ZBarrierKind::LoadVolatile,
        ZBarrierKind::KeepAlive,
        ZBarrierKind::Root,
    ] {
        let p = ZBarrierPhase::of(&ctx, kind);
        assert_eq!(p.bad_mask, ctx.bad_mask(), "{kind:?} is a strong load");
        assert_eq!(p.kind, kind);
    }
    let weak = ZBarrierPhase::of(&ctx, ZBarrierKind::Weak);
    assert_eq!(weak.bad_mask, ctx.weak_bad_mask());
    assert_eq!(weak.address_mask, ctx.address_mask());
}

/// The divergence itself: with a weak mask wired up, a merely-`Remapped`
/// referent takes the **fast** path for a weak load and the **slow** path
/// for a strong one, off the same slot and in the same phase.
#[test]
fn a_weak_mask_fast_paths_a_merely_remapped_referent() {
    let ctx = WeakMaskedContext(TestBarrierContext::marking());
    let slot = AtomicU64::new(0xA000 | Z_REMAPPED);

    assert_eq!(z_weak_load(&slot, &ctx), 0xA000);
    let s = ctx.stats().snapshot();
    assert_eq!(
        s.slow_path_entries, 0,
        "REMAPPED is good under weak_bad, so this load never left the fast path"
    );
    assert_eq!(s.weak_slow_paths, 0);
    assert_eq!(
        slot.load(Ordering::Relaxed),
        0xA000 | Z_REMAPPED,
        "a fast-path load touches nothing"
    );

    // Same word, same phase, strong load: REMAPPED is bad under bad_mask.
    assert_eq!(z_load(&slot, &ctx), 0xA000);
    let s = ctx.stats().snapshot();
    assert_eq!(s.slow_path_entries, 1);
    assert_eq!(
        ctx.0.marked_addrs(),
        vec![0xA000],
        "and the strong load still keeps the referent alive"
    );
}

/// Exit 5 must fire on the PROPERTY, not on the variant: a weak load whose
/// heal colour stays bad for a strong load is allowed to heal, even while
/// marking is running.
///
/// This is the case the 2026-09-20 fix could not express and the reason it
/// had to suppress the heal wholesale. With `weak_heal_color = Remapped`
/// the slot ends healed, the forwarding walk is amortised, and a later
/// strong load *still* reaches `mark_live` — which is the invariant exit 5
/// exists to protect.
#[test]
fn a_weak_heal_that_stays_bad_for_a_strong_load_is_allowed_while_marking() {
    let ctx = WeakMaskedContext(TestBarrierContext::marking());
    // MARKED1 is bad under BOTH masks, so the weak load really does take
    // the slow path and really does reach the heal.
    let slot = AtomicU64::new(0xC000 | Z_MARKED1);

    assert_eq!(z_weak_load(&slot, &ctx), 0xC000);
    assert!(ctx.0.marked_addrs().is_empty(), "weak never marks");
    assert_eq!(
        slot.load(Ordering::Relaxed),
        0xC000 | Z_REMAPPED,
        "healed to a colour that is good-for-weak and bad-for-strong"
    );
    let s = ctx.stats().snapshot();
    assert_eq!(s.weak_slow_paths, 1);
    assert_eq!(s.heal_cas_wins, 1, "exit 5 must NOT fire here");
    assert_eq!(s.heal_skipped, 0);

    // The invariant exit 5 protects, restated against the healed word.
    assert_eq!(z_load(&slot, &ctx), 0xC000);
    assert_eq!(
        ctx.0.marked_addrs(),
        vec![0xC000],
        "the healed colour must still send a strong load down the slow path"
    );
}

/// And the default context keeps the 2026-09-20 behaviour exactly: its
/// weak heal colour IS good for a strong load, so exit 5 fires.
#[test]
fn exit_five_still_fires_for_a_context_with_one_good_colour() {
    let ctx = TestBarrierContext::marking();
    let healed = 0xA000u64 | ctx.weak_heal_color();
    assert_eq!(
        healed & ctx.bad_mask(),
        0,
        "with a single good colour the weak heal is indistinguishable from \
         the strong one, which is exactly why it must not be written"
    );
}

// -----------------------------------------------------------------------
// The hoisted fast path (2026-09-21)
// -----------------------------------------------------------------------

/// [`z_barrier_phased`] against a hoisted phase must be
/// indistinguishable from [`z_barrier`], on both paths and in the counters.
#[test]
fn the_phased_form_agrees_with_the_one_shot_form() {
    for raw in [0u64, 0xD000 | Z_MARKED0, 0xD100 | Z_REMAPPED] {
        let one_shot_ctx = TestBarrierContext::marking();
        let phased_ctx = TestBarrierContext::marking();
        one_shot_ctx.stats().enable_hot_counters();
        phased_ctx.stats().enable_hot_counters();

        let a = AtomicU64::new(raw);
        let b = AtomicU64::new(raw);

        let ra = z_barrier(&a, &one_shot_ctx, ZBarrierKind::Load);
        let phase = ZBarrierPhase::of(&phased_ctx, ZBarrierKind::Load);
        let rb = z_barrier_phased(&b, phase, phased_ctx.stats(), &phased_ctx);

        assert_eq!(ra, rb, "raw={raw:#x}");
        assert_eq!(
            a.load(Ordering::Relaxed),
            b.load(Ordering::Relaxed),
            "raw={raw:#x}: the two forms must leave the same word in the slot"
        );
        assert_eq!(
            one_shot_ctx.stats().snapshot(),
            phased_ctx.stats().snapshot(),
            "raw={raw:#x}: and must record the same thing"
        );
        assert_eq!(one_shot_ctx.marked_addrs(), phased_ctx.marked_addrs());
    }
}

/// The batched form is a loop over the one-shot form, and `z_on_roots` is
/// the batched form. Pinned because `z_on_roots`'s body is now delegation:
/// a regression there would be invisible at the call site.
#[test]
fn the_batched_form_is_a_loop_over_the_one_shot_form() {
    let ctx = TestBarrierContext::marking();
    let slots: Vec<AtomicU64> = (0..8u64)
        .map(|i| AtomicU64::new((0xE000 + i * 8) | Z_REMAPPED))
        .collect();
    z_barrier_slice(&slots, &ctx, ZBarrierKind::Load);
    for (i, slot) in slots.iter().enumerate() {
        assert_eq!(
            slot.load(Ordering::Relaxed),
            (0xE000 + i as u64 * 8) | Z_MARKED0,
            "slot {i} was not healed by the batched form"
        );
    }
    assert_eq!(ctx.marked_addrs().len(), 8);
    assert_eq!(ctx.stats().snapshot().slow_path_entries, 8);
}

#[test]
fn keep_alive_marks_the_referent() {
    let ctx = TestBarrierContext::marking();
    let slot = AtomicU64::new(0xB000 | Z_REMAPPED);

    assert_eq!(z_keep_alive(&slot, &ctx), 0xB000);
    assert_eq!(
        ctx.marked_addrs(),
        vec![0xB000],
        "Reference.get() must resurrect its referent"
    );
    assert_eq!(ctx.stats().marks_enqueued.load(Ordering::Relaxed), 1);
}

#[test]
fn root_barrier_marks_and_heals() {
    let ctx = TestBarrierContext::marking();
    let slot = AtomicU64::new(0xC000 | Z_REMAPPED);

    assert_eq!(z_on_root(&slot, &ctx), 0xC000);
    assert_eq!(ctx.marked_addrs(), vec![0xC000]);
    assert_eq!(slot.load(Ordering::Relaxed), 0xC000 | Z_MARKED0);
}

#[test]
fn marking_is_skipped_when_the_mark_phase_is_off() {
    // Relocation phase: bad color, so the slow path runs and heals — but
    // nothing is marked, because there is no mark closure to feed.
    let ctx = TestBarrierContext::relocating();
    let slot = AtomicU64::new(0xD000 | Z_MARKED0);

    assert_eq!(z_load(&slot, &ctx), 0xD000);
    assert!(ctx.marked_addrs().is_empty());
    assert_eq!(ctx.stats().marks_enqueued.load(Ordering::Relaxed), 0);
    assert_eq!(ctx.stats().heal_cas_wins.load(Ordering::Relaxed), 1);
}

#[test]
fn volatile_load_has_the_same_semantics_as_a_plain_load() {
    // The ordering difference is not observable from a single-threaded
    // test; what IS assertable is that the volatile variant is not
    // accidentally a different barrier.
    let ctx = TestBarrierContext::marking();
    let slot = AtomicU64::new(0xE000 | Z_REMAPPED);

    assert_eq!(z_load_volatile(&slot, &ctx), 0xE000);
    assert_eq!(slot.load(Ordering::Relaxed), 0xE000 | Z_MARKED0);
    assert_eq!(ctx.marked_addrs(), vec![0xE000]);
}

#[test]
fn kind_marks_table_is_what_the_docs_claim() {
    assert!(ZBarrierKind::Load.marks());
    assert!(ZBarrierKind::LoadVolatile.marks());
    assert!(ZBarrierKind::KeepAlive.marks());
    assert!(ZBarrierKind::Root.marks());
    assert!(!ZBarrierKind::Weak.marks(), "Weak must never keep alive");
    assert!(ZBarrierKind::LoadVolatile.is_volatile());
    assert!(!ZBarrierKind::Load.is_volatile());
}

#[test]
fn root_sweep_heals_every_slot() {
    let ctx = TestBarrierContext::marking();
    let slots: Vec<AtomicU64> = (1..=4u64)
        .map(|i| AtomicU64::new((i * 0x100) | Z_REMAPPED))
        .collect();

    z_on_roots(&slots, &ctx);

    for (i, slot) in slots.iter().enumerate() {
        let want = ((i as u64 + 1) * 0x100) | Z_MARKED0;
        assert_eq!(slot.load(Ordering::Relaxed), want);
    }
    assert_eq!(ctx.marked_addrs().len(), 4);
}

// -----------------------------------------------------------------------
// Stats accounting
// -----------------------------------------------------------------------

#[test]
fn fast_hits_are_only_counted_when_hot_counters_are_enabled() {
    let ctx = TestBarrierContext::marking();
    let slot = AtomicU64::new(0xF000 | Z_MARKED0);

    for _ in 0..5 {
        let _ = z_load(&slot, &ctx);
    }
    assert_eq!(
        ctx.stats().fast_path_hits.load(Ordering::Relaxed),
        0,
        "the per-load counter must be off by default — it is a `lock xadd` \
         on the hottest path in the VM"
    );

    ctx.stats().enable_hot_counters();
    for _ in 0..5 {
        let _ = z_load(&slot, &ctx);
    }
    assert_eq!(ctx.stats().fast_path_hits.load(Ordering::Relaxed), 5);

    ctx.stats().disable_hot_counters();
    let _ = z_load(&slot, &ctx);
    assert_eq!(ctx.stats().fast_path_hits.load(Ordering::Relaxed), 5);
}

#[test]
fn stats_snapshot_and_reset() {
    let ctx = TestBarrierContext::marking();
    ctx.stats().enable_hot_counters();

    let bad = AtomicU64::new(0x11000 | Z_REMAPPED);
    let good = AtomicU64::new(0x12000 | Z_MARKED0);

    let _ = z_load(&bad, &ctx); // slow, heals, marks
    let _ = z_load(&bad, &ctx); // now fast
    let _ = z_load(&good, &ctx); // fast
    let weak = AtomicU64::new(0x13000 | Z_REMAPPED);
    // Slow, does NOT mark, and — since 2026-09-20 — does NOT heal either,
    // because `ctx` is marking. See exit 5 on `load_barrier_slow`.
    let _ = z_weak_load(&weak, &ctx);

    let s = ctx.stats().snapshot();
    assert_eq!(s.slow_path_entries, 2);
    assert_eq!(s.fast_path_hits, 2);
    assert_eq!(s.heal_cas_wins, 1, "only the strong load healed");
    assert_eq!(s.heal_cas_losses, 0);
    assert_eq!(s.marks_enqueued, 1);
    assert_eq!(s.weak_slow_paths, 1);
    assert_eq!(s.forwards_followed, 0);
    assert_eq!(s.null_slow_paths, 0);
    assert_eq!(s.heal_skipped, 1, "the weak load's exit-5 skip");
    assert_eq!(
        s.heal_cas_wins + s.heal_cas_losses + s.heal_skipped,
        s.slow_path_entries,
        "the accounting identity, over a run that also took the fast path"
    );

    ctx.stats().reset();
    assert_eq!(ctx.stats().snapshot(), ZBarrierStatsSnapshot::default());
    assert!(
        ctx.stats().hot_counters_enabled(),
        "reset zeroes the counters, not the gate"
    );
}

#[test]
fn barrier_works_through_a_trait_object() {
    // The trait must stay object-safe: a heap reached as `&dyn` is a
    // realistic wiring, and losing object safety to a stray generic
    // method would only be discovered at that point.
    let ctx = TestBarrierContext::marking();
    let dyn_ctx: &dyn ZBarrierContext = &ctx;
    let slot = AtomicU64::new(0x14000 | Z_REMAPPED);

    assert_eq!(z_load(&slot, dyn_ctx), 0x14000);
    assert_eq!(slot.load(Ordering::Relaxed), 0x14000 | Z_MARKED0);
}

// -----------------------------------------------------------------------
// Mark queue / per-thread buffers
// -----------------------------------------------------------------------

#[test]
fn mark_queue_starts_empty_with_a_unique_id() {
    let a = ZMarkQueue::new();
    let b = ZMarkQueue::new();
    assert!(a.is_empty() && b.is_empty());
    assert_ne!(a.id(), b.id());
}

#[test]
fn thread_local_mark_buffer_batches_until_flushed() {
    // Fresh thread so the buffer starts empty regardless of test ordering.
    std::thread::spawn(|| {
        let q = ZMarkQueue::new();
        const S1: u64 = 0xA11CE_000;
        const S2: u64 = 0xA11CE_001;

        z_mark_thread_local_log(&q, S1);
        z_mark_thread_local_log(&q, S2);
        assert!(
            q.is_empty(),
            "a partial bucket must stay thread-local — that is the point"
        );

        flush_thread_mark_buffer(&q);
        let drained = q.drain();
        assert!(drained.contains(&S1) && drained.contains(&S2));
    })
    .join()
    .unwrap();
}

#[test]
fn thread_local_mark_buffer_auto_flushes_at_capacity() {
    std::thread::spawn(|| {
        let q = ZMarkQueue::new();
        for i in 1..=Z_MARK_BUFFER_CAPACITY as u64 {
            z_mark_thread_local_log(&q, i);
        }
        assert_eq!(
            q.len(),
            Z_MARK_BUFFER_CAPACITY,
            "the bucket must spill without an explicit flush"
        );
    })
    .join()
    .unwrap();
}

#[test]
fn thread_local_mark_log_ignores_null() {
    std::thread::spawn(|| {
        let q = ZMarkQueue::new();
        z_mark_thread_local_log(&q, 0);
        flush_thread_mark_buffer(&q);
        assert!(q.is_empty());
    })
    .join()
    .unwrap();
}

#[test]
fn collector_side_flush_reaches_every_live_thread_buffer() {
    std::thread::spawn(|| {
        let q = ZMarkQueue::new();
        const SENTINEL: u64 = 0xBEEF_FACE;
        z_mark_thread_local_log(&q, SENTINEL);
        assert!(q.is_empty());

        flush_all_thread_mark_buffers(&q);
        assert!(q.drain().contains(&SENTINEL));
    })
    .join()
    .unwrap();
}

#[test]
fn thread_local_mark_buffers_are_queue_scoped() {
    // With two live queues, a drain on behalf of A must not steal B's
    // buffered entries — the cross-queue steal that `satb.rs` had to fix.
    std::thread::spawn(|| {
        let qa = ZMarkQueue::new();
        let qb = ZMarkQueue::new();
        const SA: u64 = 0xAAAA_0001;
        const SB: u64 = 0xBBBB_0002;

        z_mark_thread_local_log(&qa, SA);
        z_mark_thread_local_log(&qb, SB);

        flush_all_thread_mark_buffers(&qa);
        let a = qa.drain();
        assert!(a.contains(&SA));
        assert!(!a.contains(&SB), "qa stole qb's entry: {a:?}");

        flush_all_thread_mark_buffers(&qb);
        assert!(qb.drain().contains(&SB), "qb's entry was lost");
    })
    .join()
    .unwrap();
}

#[test]
fn dying_thread_mark_entries_survive_until_the_next_drain() {
    // A mutator that logs below the auto-flush threshold and then exits
    // must not lose the entry: it describes heap reachability, and losing
    // it hides a live object from the mark closure.
    let q = Arc::new(ZMarkQueue::new());
    const SENTINEL: u64 = 0xDEAD_1234;

    let q2 = Arc::clone(&q);
    std::thread::spawn(move || {
        z_mark_thread_local_log(&q2, SENTINEL);
        assert!(q2.is_empty(), "entry must still be thread-local");
    })
    .join()
    .unwrap();

    flush_all_thread_mark_buffers(&q);
    assert!(
        q.drain().contains(&SENTINEL),
        "a dead thread's buffered mark entry must survive"
    );

    // The emptied orphan was reaped: a second sweep finds nothing.
    flush_all_thread_mark_buffers(&q);
    assert!(q.is_empty());
}

#[test]
fn mark_queue_takes_concurrent_batches_from_many_threads() {
    let q = ZMarkQueue::new();
    // Bind a shared reference first: a `move` closure is needed to copy the
    // loop variable `t` into each thread (borrowing it would let the thread
    // outlive the iteration), and `move` would otherwise swallow `q` itself.
    let qref = &q;
    std::thread::scope(|s| {
        for t in 0..4u64 {
            s.spawn(move || {
                let base = t * 1000;
                let entries: Vec<u64> = (1..=64).map(|i| base + i).collect();
                qref.push_batch(entries);
            });
        }
    });
    assert_eq!(q.drain().len(), 4 * 64);
}

/// End-to-end: a [`ZBarrierContext`] that routes `mark_live` through the
/// per-thread buffers, proving the two halves of this module compose.
struct QueueBackedContext {
    queue: ZMarkQueue,
    stats: ZBarrierStats,
}

impl ZBarrierContext for QueueBackedContext {
    fn good_mask(&self) -> u64 {
        Z_MARKED0
    }
    fn is_marking(&self) -> bool {
        true
    }
    fn is_relocating(&self) -> bool {
        false
    }
    fn forward(&self, addr: u64) -> Option<u64> {
        Some(addr)
    }
    fn mark_live(&self, addr: u64) {
        z_mark_thread_local_log(&self.queue, addr);
    }
    fn stats(&self) -> &ZBarrierStats {
        &self.stats
    }
}

#[test]
fn barrier_marks_route_through_the_per_thread_buffers() {
    std::thread::spawn(|| {
        let ctx = QueueBackedContext {
            queue: ZMarkQueue::new(),
            stats: ZBarrierStats::new(),
        };
        let slots: Vec<AtomicU64> = (1..=3u64)
            .map(|i| AtomicU64::new((i * 0x1000) | Z_REMAPPED))
            .collect();

        for slot in &slots {
            let _ = z_load(slot, &ctx);
        }

        // Below capacity → still buffered, invisible to the marker.
        assert!(ctx.queue.is_empty());
        flush_thread_mark_buffer(&ctx.queue);

        let drained = ctx.queue.drain();
        assert_eq!(drained.len(), 3);
        for i in 1..=3u64 {
            assert!(drained.contains(&(i * 0x1000)));
        }
        assert_eq!(ctx.stats().marks_enqueued.load(Ordering::Relaxed), 3);
    })
    .join()
    .unwrap();
}

#[test]
fn global_fallback_stats_are_reachable() {
    // A context that does not override `stats()` must still produce
    // numbers rather than silently produce none.
    struct Bare;
    impl ZBarrierContext for Bare {
        fn good_mask(&self) -> u64 {
            Z_MARKED0
        }
        fn is_marking(&self) -> bool {
            false
        }
        fn is_relocating(&self) -> bool {
            false
        }
        fn forward(&self, addr: u64) -> Option<u64> {
            Some(addr)
        }
        fn mark_live(&self, _addr: u64) {}
    }

    let before = global_z_barrier_stats()
        .slow_path_entries
        .load(Ordering::Relaxed);
    let slot = AtomicU64::new(0x15000 | Z_REMAPPED);
    assert_eq!(z_load(&slot, &Bare), 0x15000);
    // `>` rather than `== before + 1`: the global block is process-wide and
    // another test thread may be using it too.
    assert!(
        global_z_barrier_stats()
            .slow_path_entries
            .load(Ordering::Relaxed)
            > before
    );
}

#[test]
fn slot_as_atomic_round_trips() {
    let mut word: u64 = 0x16000 | Z_REMAPPED;
    let ctx = TestBarrierContext::marking();
    // SAFETY: `word` is a live, aligned local, and it is accessed only
    // through the returned reference for the duration of the borrow.
    let slot = unsafe { slot_as_atomic(&mut word as *mut u64) };
    assert_eq!(z_load(slot, &ctx), 0x16000);
    assert_eq!(slot.load(Ordering::Relaxed), 0x16000 | Z_MARKED0);
}

// -----------------------------------------------------------------------
// The compiled barrier's contract under a color rotation (zt t21/zb4)
// -----------------------------------------------------------------------

/// A context whose good mask is a real `vaddr::ZGoodMask` flipped through
/// its phases, with the compiled bad-mask cell published beside it -- the
/// shape a heap with the stage-2e flip would have. No relocation: this
/// collector relocates stop-the-world, so `forward` is the identity.
struct FlipContext {
    good: crate::zgc::vaddr::ZGoodMask,
    cell: crate::zgc::vaddr::ZCompiledBadMask,
    marking: AtomicBool,
    marked: Mutex<Vec<u64>>,
    stats: ZBarrierStats,
}

impl FlipContext {
    fn new() -> Self {
        Self {
            good: crate::zgc::vaddr::ZGoodMask::new(),
            cell: crate::zgc::vaddr::ZCompiledBadMask::new(),
            marking: AtomicBool::new(false),
            marked: Mutex::new(Vec::new()),
            stats: ZBarrierStats::new(),
        }
    }
    fn mark_start(&self) {
        self.good.flip_to_mark_publishing(&self.cell);
        self.marking.store(true, Ordering::Relaxed);
    }
    fn mark_end(&self) {
        self.marking.store(false, Ordering::Relaxed);
        self.good.set_mark_complete();
    }
    fn relocate_start(&self) {
        self.good.flip_to_remap_publishing(&self.cell);
    }
    /// One compiled load of `slot`: the inline fast path, and the helper
    /// on its slow branch. Answers the address and whether it went slow.
    fn load(&self, slot: &AtomicU64) -> (u64, bool) {
        let word = slot.load(Ordering::Relaxed);
        match inline_barrier_fast(word, self.cell.load(), BASE) {
            ZInlineFast::Done(a) => (a, false),
            ZInlineFast::Slow(w) => (
                inline_barrier_slow(slot, w, self, ZBarrierKind::Load, BASE),
                true,
            ),
        }
    }
    fn take_marks(&self) -> Vec<u64> {
        std::mem::take(&mut *self.marked.lock())
    }
}

impl ZBarrierContext for FlipContext {
    fn good_mask(&self) -> u64 {
        self.good.good()
    }
    fn is_marking(&self) -> bool {
        self.marking.load(Ordering::Relaxed)
    }
    fn is_relocating(&self) -> bool {
        false
    }
    fn forward(&self, addr: u64) -> Option<u64> {
        Some(addr)
    }
    fn mark_live(&self, addr: u64) {
        self.marked.lock().push(addr);
    }
    fn stats(&self) -> &ZBarrierStats {
        &self.stats
    }
}

const BASE: u64 = 0x0000_7F00_0000_0000;

/// The fast path agrees with [`classify_bad_masked`] on every colored word
/// under every phase (slow exactly when it says `Bad`, `base + offset`
/// exactly when it says `Good(offset)`), passes plain words and null
/// through untouched where `classify_bad_masked` would truncate or reject
/// them, and with a bad mask of `0` is stage 2d's pinned decode.
#[test]
fn zt21zb4_the_inline_fast_path_agrees_with_the_classifier() {
    use crate::zgc::vaddr::{color, ZColor, ZGoodMask};
    let colors = [
        ZColor::Marked0,
        ZColor::Marked1,
        ZColor::Remapped,
        ZColor::Finalizable,
    ];
    let good = ZGoodMask::new();
    let mut bads = vec![good.bad()];
    good.flip_to_mark();
    bads.push(good.bad());
    good.flip_to_remap();
    good.flip_to_mark();
    bads.push(good.bad());
    for bad in bads {
        for c in colors {
            for off in [0u64, 0x1238, Z_OFFSET_MASK] {
                let w = color(off, c);
                let fast = inline_barrier_fast(w, bad, BASE);
                match classify_bad_masked(w, bad, Z_OFFSET_MASK) {
                    ZFastPath::Good(o) => assert_eq!(fast, ZInlineFast::Done(BASE + o)),
                    ZFastPath::Bad(raw) => assert_eq!(fast, ZInlineFast::Slow(raw)),
                }
            }
        }
        // Plain words and null never meet the color test.
        for plain in [0u64, 0x0000_7FFF_1234_5678, 0x0000_0FFF_FFFF_FFF8] {
            assert_eq!(
                inline_barrier_fast(plain, bad, BASE),
                ZInlineFast::Done(plain)
            );
        }
    }
    // The pinned decode: every colored word decodes, whatever its color.
    for c in colors {
        assert_eq!(
            inline_barrier_fast(color(0x80, c), 0, BASE),
            ZInlineFast::Done(BASE + 0x80)
        );
    }
    // An object at arena offset 0 is `base`, never null.
    assert_eq!(
        inline_barrier_fast(color(0, ZColor::Remapped), 0, BASE),
        ZInlineFast::Done(BASE)
    );
}

/// Stage 2e end to end on the barrier side: colored slots written at
/// `Remapped` are healed LAZILY through two mark cycles. Each mark start
/// makes every slot bad; the first compiled load of a slot marks its
/// target once and heals it to the cycle's mark color, the second is a
/// fast hit; the relocate flip makes the mark color bad and the first load
/// heals back to `Remapped` without marking; a slot nobody loaded during
/// cycle 0 is still bad (and marked once) in cycle 1 -- the stale mark bit
/// self-clears with no heap pass. A plain-word slot is never marked by a
/// load: what keeps its target is SATB (stage 2f's question).
#[test]
fn zt21zb4_a_color_rotation_heals_slots_lazily_and_marks_each_once() {
    use crate::zgc::vaddr::{color, color_of, ZColor};
    let ctx = FlipContext::new();
    let a = AtomicU64::new(color(0x100, ZColor::Remapped));
    let b = AtomicU64::new(color(0x200, ZColor::Remapped));
    let cold = AtomicU64::new(color(0x300, ZColor::Remapped));
    let plain = AtomicU64::new(0x0000_7FFF_0000_1000);
    let null = AtomicU64::new(0);

    // Quiescent (the pinned state stage 2d relies on): all fast.
    for (s, want) in [
        (&a, BASE + 0x100),
        (&b, BASE + 0x200),
        (&cold, BASE + 0x300),
    ] {
        assert_eq!(ctx.load(s), (want, false));
    }

    // Cycle 0, mark: every colored slot is bad once.
    ctx.mark_start();
    assert_eq!(ctx.load(&a), (BASE + 0x100, true));
    assert_eq!(ctx.load(&a), (BASE + 0x100, false), "healed: a fast hit");
    assert_eq!(ctx.load(&b), (BASE + 0x200, true));
    assert_eq!(ctx.load(&plain), (0x0000_7FFF_0000_1000, false));
    assert_eq!(ctx.load(&null), (0, false));
    assert_eq!(
        ctx.take_marks(),
        vec![0x100, 0x200],
        "each target marked once"
    );
    assert_eq!(color_of(a.load(Ordering::Relaxed)), Some(ZColor::Marked0));
    assert_eq!(
        plain.load(Ordering::Relaxed),
        0x0000_7FFF_0000_1000,
        "plain untouched"
    );
    ctx.mark_end();

    // Cycle 0, relocate: the mark color is bad, the heal marks nothing.
    ctx.relocate_start();
    assert_eq!(ctx.load(&a), (BASE + 0x100, true));
    assert_eq!(ctx.load(&a), (BASE + 0x100, false));
    assert!(
        ctx.take_marks().is_empty(),
        "no marking outside a mark phase"
    );
    assert_eq!(color_of(a.load(Ordering::Relaxed)), Some(ZColor::Remapped));
    assert_eq!(color_of(b.load(Ordering::Relaxed)), Some(ZColor::Marked0));

    // Cycle 1, mark: Marked1 good. `a` (Remapped), `b` (stale Marked0)
    // and `cold` (never loaded since cycle 0) are each bad once.
    ctx.mark_start();
    assert_eq!(ctx.good.good(), Z_MARKED1);
    for (s, want) in [
        (&a, BASE + 0x100),
        (&b, BASE + 0x200),
        (&cold, BASE + 0x300),
    ] {
        assert_eq!(ctx.load(s), (want, true));
        assert_eq!(ctx.load(s), (want, false));
        assert_eq!(color_of(s.load(Ordering::Relaxed)), Some(ZColor::Marked1));
    }
    assert_eq!(ctx.take_marks(), vec![0x100, 0x200, 0x300]);
    let snap = ctx.stats.snapshot();
    assert_eq!(
        snap.heal_cas_wins, 6,
        "two cycle-0 marks, one remap, three cycle-1 marks"
    );
    assert_eq!(snap.heal_cas_losses, 0);
}

/// A word that reaches the slow helper untagged (a stale decision) is
/// answered as the plain pointer and the slot is not recolored.
#[test]
fn zt21zb4_the_slow_helper_never_recolors_a_plain_slot() {
    let ctx = FlipContext::new();
    ctx.mark_start();
    let plain = 0x0000_7FFF_0000_2000u64;
    let slot = AtomicU64::new(plain);
    assert_eq!(
        inline_barrier_slow(&slot, plain, &ctx, ZBarrierKind::Load, BASE),
        plain
    );
    assert_eq!(slot.load(Ordering::Relaxed), plain);
    assert_eq!(ctx.stats.snapshot().slow_path_entries, 0);
    assert!(ctx.take_marks().is_empty());
}
