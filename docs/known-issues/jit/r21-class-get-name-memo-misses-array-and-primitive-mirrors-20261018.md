# `Class.getName()` costs ~500 ns in a mixed loop: the leaf memo serves only mirrors with a class id, through three locks

Status: FIX IN CODE (JIT round 21 wave 1, lane reflect, opt-in `CRATONVM_CLASS_GET_NAME_MIRROR_MEMO`), PENDING HOST MEASUREMENT; soundness defect
(`r21-optin-flip-review-20261019.md` #13) FIXED in wave 24, lane names (pending build)
Area: `native-builtins/src/lang_class.rs` (`native_class_get_name_leaf`, `class_get_name_memo_hit`,
`native_class_get_name_memoizing`; the reviewed `Intrinsic` registration of `java/lang/Class.getName()` in
`native-builtins/src/lib.rs`)
Found by: JIT round 21 orchestrator baseline (`tools/probes/jit/r21/R21TriageRows.java` `time class-name`:
499 ns/op on CratonVM, 0.8 ns on HotSpot 25), traced by lane reflect

## What (by reading; not profiled)

There is no stand-in to remove. JDK 25's body is `name != null ? name : initClassName()`, but this VM's mirror
allocator writes the INTERNAL name into `Class.name` (read in that form by `mirror_class_name_strict` and its
callers), so the reviewed `Intrinsic` stays in front of it. Since r20 (`CRATONVM_CLASS_GET_NAME_MEMO`, default
ON) it is claimed leaf, and its hit is `class_get_name_memo_hit`:

* `class_id_from_mirror` (the reverse map's read lock),
* the per-VM `ReflectDoorCaches.class_names` read lock and a hash lookup,
* `resolve_global_root` (the global-ref table's lock and a hash lookup),
* a field read and two array reads.

The r20 record measured that hit at 154-169 ns. A mirror with no reverse-map id NEVER hits: primitive mirrors
(433-491 ns on r20, the full funnel each call), and, the likely reason the 8-class mixed row averages ~500 ns,
any array mirror the reverse map does not hold. The triage loop cycles `int[].class` and `Object[].class` with
six plain classes. Both kinds then decline the leaf, run the funnel, recompute and intern the name, and cannot
file a row because `class_id_from_mirror` answers `None`. Not confirmed: the probe split below decides whether
arrays miss.

## Fix in code (opt-in)

`CRATONVM_CLASS_GET_NAME_MIRROR_MEMO` (row `class-get-name-mirror-memo`, `off_word: None`; reader
`NativeFlags::class_get_name_mirror_memo`). A per-thread, direct-mapped memo (64 lines,
`CLASS_GET_NAME_MIRROR_MEMO`) keyed by the VM identity, the collection address epoch (`gc_address_epoch`)
and the mirror's ADDRESS, guarded per hit by the mirror's `name` slot word, answers the noted `String`.
The leaf asks it first: one TLS read, the epoch load, one field read, no lock. It is noted by the funnel
path for every mirror (arrays and primitives included), and by a class-id memo hit. Soundness is
`REAL_HANDLE_MEMO`'s: within one address epoch nothing moves or is freed, so the same address is the same
mirror and the noted answer is the same live interned `String`. The answer is a function of the mirror
(its class id, fixed for a mirror, and its `name` slot, compared per hit). Identity holds, as on HotSpot.

It is opt-in because it is a performance change (the round's switch rules); it is not a dispatch defect.

### Wave 24 (lane names): only strongly held answers are noted

The soundness argument above was incomplete: the epoch keeps a line from naming another object, but a line is not
a root and a hit takes no keep-alive. For a mirror with no class id the answer was held only by the weak intern
table, so during a concurrent mark it could be handed to Java, stored behind the marker and reclaimed (flip review
#13). Now:

- `native_class_get_name_memoizing` notes a line only when the answer is held strongly: the class-id row was filed
  (`class_get_name_memo_fill` answers `true`), or, for a mirror with no class id,
  `class_get_name_hold_idless_answer` made a per-VM hold (`ReflectDoorCaches::class_names_idless`: answer text ->
  a global root of the interned answer, as HotSpot's `Class.name` field holds it). A class-id memo hit notes from
  its row, which is a global root;
- the hold table has one row per distinct text (primitives, arrays the reverse map does not hold), never removed
  before the VM goes. A row can outlive an unloaded array class's mirror: memory only;
- NOT gated on `!jit_enabled()`. The memory rule is about intrinsics that lose to compiled bytecode; here the
  JDK body cannot run (the internal name is in `Class.name`), so with the JIT on the same leaf `Intrinsic` is called
  either way, and the line only replaces the class-id hit's three locks. The A/B below should still be run with
  `--nojit` and the default.

Test: `lang_class` `tests::r21w24_mirror_memo_notes_only_strongly_held_idless_answers`. Missing row (the flip
gate): `int[].class.getName()` / `int.class.getName()` stored into a list under a concurrent mark (`-Xmx64m`, Gen
and G1), then `System.gc()` and every stored name compared.

## How to verify

`tools/probes/jit/r21/R21ReflectInvoke.java`: `time class-name-plain` / `-array` / `-primitive` / `-mixed8`,
default vs `CRATONVM_CLASS_GET_NAME_MIRROR_MEMO=1`, interleaved, both modes. Expected:
* the default arm's `-array` row near `-primitive` (arrays miss) or near `-plain` (they hit, and the cost is
  the three locks plus the door);
* the arm on: all four rows at or below the default `-plain` row.

`fact class-names` must equal HotSpot in both arms. `R21TriageRows class-name` with the arm on is the
orchestrator's row. Engagement: `CRATONVM_INTRINSIC_STATS=1` "compiled leaf-native dispatches" grows with
the calls.

Next if the arm-on rows stay well above ~20 ns: the leaf door itself (`try_lean_native_door` / the MIC
helper prefix) is the floor, and the remaining step is an IR intrinsic for `Class.getName()` reading the
memo inline.
