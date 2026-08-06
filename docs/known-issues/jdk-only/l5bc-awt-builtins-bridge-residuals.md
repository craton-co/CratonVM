# `native-awt` and `native-builtins` after L5b/L5c — what still inherits, and why

**Status:** OPEN — reclassification questions left by the L5b/L5c
`register_with_kind` migration, filed 2026-08-05. Nothing here is a crash.
What is open is that 7,748 registrations across the two crates are tagged
`Bridge` while the JDK 25 image says their target is not an `ACC_NATIVE`
method, so `--jdk-only` admits every one of them on a claim nobody has checked.

> **MOSTLY CLOSED 2026-08-06 — the statement pass this file calls "the obvious
> next increment" is finished.**
>
> * L5's criterion now selects **zero** rows tree-wide (was one:
>   `java/io/UnixFileSystem.list0`, in the very `nio_file.rs` double loop this
>   file declined to restructure — it needed the existing `fs_reg` helper, not a
>   restructure). The 21 mixed sites were split by
>   [`census-asks-one-class-on-one-platform.md`](census-asks-one-class-on-one-platform.md);
>   `hasDisplays0` and `ComponentSampleModel.initIDs` are both real bridges and
>   state their kind.
> * **The tree-wide totals this file quotes have moved and were overstated.**
>   `bridge` is 10,434 (not 10,842) and `synthetic-stub` 755 (not 387) on
>   2026-08-06; the unadjudicated `Bridge` population is 9,656 (L6's baseline was
>   a stale 9,675 and is re-frozen here — it fell twice while this was being
>   verified, 9,675 -> 9,660 -> 9,656, none of it this change's doing). More
>   importantly the metric itself double-counts: **1,092 of those rows own no slot
>   and can never dispatch**, so the live surface is **~8,564**. `owns_slot` is now a census
>   column rather than something a reader had to infer from row order, and
>   `jdk-only-adjudicate.py` breaks it out as a separate addend — L6's number is
>   left whole on purpose, for the same reason section 3 keeps the shadows
>   separate.
> * `native-collections` **checks out as written**: 1,356 rows today (989
>   `bridge` + 367 `synthetic-stub`) against the file's 1,350, and the image
>   declares `ACC_NATIVE` on **exactly zero** of them — re-measured per row, not
>   inherited. A statement lane still has nothing to do there.
>
> **Still open from this file:** nothing in its own file ownership. The 83
> tree-wide adjudicable rows it lists are gone (0 remain); what is left is
> reclassification, which is contract §8's wave, plus the 796-row deletion list
> now committed at `scripts/baselines/jdk-only-dead-everywhere.tsv`.

> **PARTLY SUPERSEDED 2026-08-05.** This record's headline finding — that the
> tree's only `bridge` marker outside `native-io` "is dead on this image" — was
> half the story. `sun/awt/PlatformGraphicsInfo.hasDisplays0()Z` **is**
> ACC_NATIVE, on the Windows and macOS images; a Linux census simply cannot say
> so, and one taken against an unpacked Windows JDK does. The marker was right.
> `java.awt.image.ComponentSampleModel.initIDs()V`, split out of the awt loop
> here as "not an adjudicated bridge", inherits a **native** `initIDs` from
> `java.awt.image.SampleModel`. Both state their kind now, and all 21 mixed
> sites named below have been split. See
> [`census-asks-one-class-on-one-platform.md`](census-asks-one-class-on-one-platform.md).

Sibling record for the crate L5 did first:
[`l5-native-io-bridge-residuals.md`](l5-native-io-bridge-residuals.md). The two
share a method and a conclusion; this one is where the *scale* of the ambient
default shows up.

> **This record now owns the reclassification question, 2026-08-06.** The
> retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up used
> to carry it. That record is closed: `current_category` is an `Option` and is
> scoped by the save/restore idiom the tree already used, so it restores the
> *absence* of a choice as well as the kind; nine registrars that had no scope
> at all now state theirs; no registration in a real boot runs on the
> constructor default (`kind_chosen: false` on 0 of 11,876); and
> `scripts/jdk-only-kind-map.py` freezes the kind of **every** registration, so
> a one-line ambient edit is a reviewable per-row diff instead of a silent
> thousand-row change. It also closed 58 triples `--jdk-only` was admitting
> because the drop happens at registration and the *surviving* copy of a
> twice-registered triple was whichever one was not tagged a stub.
>
> **None of that reclassified anything, and it could not have.** The 7,748 rows
> below are unchanged in kind; what changed is that nothing can move them
> quietly any more. The tree-wide figure is **9,571** `Bridge` registrations
> with no `ACC_NATIVE` target (JDK 25 / linux, 2026-08-06), ratcheted slack-free
> by `regression-suite/bridge-ratchet.sh`. Lowering it is this record's job and
> contract §8's wave, one subsystem at a time, with the census re-taken after
> each.

## What L5b/L5c did

L5 established the rule on `native-io`: a registration may state
`NativeKind::Bridge` at its own site only where the schema-3 census says the
image declares that exact triple `ACC_NATIVE`, because contract §1.5 defines a
`Bridge` as what an `ACC_NATIVE` method binds to. L5b applied it to
`native-awt`, L5c to `native-builtins`.

| lane | crate | rows | stated | still inherited |
|---|---|---:|---:|---:|
| L5b | `native-awt` | 188 | **21** | 167 |
| L5c | `native-builtins` | 9,243 | **582** | 8,652 |
| (L5, for comparison) | `native-io` | 1,013 | 87 | 926 |

(`native-builtins`' "still inherited" is 8,652 rather than 8,661 because nine
rows there already stated their kind before these lanes: the `String` and
`deprecated_util` intrinsics that landed 2026-08-05.)

**603 registrations across 475 call sites now state their kind; `kind_stated`
went from 96 rows to 699.** No native's kind changed, the registration totals
are identical, and L6's ratchet is unmoved — see *Verification* below.

## The finding: the one adjudicated `bridge` verdict outside `native-io` is dead on this image

`native-awt/src/natives.rs::register_headless_natives` carried the only
`JDK-ONLY-CLASSIFY: bridge` marker in the tree outside `native-io`:

> `sun/awt/PlatformGraphicsInfo.hasDisplays0()Z` is ACC_NATIVE in JDK 25: it is
> the display probe, an OS boundary with no bytecode fallback […]

On a **Linux** JDK 25 image it is not there at all. `javap -p
sun.awt.PlatformGraphicsInfo` lists `createGE`, `createToolkit`,
`getDefaultHeadlessProperty` and `getDefaultHeadlessMessage` — four ordinary
bytecode methods, no `hasDisplays0`. The census agrees per row: `declared:
false`. `hasDisplays0` belongs to the Windows and macOS variants of the class,
which a Unix image does not ship.

So **the registrar that was the most confident about being a bridge is the one
registrar in `native-awt` that states nothing.** This is step 3 of the L5 recipe
doing exactly its job: it is a reclassification question — is a registration
that can only ever bind on another platform a bridge, a dead entry, or both? —
and not something a migration may decide. It also means **`ABSENT`/`UNDECL` on a
platform-named class is "not measured here", never "dead"**, and no automated
pass may treat the two the same until a Windows-image census exists.

The crate marker's old count is corrected in the same change: it said "only 10
of the 122 target an ACC_NATIVE method". The true figure is **21 of 188
registration rows** — the 122 was a count of *sites*, and `natives.rs`
deliberately registers 27 drawing primitives three times over — and
`hasDisplays0` is not among them.

## `native-awt` — 167 of 188 still inherit

The whole crate takes its kind from one line, `lib.rs::register_awt_natives`'s
`with_category(NativeKind::Bridge, natives::register_all)`. That line stays: it
is what keeps these registered under `CRATONVM_NO_STUBS`, and t7 desktop
conformance depends on them. What the census says about the 167:

| verdict | rows | what it means here |
|---|---:|---|
| `CODE` — concrete bytecode | 103 | the native shadows real Java. `javax.swing` is pure Java; so are most of `java.awt`. |
| `UNDECL` — class present, method not | 32 | `hasDisplays0` and friends: platform-variant or removed spellings |
| `ABSTRACT` | 32 | on `java.awt.Toolkit`, `Component`, … — these intercept **every** implementor, including a user subclass |

The 21 that do state their kind are the JNI field-ID caches (`Toolkit.initIDs`,
`Disposer.initIDs`, and twelve of the thirteen `java.awt.image` /
`sun.awt.image` `initIDs`) plus the `JPEGImageReader`/`JPEGImageWriter`
bootstrap and lifecycle natives. The thirteenth `initIDs`,
`java.awt.image.ComponentSampleModel`, is not declared at all — its superclass
`SampleModel` owns the family's one ID cache — so the loop was split rather than
claimed whole, the same way L5 split `nio_native.rs`.

## `native-builtins` — 7,581 `Bridge` rows the image does not back

| verdict | rows |
|---|---:|
| `CODE` — concrete bytecode (a shadow) | 3,646 |
| `UNDECL` — class present, method not declared | 2,020 |
| `ABSENT` — class not in the image (third-party natives) | 992 |
| `ABSTRACT` | 923 |

This is the bulk of the tree-wide 10,069 that L6's ratchet pins, and none of it
is L5c's to decide. What L5c can say is that the 582 it *did* state are now
beyond doubt: `jmx.rs` 107, `jfr.rs` 75, `unsafe_natives_ext.rs` 36,
`lang_invoke.rs` 33, `zip_real.rs` 24, `reflect_annotations.rs` 19,
`unsafe_natives.rs` 19, `net_phase_e.rs` 12, `inet_address.rs` 11,
`phases_late/concurrent.rs` 11, and 206 in `lib.rs` — every one of them a
method JDK 25 declares `ACC_NATIVE`.

`native-builtins/src/lib.rs` was migrated in its own commit. Contract §8 says
the 157-stub reclassification there is "a separate wave with its own
subsystem-per-PR discipline"; stating a kind is not reclassifying one, but the
discipline is worth keeping either way.

## The 21 sites left alone, and the single shape they share

A site is skipped when its census rows disagree — some `ACC_NATIVE`, some not.
There are 21 such sites in `native-builtins`, holding 24 adjudicable rows
between them, and every one is the same idiom: **a loop over a compatibility set
in which only the running JDK's spelling exists.**

| where | shape |
|---|---|
| `phases_late/nio_file.rs` ×18 | `for fs_cls in ["java/io/WinNTFileSystem", "java/io/UnixFileSystem"]`, often crossed with `for name in ["getLength", "getLength0"]` — one class per platform, one name per JDK generation. On a Unix image exactly one of the four combinations is `ACC_NATIVE`. |
| `unsafe_natives.rs` ×3 | `for name in [compareAndExchangeInt, …Acquire, …Release, weakCompareAndExchange*]` — JDK 25 declares some of the family and not the rest |
| `jmx.rs` ×1 | a loop over `sun/management/VMManagementImpl` probes; `isThreadAllocatedMemoryEnabled` and `isThreadContentionMonitoringEnabled` are native, the `*Supported` siblings are bytecode |
| `jfr.rs` ×1 | `for descriptor in ["(IJ)J", "(I)J"]` on `JVM.getStackTraceId` — only the JDK 25 arity is declared |

Splitting these is the same operation L5 performed on `nio_native.rs` and
`net.rs`, and it is the obvious next increment. It was not done here because
`nio_file.rs`'s block is a 400-line nested double loop over live filesystem
natives, and a restructure of that is a change with real behaviour risk that
should not ride along with a mechanical statement pass.

## `native-collections`: measured, and there is nothing a migration may state

Worth recording because it is the one crate everybody expects to be the problem.
`native-collections/src/lib.rs`'s single `set_category(Bridge)` covers **1,350
registration rows, and the image declares `ACC_NATIVE` on exactly zero of
them.** The crate's own `JDK-ONLY-CLASSIFY: stub` marker said so from a static
`javap -p -s` read ("not one of those 1,195 targets an ACC_NATIVE method"); the
runtime census confirms it per row, at the larger row count.

So a `register_with_kind` lane has **nothing to do in `native-collections`** —
not "not yet", but nothing, by the criterion. Every row there is a
reclassification question, and the marker's own warning stands: flipping that
line is the 2026-07-14 regression shape at ~8× blast radius, and 214 of the
registrations are on abstract interface methods that decide dispatch for every
*user* subclass, not just for `java.util`.

## Not in these lanes' file ownership

83 adjudicable rows remain tree-wide after L5b/L5c:

* **49 in `native-io`** — `lib.rs` 32, `nio_selector.rs` 10, `file_channel.rs`
  5, `direct_buffer.rs` 2. All sit under `JDK-ONLY-CLASSIFY: unknown — needs
  census` markers, which L5's rules deliberately excluded ("take the census
  first"). The census now exists, so these are ready; several of the markers
  also carry their own hazards (`CONCRETE OVERWRITE HAZARD`) that want reading
  before anything is stated.
* **24 in the 21 mixed `native-builtins` sites** above.
* **10 in `vm/src/runtime/instrument.rs`** — the `vm` crate, which other wave-2
  lanes own.

## Verification

Two release binaries from the same tree, JDK 25 / Linux, 2026-08-05.

* **Census `--real-jdk`:** per-kind totals identical (`intrinsic 687, bridge
  10842, synthetic-stub 387, total 11916`); **zero `kind` changes** across
  10,780 distinct triple+kind rows; `kind_stated` 96 → 699, **+603 exactly**,
  every newly-stated row `bridge`.
* **Census `--jdk-only`:** same, 11,529 rows, `synthetic-stub: 0` before and
  after.
* **`CRATONVM_NO_STUBS=1`, both arms:** boots, and the dropped-stub list is
  **byte-identical at 436 entries**.
* **L6 `bridge-ratchet.sh`:** PASS, unmoved at 10,069 / 4,755 — and it could not
  have moved, for the reason spelled out in the L5 record: the rows a migration
  may state are exactly the rows that *have* an `ACC_NATIVE` target, so they
  were never in the 10,069.
* **`stub_ratchet`:** 4 passed, `BASELINE_SYNTHETIC_STUBS = 157` unmoved.
* **`cargo test --release -p cratonvm-native-builtins --lib`:** 3,275 passed,
  0 failed.
* **`cargo test --release -p cratonvm-native-awt`:** 259 passed, 1 failed —
  `image::tests::get_rgb_oob`, which **fails identically on unmodified dev**
  (verified by stashing the change and re-running). Pre-existing, not L5b's.
