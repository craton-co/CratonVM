# `gc/src/zgc/` — closing cross-module consistency audit

*Written 2026-08-07 against `dev@a355cb63d`, by a read of all twelve modules
under `gc/src/zgc/` (20 093 non-test lines), `gc/src/zgc.rs`,
`gc/src/zgc_concurrent.rs`, `gc/tests/zgc_module_integration.rs`,
`.github/workflows/ci.yml`, and the `cratonvm_types` / `gc/src/heap.rs` /
`gc/src/tlab.rs` / `gc/src/card_table.rs` constants they claim against.*

**No code was changed by this audit and no compiler was run.** Every claim below
is derived from source and anchored to `file:line`. Where a conclusion rests on a
convention rather than a check, that is stated.

### ⚠ Line anchors, and the fact that the tree moved during this audit

Four of the files below were **edited by other agents while this audit was being
written**. Observed working-tree mtimes at 13:31:08:

| file | mtime | size |
|---|---|---|
| `gc/src/zgc/mark.rs` | **13:30:20** | 169 968 B (was 128 487 B at 11:11) |
| `gc/src/zgc.rs` | **13:28:21** | 223 511 B (was 177 472 B at 12:47) |
| `gc/src/zgc/barrier.rs` | **13:24:26** | 127 388 B (was 121 891 B at 12:34) |
| `gc/src/zgc/forwarding.rs` | 13:00:39 | 115 676 B (was 113 829 B at 12:50) |

Every anchor in this document was **re-derived at 13:31** against the working
tree, so the numbers below are current as of that minute and not as of
`a355cb63d`. They will drift again. **Grep the symbol name, which is given
alongside every load-bearing anchor, rather than trusting the line.**

Two consequences worth stating rather than burying:

* A ninth blind pass landed on `mark.rs` at 13:30 — a "protocol audit" that added
  dated notes at `mark.rs:132`, `:914`, `:1359`, `:1821`, `:3502`, `:3685`.
  I re-checked afterwards: it added **no** mention of `heap offset`,
  `Z_OFFSET_MASK`, `offset domain`, or `barrier.rs` anywhere in the file. **N3 —
  the highest-severity finding here — survived it.**
* Findings against these four files may have been fixed after 13:31. Findings
  against the other ten files (`vaddr`, `page`, `metrics`, `remembered`,
  `generation`, `relocate`, `tlab`, `census`, `adapters`, `zgc_concurrent`) and
  against `gc/tests/zgc_module_integration.rs` and `.github/workflows/ci.yml`
  were read against files untouched since 12:52 or earlier.

Companion audits: [`zgc-vmheap-arm-audit.md`](zgc-vmheap-arm-audit.md),
[`gc-crate-audit.md`](gc-crate-audit.md), [`g1-audit.md`](g1-audit.md).

---

## 0. Headline

The twelve modules were written in parallel by authors who could not see each
other, an integration suite found ten cross-module contradictions, and roughly
eight reconciliation passes then landed — each also blind to the others. **All
ten prior findings are genuinely resolved in source.** That is the good news and
it is real: `barrier.rs`'s encoding, `remembered.rs`'s page-id width and
`HEADER_SIZE`, `forwarding.rs`'s payload newtype, the offset-0 null bug, the
tag-less heal color, the missing `ConcurrentRemap` phase, the occupancy
denominator, the promoting-minor `debug_assert`, and the barrier/relocator
address domain are each fixed, each with a regression pin.

The bad news is the thing the eighth pass could not see:

> **`gc/tests/zgc_module_integration.rs` — the suite that found all ten — does
> not compile, and no CI job builds it.** Two `u32` page-id call sites
> (`zgc_module_integration.rs:1950`, `:1961`) survived the widening that the
> same pass performed 200 lines earlier in the same file. CI's only ZGC test
> step is `cargo test -p cratonvm-gc --lib --features zgc`
> (`.github/workflows/ci.yml:504`) — **`--lib`**, which excludes every
> integration target — and the `cargo check --all-targets` step
> (`ci.yml:425-429`) is scoped to `-p cratonvm-vm -p cratonvm-native-builtins`,
> not `cratonvm-gc`.

So the ten contradictions were fixed against a suite that has been unbuildable
for an unknown fraction of the day, and none of the eight passes was validated
by it. That single fact reframes everything else in this document: **twenty-six
new findings below were produced by a process with no cross-module check
running.**

### Severity legend

| | |
|---|---|
| **CRITICAL** | Breaks the build, or would corrupt memory the moment the module is adopted. |
| **HIGH** | Two modules disagree about a load-bearing fact; adopting either as written is a use-after-free or a silent wrong answer. |
| **MEDIUM** | A real disagreement whose blast radius is bounded today, or a doc that will mislead the next implementor into one. |
| **LOW** | Stale prose, dead anchors, arithmetic in a comment. Cheap to fix, and each one erodes the docs' authority. |

---

## 1. Verdict table

### 1a. The ten prior findings

| # | Finding | Verdict | Anchor |
|---|---|---|---|
| 1 | `barrier.rs` copied colored-pointer constants from the `zgc.rs` simulation | **RESOLVED** | `barrier.rs:307` is now `pub use super::vaddr::{…}`; the deleted duplicate and *why the two encodings differed* are recorded at `barrier.rs:289-305`. Pinned by `barrier.rs:1841`/`:1853` (`barrier_reexports_are_bit_identical…`) and `zgc_module_integration.rs:1846`. |
| 2 | `remembered.rs` used `u32` page ids | **RESOLVED (in `src`)** | Every page-id-carrying site is `u64`: field `remembered.rs:319`, accessor `:377`, `register_old_page` `:801`, `get` `:828`, `remove` `:840`, `remember` `:860`, `page_ids` `:875`, `page_of` `:1106`, `page_base` `:1163`, `record` `:1388`. **But see N1 — the test binary was not widened.** |
| 3 | `remembered.rs` hard-coded `HEADER_SIZE = 24` | **RESOLVED** | No literal survives. `remembered.rs:216-218` are `const _: () = assert!()`s against `cratonvm_types::{HEADER_SIZE, SLOT_SIZE, ARRAY_DATA_OFFSET}`; the doc table at `:47-51` cites `types/src/heap_types.rs:19/171/176/185/300` and **every one of those anchors is correct**. |
| 4 | `forwarding.rs`'s 41-bit `to` could not hold a Linux `mmap` base | **RESOLVED** | `ZForwardingPayload` newtype at `forwarding.rs:546`; `find` → `find_payload` at `:1135`; `try_insert`/`insert`/`snapshot_payloads` all traffic in the newtype (`:985`, `:1092`, `:1173`). The false justification is replaced by a re-derivation at `:252-335` (`ZFWD_TO_BITS`), and `ZFWD_MAX_PAYLOAD` is documented as a bound on the *encoded* value at `:466-472`. |
| 5 | `barrier.rs`'s null test made a live object at heap offset 0 a silent null | **RESOLVED** | The fast path is now bad-mask form (`classify_bad_masked`, `barrier.rs:935`), which `z_barrier` selects. The slow path tests `observed == Z_NULL`, not the payload (`barrier.rs:1185`). Both changes carry their derivations in the `classify_bad_masked` doc and immediately above `:1185`. |
| 6 | `heal_color()`'s default omitted `Z_COLORED_TAG` | **RESOLVED** | `barrier.rs:603` (`fn heal_color`) is `Z_COLORED_TAG \| self.good_mask()`. The rejected alternative (mandatory override + `debug_assert`) and *why* it was rejected are in the same doc block. Pinned in-module by `the_default_heal_color_carries_vaddrs_tag_bit` (`barrier.rs:2088`) over a `DefaultsOnlyContext` (`:2045`). **Not** pinned by the integration suite — see N20. |
| 7 | `metrics::ZgcPhase` lacked `ConcurrentRemap` | **RESOLVED** | `metrics.rs:249`; `ZGC_PHASE_COUNT` 10 → 11 at `:176`; every array/TSV width is symbolic and consistent (`:261`, `:571`, `:1918`). Decision recorded at `:52-100`. |
| 8 | `page.rs` and `forwarding.rs` divided occupancy by different denominators | **RESOLVED** | Settled in favour of the allocated extent. `ZPageReal::relocation_capacity_bytes` = `used()` (`page.rs:701-703`) with the full argument at `:632-693`; `PageCandidate::capacity_bytes` re-documented at `forwarding.rs:1221-1235`; `adapters.rs` collapsed its two mappings into one (`adapters.rs:197-204`) and says so at `:31-35`. The two gates now agree exactly: `page.rs:708` is `live_ratio() < t`, `forwarding.rs:1418` is `>= t` → skip. |
| 9 | `generation.rs::collect_young`'s `debug_assert` panicked on every promoting minor | **RESOLVED** | `generation.rs:1757-1765` now asserts `old_live_before + out.bytes_promoted == self.old.live_bytes()`, with the old form and why it was wrong at `:1748-1755`. Correct — but load-bearing on a non-obvious ordering (`set_live_bytes` at `:1898` before `adopt_page` at `:1234`) that nothing else guards. |
| 10 | The address domain: barrier=offset, remembered=machine, relocate=absolute | **RESOLVED for `relocate.rs`; INCOMPLETE overall** | `relocate.rs` landed `forward_offset` (`:2203`) and `forward_lookup_offset` (`:2248`) with a release-build domain check (`check_offset_domain`, `:2139`), and states the two-domain table at `:105-113`. **But `mark.rs` was never brought into the decision — see N3 — and `barrier.rs`'s own header still describes the pre-fix `relocate.rs` — see N5.** |

### 1b. New findings

| # | Sev | Finding | Anchor |
|---|---|---|---|
| **N1** | CRITICAL | The integration suite does not compile: `u32` page ids passed to `u64` parameters | `zgc_module_integration.rs:1950`, `:1961` vs `remembered.rs:801`, `:828` |
| **N2** | CRITICAL | No CI job compiles that test target, so N1 is invisible and no reconciliation pass was validated | `.github/workflows/ci.yml:504`, `:425-429` |
| **N3** | HIGH | `mark_live` crosses the offset/machine-address boundary with no adapter, no guard and no note on either side | `barrier.rs:1253` vs `mark.rs:303`, `zgc.rs:2831`, `:2901` |
| **N4** | HIGH | `relocate.rs` forbids the `Err` fallback and then prints it as the sanctioned body — and `barrier.rs`'s trait cannot express the alternative | `relocate.rs:2183-2186` vs `:2196`; `barrier.rs:650` |
| **N5** | HIGH | `barrier.rs`'s module header describes a `relocate.rs` that no longer exists, with two dead line anchors | `barrier.rs:120-140`, `:691-697` vs `relocate.rs:2203`, `:2248`, `:1753`, `:1799` |
| **N6** | HIGH | `ZPageReal::age` has two shipped writers with incompatible meanings | `tlab.rs:176-178`, `:889` vs `generation.rs:1906-1907`, endorsed by `page.rs:128-129` |
| **N7** | HIGH | `remembered.rs`'s only shipped `ZGenerationContext` violates the page-id contract stated 80 lines above it | `remembered.rs:1103-1105` vs `:1192`; `page.rs:1258` |
| **N8** | MED-HIGH | `zgc.rs`: "NONE of them is wired into `ZgcRealHeap` yet" is false — census is wired *and running* | `zgc.rs:98-101` vs `:1562`, `:1647`, `:2565`, `:2837`, `:3540` |
| **N9** | MED-HIGH | `zgc_concurrent.rs` says no `ZMarkContext` impl exists for `ZgcRealHeap`; one does | `zgc_concurrent.rs:96-101`, `:151-155` vs `zgc.rs:2837` |
| **N10** | MEDIUM | The mutator mark hand-off is built twice, with opposite verdicts on `thread_local!`/process globals, each citing `satb.rs` as authority | `mark.rs:120-125`, `:802`, `:808`, `:974` vs `barrier.rs:1429`, `:1436`, `:1441`, `:1500`, `:1533` |
| **N11** | MEDIUM | Two `object_size` trait methods, identical signature, opposite meaning for `0` | `mark.rs:373-377` vs `relocate.rs:520-527`; `zgc.rs:3024-3029` already implements one |
| **N12** | MEDIUM | `generation.rs` assigns the old-slot read to `remembered.rs`; `adapters.rs` quotes that sentence and refutes it | `generation.rs:421-425` vs `adapters.rs:367-372`, `:387-422`; `remembered.rs:1017-1022` |
| **N13** | MEDIUM | `ZRangeGenerationContext`: "a genuine implementation, not a test double" vs "unusable against a real heap" | `remembered.rs:1116-1120` vs `adapters.rs:236-243` |
| **N14** | MEDIUM | `census.rs`'s slot-width facts have drifted from narrow-oop reality and are stated as absolutes | `census.rs:39`, `:239-242`, `:446-448` vs `zgc.rs:2676-2685`, `:2356-2366` |
| **N15** | MEDIUM | `census.rs`'s headline validity gate is structurally unable to fail | `census.rs:576`, `:914` vs `zgc.rs:2181`, `:2574`, `:2623`, `:2627` |
| **N16** | MEDIUM | `tlab.rs`'s stated reason-for-existing #3 (allocation colour) is declared and never called | `tlab.rs:42-44`, `:257`, `:286` vs `:674-687` |
| **N17** | MEDIUM | `tlab.rs::footprint_of` omits `ZPAGE_MIN_ALLOC`, bypassing the walker invariant on the path TLABs exist to make hot | `tlab.rs:483-488` vs `page.rs:66-71`, `:741` |
| **N18** | MEDIUM | `ZGenerationMarker` has no producer and its "page absent ⇒ dead" default is the dangerous direction | `generation.rs:460-466`, `:1892-1909`; `mark.rs` has no per-page liveness |
| **N19** | MEDIUM | `generation.rs`'s occupancy numerator (`used()`) disagrees with the allocator's budget gate (`committed`) by up to 8× | `generation.rs:1054-1057`, `:1484-1487` vs `page.rs:1393`, `:1399` |
| **N20** | MED-LOW | `adapters.rs`'s three conversions are duplicated unchanged inside the test binary, whose docs still say `src` has none — and the test never imports `adapters` | `zgc_module_integration.rs:104-110`, `:1215`, `:1264` vs `adapters.rs:121`, `:271`, `:543` |
| **N21** | MED-LOW | The integration fixture overrides `heal_color` unconditionally, so its `// use the trait default` comment is false and finding 6 has no pin in that file | `zgc_module_integration.rs:164-181`, `:505` |
| **N22** | LOW | `metrics.rs` violates its own "never write 58 (or 54) anywhere"; two sizing claims stale after 10 → 11 | `metrics.rs:173-175` vs `:99-100`, `:1173-1174`; `:182-183`, `:137` |
| **N23** | LOW | The only `ZgcPhase` ↔ `ZgcPhase` mapping is `#[cfg(test)]`, so the claimed compile-time gate does not bind `cargo build`; nothing in `gc/src/` constructs `ZgcMetrics` | `metrics.rs:1460-1463`, `:1471-1488`; `zgc.rs:3539` |
| **N24** | LOW | `census.rs`: "The two orderings that are *not* relaxed do not exist: there are none" — there are two | `census.rs:156-161` vs `:1377`, `:1452` |
| **N25** | LOW | Five modules still say their siblings are "being written in parallel" / "in flight" | `remembered.rs:281-283`, `:994-996`, `:1119-1120`; `relocate.rs:509-514`; `tlab.rs:146-153` |
| **N26** | LOW | Six stale `file:line` anchors, including one that now points at the *fix* for the defect it cites | `barrier.rs:124`; `remembered.rs:288-290`; `census.rs:123-126`, `:522-525`, `:49`; `tlab.rs:115` |
| **N27** | LOW | `vaddr.rs` describes an `Arena`-backed relocatable heap; `page.rs` reserves one `Vec<u8>` "allocated once and never resized" | `vaddr.rs:74-101`, `:877-883` vs `page.rs:1177-1179`, `:1240` |
| **N28** | LOW | `tlab.rs::retire_all` claims a mutex replaces a suspension handshake, concedes it cannot three lines later, and its own post-condition assert can fire for that reason | `tlab.rs:109-111`, `:1242-1245` vs `:112-118`, `:1272-1276` |

---

## 2. The new findings in detail

### N1 — CRITICAL — the integration suite does not compile

`gc/tests/zgc_module_integration.rs:1950`:

```rust
        table.register_old_page(id as u32, page.size());
```

and `:1961`:

```rust
            .get(page.id() as u32)
```

`ZRememberedSetTable::register_old_page` takes `page_id: u64`
(`remembered.rs:801`) and `get` takes `page_id: u64` (`remembered.rs:828`). Rust
performs no implicit integer widening at a call site, so both are `E0308:
mismatched types`. The enclosing test is
`page_ids_narrow_losslessly_from_page_rs_into_the_remembered_set_table`
(`:1925`), and its whole premise — the doc block at `:1908-1922`, "`remembered::
ZRememberedSetTable` and `ZRememberedSet::page_id` use **`u32`**" — is now
false.

**This is a reconciliation-pass artifact, and the evidence is 200 lines up in
the same file.** `HeapGenerationContext::page_of` at
`zgc_module_integration.rs:1229-1234` was updated by the widening pass:

```rust
        // No narrowing check any more: `remembered.rs` was widened to `u64`
        // page ids on 2026-08-07, which is what this seam was flagging. The
        // assertion that used to stand here (`id <= u32::MAX`) is now both
        // dead and misleading, so it is gone rather than left as folklore.
```

The same pass left `:1908-1988` untouched, and left the stale `u32` claim in
`HeapGenerationContext`'s own doc header (`:1207-1214`) and in
`TableBackedRememberedSetView`'s field comment (`:1266`, "`u32 page id -> page
base address`", on a field declared `HashMap<u64, u64>`).

**Fix.** Delete the narrowing from `:1935-1963` and rewrite the test to assert
the property that now matters — that two ids congruent modulo 2³² are two
different remembered sets, which is exactly what `remembered.rs:1911` already
pins in-module and which this file should pin *across* the seam. Update
`:1207-1214` and `:1266`.

### N2 — CRITICAL — nothing in CI compiles that target

`.github/workflows/ci.yml:502-504`:

```yaml
      - name: Test the ZGC collector (cratonvm-gc unit tests)
        timeout-minutes: 20
        run: cargo test -p cratonvm-gc --lib --features zgc
```

`--lib` restricts the target set to the library. `gc/tests/*.rs` are integration
targets and are not built. The only `--all-targets` step that names the `zgc`
feature is `ci.yml:425-429`, and it is scoped `-p cratonvm-vm -p
cratonvm-native-builtins` — not `cratonvm-gc`.

The job's own comment block (`ci.yml:445-490`) is a long, well-written account
of exactly this failure mode happening twice before: "NOTHING BUILT THE BINARY"
(`:456`), "THE COLLECTOR'S OWN TESTS RAN NOWHERE" (`:466`), and "which is how
`census.rs`'s 24 tests sat unexecuted until it was declared" (`:479-480`). The
third instance was already on disk when that comment was written.

**Consequences, stated plainly.**

* N1 has been invisible since it landed.
* The suite that produced the ten findings has not run since. Every "resolved"
  in §1a rests on my read of source, on the modules' own `#[cfg(test)]` suites
  (which CI does run), and on nothing else.
* The three `#[ignore]`d tests in §3 could not have been re-evaluated by anyone,
  because re-evaluating them means running the file.

**Fix.** Add `cargo test -p cratonvm-gc --features zgc` (no `--lib`) beside
`ci.yml:504`, or extend `:425-429` to include `-p cratonvm-gc`. The first is
better: it also *executes* the seam assertions, and the seam is where the
remaining risk is.

### N3 — HIGH — `mark_live` crosses the address domain, unguarded, unassigned

The barrier passes a **heap offset**. `barrier.rs:1253`:

```rust
            ctx.mark_live(destination);
```

`destination` was bounds-checked as an offset twenty-seven lines earlier
(`barrier.rs:1226`, `if !is_bare_offset(destination, address_mask)`), and
`barrier.rs:60-61` states the rule: "every address the barrier takes or returns
is a **42-bit heap offset**, never a machine pointer".

The mark engine expects a **machine address**. `mark.rs:303`:

```rust
/// Every `u64` crossing this trait is an **unmasked machine address of an
```

and the only real implementation dereferences it — `zgc.rs:2901` does
`self.header_ref(addr as usize as *mut u8)`, with `zgc.rs:2831` restating the
same domain claim.

Neither side records the conflict. `barrier.rs:108-146` enumerates "What the
other modules must now do" and names `page.rs`, `forwarding.rs`, `relocate.rs`
and `remembered.rs` — **`mark.rs` is absent from that list.** As of 12:34
`mark.rs` carried no dated note of any kind, alongside `census.rs` and
`generation.rs`; a pass at 13:30 added six (`mark.rs:132`, `:914`, `:1359`,
`:1821`, `:3502`, `:3685`) and **none of them is about the address domain** —
the file still contains no occurrence of `heap offset`, `Z_OFFSET_MASK`,
`offset domain` or `barrier.rs`. `adapters.rs`, whose entire job is these
seams, contains zero references to `mark::`.

**Why this is the most dangerous item in the document.** The guard that exists
on the barrier side (`is_bare_offset`) *passes* an offset — that is what it is
for. The guard on the mark side is `is_in_heap` (`mark.rs:371`), which for a
real heap is a registry membership test: a 42-bit offset is not a registered
object base, so it is refused and counted in `ZMarkStats::off_heap_children`
(`mark.rs:459`, surfaced at `:521`). So the first symptom of wiring the barrier to the marker is
not a crash — it is **every barrier-driven mark silently discarded**, reported
as a wild-pointer counter, while the objects those marks were meant to keep
alive are swept. That is a use-after-free that presents as a plausible
diagnostic.

**Fix.** One of two, and it is a decision, not a patch: either `mark.rs` joins
the offset domain (it would need `Z_OFFSET_MASK` awareness it currently and
deliberately has none of), or `adapters.rs` grows the conversion and
`barrier.rs:108-146` grows a `mark.rs` bullet. The second is smaller and matches
the pattern `relocate.rs` already set with `forward_offset`.

### N4 — HIGH — `relocate.rs` forbids the fallback and then prescribes it

`relocate.rs:2183-2186`:

```
    /// * `Err(_)` — the offset is outside the reservation, or relocation was
    ///   needed and failed. The barrier must **not** fall back to `offset`: the
    ///   from-space page may already be quarantined, and handing back a
    ///   from-space reference is the use-after-free ZR-1 exists to prevent.
```

Ten lines later, `relocate.rs:2188-2199`:

```
    /// The sanctioned `ZBarrierContext::forward` body over a `ZRelocate` is
    /// therefore:
    ///
    /// ```text
    ///     fn forward(&self, addr: u64) -> u64 {
    ///         match self.relocate.forward_offset(addr) {
    ///             Ok(Some(to)) => to,
    ///             Ok(None)     => addr,   // identity: it did not move
    ///             Err(e)       => { /* log; fail the cycle */ addr }
    ///         }
    ///     }
    /// ```
```

The `Err` arm does exactly what the paragraph above it says must not be done, on
the path both texts agree is a use-after-free.

**And the sanctioned body is the only one available**, because
`barrier.rs:744` is

```rust
    fn forward(&self, addr: u64) -> u64;
```

— infallible. There is no error channel through which `forward_offset`'s `Err`
could be honoured. The barrier's slow path has one adjacent behaviour
(`barrier.rs:1199-1213`: a `forward` that answers `0` for a non-zero input is
logged as a forwarding-table defect and the stale address is kept), which is a
*softer* version of the same wrong choice and would not even fire here, because
the sanctioned body returns a non-zero `addr`.

This is a genuine trait-composition defect: the two modules reconciled their
*domains* on 2026-08-07 and did not reconcile their *failure semantics*.

**Fix.** `ZBarrierContext::forward` needs to return something that can say "do
not use this reference" — `Option<u64>`, or a two-variant enum — and
`load_barrier_slow` needs a defined behaviour for it (abort the cycle is the
honest one; there is no correct value to hand a mutator). Until then, delete the
`Err` arm from the code block and replace it with a `todo!()` plus a pointer to
this finding, so the next implementor cannot copy a body its own doc forbids.

### N5 — HIGH — `barrier.rs`'s header describes a `relocate.rs` that no longer exists

`barrier.rs:120-127`:

```
//! * **`relocate.rs` is the one module that must change, and it is not a
//!   documentation change.** *(Checked in source 2026-08-07, not assumed.)* It
//!   owns the only encoder and decoder, and **its API boundary is absolute
//!   machine addresses**: `ZRelocate::encode_to` takes an absolute to-space
//!   address, `ZRelocate::decode_to` returns one (`relocate.rs:1408`, `:1436`),
//!   and that module's own docs instruct the load barrier to obtain addresses
//!   through `ZRelocate::forward` / `forward_lookup`.
```

and `:137-140`:

```
//!   adapter that implements `ZBarrierContext::forward` over a `ZRelocate` must
//!   subtract the heap base from `decode_to`'s answer and add it before calling
//!   in. That adapter does not exist yet in `src`; whoever writes it owns this.
```

Four claims, all now false:

1. **"its API boundary is absolute machine addresses"** — `relocate.rs:105-113`
   is a two-row table; `forward_offset` (`:2203`) and `forward_lookup_offset`
   (`:2248`) are the offset-domain half, added specifically for the barrier.
2. **"`relocate.rs:1408`, `:1436`"** — `encode_to` is at `relocate.rs:1753` and
   `decode_to` at `:1799`. Line 1408 is the `pointer_map_reserve` field; line
   1436 is mid-paragraph in a doc comment.
3. **"that module's own docs instruct the load barrier to obtain addresses
   through `ZRelocate::forward` / `forward_lookup`"** — `relocate.rs:117-121`
   says "Earlier drafts of this file's docs sent the load barrier to
   [`ZRelocate::forward`]. **That was wrong** and is corrected on both methods",
   with the corrections at `:1794` and `:2016`.
4. **"That adapter does not exist yet in `src`; whoever writes it owns this"** —
   `relocate.rs:2172-2175` answers it directly: "This is the adapter that header
   says 'does not exist yet in `src`; whoever writes it owns this' — **it exists
   now**".

The same instruction is repeated on the trait method itself,
`barrier.rs:691-697` (inside `ZBarrierContext::forward`'s doc), so an implementor reading only the `forward` docs gets the
stale advice too. Following it — hand-rolling `decode_to` ± heap base — bypasses
`check_offset_domain` (`relocate.rs:2139`), which is the release-build guard
`relocate.rs` added for precisely this mistake.

**Fix.** Rewrite `barrier.rs:120-140` and `:625-631` to point at
`forward_offset` / `forward_lookup_offset`, and add the missing `mark.rs` bullet
from N3 while the section is open. Drop the two line numbers; `relocate.rs`'s
own methods are the stable reference.

### N6 — HIGH — `ZPageReal::age` has two writers with incompatible meanings

`page.rs:127-129` states what the field means:

```
/// G1 there is no Eden/Survivor/Old typing here, because ZGC's generational
/// split (JEP 439) is a per-page *age*, not a per-page kind. [`ZPageReal::age`]
/// carries that for a later agent.
```

`generation.rs` writes a **survival count**, `:1906-1907`:

```rust
            let age = page.age().saturating_add(1);
            page.set_age(age);
```

consumed by `ZPromotionPolicy::should_promote` (`generation.rs:577-579`) against
`Z_DEFAULT_PROMOTION_AGE = 3` (`:121`).

`tlab.rs` writes a **generation tag** into the same field, `:176-178`:

```rust
    /// The `ZPageReal::age` value a page carved for this generation carries.
    pub fn page_age(self) -> u32 {
        self as u32
    }
```

`ZTlabGeneration::{Young = 0, Old = 1}`, applied at `tlab.rs:889`:

```rust
                page.set_age(self.generation.page_age());
```

So an `Old` `ZTlab` stamps `age = 1` on a fresh page — one minor cycle's worth
of survival credit it never earned — and a `Young` `ZTlab` stamps `age = 0`,
resetting whatever count was there. If such a page ever entered
`ZYoungGeneration`'s map it would promote after two minor cycles instead of
three.

They do not collide *today* only by construction: `ZTlab` takes private pages
through `alloc_page` (`tlab.rs:888`) while `ZYoungGeneration::allocate`
registers only pages that came back from `alloc_object`
(`generation.rs:976-977`). That disjointness is itself a finding — **a
`ZGenerationalHeap` driven through `ZTlab` would register no pages in either
generation map and collect nothing.**

`tlab.rs:146-153` explains the divergence as a timing artifact — "Generational
ZGC (JEP 439) is being built in a sibling module that is **not on disk yet**" —
which is stale: `generation.rs` is 2 632 lines on disk and defines
`ZGeneration { Young, Old }` at `:172-178` with a byte-identical `as_str()`.

**Fix.** `tlab.rs` must stop writing `age`. If a TLAB needs to record which
generation carved a page, that is a second field on `ZPageReal` (or the
`ZGenerationalHeap` page maps, which already answer the question). Replace
`ZTlabGeneration` with `generation::ZGeneration` while you are there — the
premise for the local copy is gone.

### N7 — HIGH — `remembered.rs`'s shipped context violates its own trait contract

`remembered.rs:1103-1105`, written by the page-id widening pass:

```
    /// The page id is a `u64` and must be **the same id
    /// [`crate::zgc::page::ZPageReal::id`] reports** for that page. Widened
    /// from `u32` on 2026-08-07; see [`ZRememberedSet::page_id`].
```

`ZRangeGenerationContext::page_of`, eighty lines below at `remembered.rs:1191-1192`:

```rust
        let delta = addr - self.old_start;
        let page = delta / (self.old_page_size as u64);
```

That is a dense index from 0, derived from address arithmetic. `ZPageReal` ids
come from `next_page_id` (`page.rs:1075`), initialised to **1** (`page.rs:1258`)
and incremented in allocation order (`page.rs:1434-1435`) — not address order,
and never 0. The module's only shipped `ZGenerationContext` therefore cannot
satisfy the contract the module states.

It also contradicts `remembered.rs:293-294`, in the same doc block as the
contract sentence:

> "This was a `u32` while the page modules were in flight, on the guess that a
> page id would be a small dense index. **It is not.**"

— while the impl below manufactures precisely a small dense index.

Blast radius is bounded today: `adapters.rs` declines to use
`ZRangeGenerationContext` and ships `ZHeapGenerationContext`
(`adapters.rs:271`), which returns `page.id()` (`:339`). But
`ZRangeGenerationContext` is `pub`, is the module's advertised production shape
(N13), and is what every test in `remembered.rs` uses.

**Fix.** Either give `ZRangeGenerationContext` an id base/offset so it can map
its dense index onto real ids, or re-document it honestly as a test double and
point production callers at `adapters::ZHeapGenerationContext`.

### N8 — MED-HIGH — `zgc.rs` says nothing is wired; census is wired and running

`zgc.rs:98-101`:

```
// and be reviewed independently. NONE of them is wired into `ZgcRealHeap` yet —
// `ZgcRealHeap` is still the stop-the-world non-moving mark-sweep it has always
// been. Declaring them here compiles and unit-tests them under `--features
// zgc`; adopting them is a separate, later step.
```

Contradicted three ways in the same file:

* `ZgcRealHeap` **owns** a census: field `slot_census: census::ZSlotCensus`
  (`zgc.rs:1562`), constructed at `:1647`, exposed by `ZgcRealHeap::slot_census()`.
* `impl census::ZCensusHeapView for ZgcRealHeap` (`zgc.rs:2565`) — a 250-line
  implementation.
* And it **runs**, from inside `collect_garbage` (`zgc.rs:3535-3540`):

```rust
        if self.slot_census.is_enabled() {
            let _ = self.slot_census.run_walk(self);
```

`impl mark::ZMarkContext for ZgcRealHeap` (`zgc.rs:2837`) also exists; that one
really is inert, and `zgc.rs:2826` says so correctly.

This matters more than a stale comment usually would, because it is the first
thing anyone reads before touching these modules, and it is the sentence that
justifies "unit tests are enough".

**Fix.** Replace `:98-101` with the true status per module: census adopted and
executing behind a flag; mark implemented but unreached; the other ten
unadopted.

### N9 — MED-HIGH — `zgc_concurrent.rs` says the wiring step is undone; it is done

`zgc_concurrent.rs:92-93` opens with:

> "**Not** real yet, and this must not be overclaimed — stale ZGC docs are how
> this tree got into trouble the first time:"

and then, `:96-101`:

```
//! * **No [`ZMarkContext`](crate::zgc::mark::ZMarkContext) implementation for
//!   [`ZgcRealHeap`](crate::zgc::ZgcRealHeap) exists.** That implementation
//!   lives in `gc/src/zgc.rs` (which this module does not own) and is the
//!   wiring step; see "What the wiring step must provide" below. Until it
//!   lands, the only context in the tree is
//!   [`TestMarkContext`](crate::zgc::mark::TestMarkContext)
```

`zgc.rs:2837` is that implementation, and `zgc.rs:2806` records that it
landed: "before this existed nothing implemented `mark::ZMarkContext` for a real
heap, so both were unreachable code that compiled and marked nothing."
`zgc_concurrent.rs:151-155`'s "What the wiring step must provide" section
therefore describes completed work.

The rest of `:102-113` (the load barrier is not wired into field reads; the
restart loop is untaken) remains accurate.

**Fix.** Rewrite `:96-101` and `:151-155`. The file is right that stale docs are
the historical failure; it is now one of them.

### N10 — MEDIUM — the mutator hand-off exists twice, with opposite verdicts

|  | `mark.rs` | `barrier.rs` |
|---|---|---|
| queue | `ZMarkIngress` (`:808`) | `ZMarkQueue` (`:1533`) |
| shards | `Z_MARK_INGRESS_BUCKETS = 16` (`:231`) | `Z_MARK_SHARDS = 16` (`:1425`) |
| buffer cap | `Z_MARK_MUTATOR_BUFFER_CAPACITY = 256` (`:235`) | `Z_MARK_BUFFER_CAPACITY = 256` (`:1421`) |
| per-thread storage | `ZMarkMutatorBuffer`, caller-owned (`:974`) | `thread_local! { … Z_MARK_BUFFER … }` (`:1500`) |
| process globals | none | `:1429`, `:1436`, `:1441` |

Same two numbers, different names, incompatible designs — and the two modules
read the same historical incident and reached opposite conclusions, each
recording its conclusion as the tree's hard requirement.

`mark.rs:120-125`:

> "# No process-global state … There is no `static`, no `OnceLock`, no
> `thread_local!`. That is a hard requirement in this tree: process-global GC
> caches have already caused parallel-test crashes…"

`mark.rs:802-806` names the mechanism and the precedent: "a thread-local buffer
keyed to the *thread* rather than the *heap* is exactly that bug in miniature
(see `satb.rs`'s `thread_local_buffers_are_queue_scoped` regression test…)" —
which is real, at `gc/src/satb.rs:1199`.

`barrier.rs` built the thread-local anyway (`:1500-1510`), keyed it to the
queue, and wrote its own copy of that same regression test —
`barrier.rs:2719`, `fn thread_local_mark_buffers_are_queue_scoped()`.

Neither approach is obviously wrong. Having both, unreconciled, with the modules
that must compose disagreeing about whether the mechanism is permissible, is.

**Fix.** Pick one. `barrier.rs`'s is queue-scoped and has the orphan-parking that
`satb.rs` needed; `mark.rs`'s has no globals at all. Whichever survives, the
other's constants and its duplicate regression test should be deleted, not left
as a second answer.

### N11 — MEDIUM — two `object_size` methods, opposite meaning for `0`

`mark.rs:373-377`:

```
    /// Bytes to charge to live-set accounting for the object at `addr`.
    /// Default `0`, meaning "accounting disabled"; …
```

`relocate.rs:520-527`:

```
    /// It must return a value **below**
    /// [`ZRELOCATE_MIN_OBJECT_BYTES`] (conventionally `0`) for a header it
    /// believes is corrupt — that is the signal that stops the page walk
    /// instead of striding into garbage.
```

Identical name, identical signature (`fn object_size(&self, addr: u64) ->
usize`), opposite semantics for the same return value. `ZgcRealHeap` already
implements the first (`zgc.rs:3024-3029`) and returns `0` for `addr == 0`. When
the same type implements `ZRelocateContext` — the obvious next step — a
copy-pasted body turns "null address" into "corrupt header, abort the page
walk", or worse, silently disables the corruption tripwire.

**Fix.** Rename one. `relocate.rs`'s is the load-bearing one; `mark.rs`'s is dead
(see below) and could become `live_bytes_to_charge`.

Note also that `mark.rs::object_size` has **no consumer**: it is called nowhere
in `mark.rs`'s 3 300+ lines, and `ZMarkStats` (`mark.rs:433`) has no
live-bytes counter to charge it to. Its doc promises the value feeds "the
relocation-set chooser" — a job `page.rs` took over on 2026-08-07 (`page.rs:632`)
without anyone touching this method.

### N12 — MEDIUM — `generation.rs` assigns a job `remembered.rs` says it does not have

`generation.rs:421-425`:

> "So the contract is: **`iterate_old_to_young` yields the addresses of
> young-generation objects that are referenced from the old generation.** The
> card scan (or whatever the implementation uses) happens inside the
> remembered-set module, which legitimately owns old memory…"

`adapters.rs:367-372` quotes that sentence back and refutes it:

> "`remembered` states that the read is legitimately its own job — its
> `ZRememberedSetView`-facing rationale in `generation` says '…' — but it
> **exposes no API that performs it**."

And `remembered.rs:1017-1022` says "**Nothing here needs to change**", while
`adapters.rs:387-422` is a section titled "What `remembered.rs` must add for this
trait to be deletable", listing three decisions only `remembered.rs` can make.

`remembered.rs` implements `ZRememberedSetView` nowhere. `adapters.rs` bridged
the gap by declaring a *third* trait of its own, `ZOldSlotReader`
(`adapters.rs:423`), and shipping only a test double for it (`ZMapSlotReader`,
`:445`, documented at `:437` as "**a test double and a specification, not a
production reader**").

So the seam `generation.rs` says is closed is open, `generation.rs`'s sentence is
the reason nobody in `remembered.rs` built it, and the third-party trait that
papers over it has no production implementation.

The unassigned decision that matters most is `adapters.rs:411-418`: whether the
word a remembered-set scan yields is *coloured*. `generation.rs` documents its
roots as uncoloured machine addresses (`:78-81`, `:494`); a colored word carries
`Z_COLORED_TAG = 1 << 63` (`vaddr.rs:264`), so `scope.admits(addr as usize)`
(`generation.rs:1722`) silently drops it as an out-of-scope hint. A lost root is
a use-after-free, and the trait's own "entries are hints" clause
(`generation.rs:427-428`) legitimises the silence.

**Fix.** `generation.rs:421-425` must stop asserting a capability that does not
exist. Either `remembered.rs` grows `iterate_edges` (the shape `adapters.rs:392`
specifies) with a decided colour convention, or `generation.rs` documents the
read as unassigned and points at `adapters::ZOldSlotReader`.

### N13 — MEDIUM — "a genuine implementation, not a test double" vs "unusable against a real heap"

`remembered.rs:1116-1120`:

> "It is a genuine implementation, not a test double — it is the shape that fits
> `ZgcRealHeap`'s single-arena storage if that arena is split into a young
> sub-range and an old sub-range."

`adapters.rs:236-243`:

> "`page` does not lay the heap out that way: young and old pages are
> **interleaved in one granule pool** and either generation may own any page
> (`ZGenerationalHeap::new` says so explicitly — the young/old split is a byte
> *budget*, not a partition of the reservation). No pair of ranges and no stride
> can describe that, so the shipped context is unusable against a real heap."

`adapters.rs` is right: `generation.rs:1480-1482` does say the split is a policy
figure and not a partition. Two `pub` types, one of them presented to callers as
production-ready by its own module and as unusable by the seam module.

### N14 — MEDIUM — `census.rs`'s slot-width facts have drifted from narrow oops

`census.rs:239-242`, stated as an absolute:

```
    /// Element of a reference array: a bare 8-byte word at
    /// `obj + ARRAY_DATA_OFFSET + i*8`. `REF_ELEMENT_SIZE = 8`
    /// (`types/src/heap_types.rs:176`) — unconditionally, since the compact
    /// layout never applied to arrays.
```

The implementation says the opposite, in a comment written at the call site —
`zgc.rs:2676-2685`:

> "`ref_element_size()`, not the `REF_ELEMENT_SIZE` constant: the constant is
> the wide (8-byte) width, and the stride is 4 under narrow oops."

Same divergence for `CompactField` (`census.rs:231-233` "a bare 8-byte pointer"
vs `zgc.rs:2777` and the fork note at `zgc.rs:2356-2366`, which route through
`narrow_oop::read_ref_slot` explicitly to match narrow-oop loads), and the
module table at `census.rs:39` ("8 B always").

`census.rs:446-448` then pins an invariant that does not hold:

```
    /// two 16-byte-cell shapes, so that
    /// `slot_addr % 8 == 0` holds for all four (study §2.1).
```

Under narrow oops the array-element stride is 4 (`types/src/narrow_oop.rs:77-79`),
so `slot_addr` is only 4-aligned.

This is the classic drift shape: the census docs describe the world before
compressed oops landed; the only implementation describes the world after.

### N15 — MEDIUM — `census.rs`'s validity gate cannot fail

`census.rs:576-579` states the argument for the whole check:

> "Redundant with [`ZCensusObjectKind::CompactInstance`] **on purpose**:
> [`ZSlotCensus::run_walk`] cross-checks the two and counts every disagreement…
> Compactness is the single fact this whole census turns on, and an instrument
> that derives it twice and compares is an instrument that can be believed."

`census.rs:914` makes it a validity gate: "**Must be zero.** Any other value
means the two derivations of the single fact this census turns on do not agree,
and the run is void."

But the two derivations are one derivation, answered twice.
`for_each_live_object` (`zgc.rs:2574`) selects the kind with
`Self::effectively_compact_header(header)` (`zgc.rs:2181`); `is_compact`
(`zgc.rs:2627`) returns `header.kind() == ObjectKind::Object &&
Self::effectively_compact_header(header)`. `zgc.rs:2623` says so outright: "Both sides route through
[`ZgcRealHeap::effectively_compact_header`]".

`kind_disagreements` can be non-zero only if the header mutates between the two
calls. The census prints a green light it is structurally unable to withhold —
[`a-gate-that-measures-a-fraction-reads-as-good-news`] in a new costume.

**Fix.** Either derive the second answer independently (from the class layout
registry rather than from the header flag), or delete the counter and stop
claiming the census is self-validating. The second is honest and cheap.

### N16 — MEDIUM — `tlab.rs`'s reason #3 for existing is never exercised

`tlab.rs:33-34` and `:42-44`:

> "So this file adds only what is genuinely ZGC-specific and cannot live in the
> shared type: … 3. **The allocation color.** A freshly allocated object must be
> born with [`ZGoodMask::allocation_color`], or the load barrier's fast path
> rejects every new object forever."

`ZTlabHeapHooks::allocation_color` (`tlab.rs:257`) and `allocation_color_bit`
(`:286`) are declared, documented, and **called from nowhere** in the non-test
body. `ZTlab::alloc` (`:674-687`) returns a raw address and writes nothing; it
delegates to `self.inner.alloc(bytes, align)`, i.e. `alloc_initialized(size,
align, |_| {})` with the no-op initializer, while `crate::tlab::Tlab` takes an
`init: F` closure (`gc/src/tlab.rs:288`) for exactly this purpose. `next_hash`
(`tlab.rs:264`) is in the same state.

So the file's headline justification for existing separately from
`crate::tlab::Tlab` — three of its five reasons — is unimplemented, while its own
doc states the consequence of getting it wrong in absolute terms.

### N17 — MEDIUM — `footprint_of` bypasses the walker invariant

`tlab.rs:483-488`:

```rust
fn footprint_of(bytes: usize, align: usize) -> usize {
    let a = if align == 0 { ZPAGE_OBJECT_GRID } else { align };
    debug_assert!(a.is_power_of_two(), "alignment must be a power of two");
    (bytes.saturating_add(a - 1)) & !(a - 1)
}
```

`page.rs:741` clamps: `let bytes = bytes.max(ZPAGE_MIN_ALLOC);` where
`ZPAGE_MIN_ALLOC = HEADER_SIZE` (`page.rs:71`). `page.rs:66-70` says what that
buys:

> "Clamping here buys a walker invariant for free: two distinct object bases in a
> page are never closer together than a header, so a linear walk that reads a
> header at `p` can always read the *next* header without first proving it did
> not land inside the previous object."

`tlab.rs` references neither `ZPAGE_MIN_ALLOC` nor `HEADER_SIZE` in non-test code
(the only occurrence, `tlab.rs:1354`, is inside `#[cfg(test)]`), and neither
`ZTlab::alloc` (`:674-687`) nor `Tlab::alloc_initialized` clamps. A sub-header
request through a TLAB therefore places two object bases closer than a header,
inside a page a `walk_bounds()`-bounded walker will traverse — on exactly the
path TLABs exist to make hot.

Second-order: `tlab.rs:476-482` claims `footprint_of` "Mirrors
`Tlab::alloc_initialized`'s own `footprint` computation **exactly**, so the
accounting the hooks receive is the accounting the buffer performed." True of the
inner TLAB; false of the two paths that go to `ZPageAllocator::alloc_object`
(`tlab.rs:749`, `:797`), where the page *does* clamp. That under-reports to
`ZTlabHeapHooks::register_allocations`, whose contract (`tlab.rs:246-247`) is
"the size actually consumed from the page" and whose doc (`:240-243`) names
under-counting as the "re-arms too late (heap exhaustion)" failure.

### N18 — MEDIUM — `ZGenerationMarker` has no producer, and its default is the dangerous one

`generation.rs:75-76` says a sibling owns the tracer. `mark.rs` is that sibling,
and it does not implement `ZGenerationMarker` — the only impls are
`generation.rs`'s own test doubles (`:2044`, `:2058`, `:2103`) and the
integration test's `ScopedAddressMarker` (`zgc_module_integration.rs:906`).

The blocker is structural. `ZMarkReport` is per page id
(`generation.rs:460-466`):

```rust
    pub live_bytes_by_page: FxHashMap<u64, usize>,
```

`mark.rs` has no per-page accounting anywhere in 3 256 lines; its output is
`marked_sorted() -> Vec<u64>` (`mark.rs:2872`) and `marked_count()` (`:2879`).
And `generation.rs:462-463` defines the missing-key default:

> "Page id → live bytes found in that page. A page absent from this map held no
> live object and is reclaimable."

An adapter that produced an empty or partial report would make `sweep_young`
(`:1892-1909`) and `sweep_old` (`:1954-1970`) free every unreported page.
"Absent means dead" is the worst possible default for a trait with no
implementor.

Note that `page.rs` already ships the accumulation channel the marker would
naturally use — `ZPageReal::add_live_bytes` (`page.rs:563`, "Atomic so parallel
markers can credit the same page without a lock") — and `generation.rs` instead
demands a `FxHashMap` snapshot and then throws it away with `page.set_live_bytes`
(`:1898`, `:1960`). Two mechanisms for one fact, and only the unused one is on
`page.rs`.

### N19 — MEDIUM — the generational occupancy numerator disagrees with the allocator's budget

`generation.rs:1054-1057` sums bump cursors:

```rust
    pub fn used(&self) -> usize {
        let map = self.pages.lock();
        map.values().map(|p| p.used()).sum()
    }
```

and `:1484-1487` sets both capacities as fractions of `allocator.max_capacity()`.
But `ZPageAllocator` enforces its budget on **committed page spans**
(`page.rs:1393`, `:1399`):

```rust
        if state.committed + page_size > self.max_capacity {
            return Err(ZPageError::OutOfCapacity { … });
```

A 32 MiB Medium page holding one 4 MiB object contributes 4 MiB to
`young_used` and 32 MiB to `state.committed`. `page.rs:657-668` is explicit that
sparsely-filled retired pages are "not an unreachable corner". So
`should_collect` can report occupancy 0.125 at the moment `alloc_page` returns
`OutOfCapacity` — the trigger under-reports pressure by up to 8× and the poll
path can never fire ahead of allocation failure.

A related, softer note: `generation.rs:652-665` defines `young_occupancy` /
`old_occupancy` as `used/budget`, while `page.rs:577-579` calls `live_ratio`
(`live/used`) "this module's **only** occupancy figure". Those are two different
questions at two different granularities and both are legitimate, but the word is
now overloaded across the seam and `page.rs`'s sentence reads as exclusive.

### N20 — MED-LOW — `adapters.rs`'s conversions are duplicated in the test binary

`adapters.rs` exists to be "those conversions, moved into `src` so there is
exactly one of each" (`:16-18`). There are two of each.

| conversion | `src` | test binary |
|---|---|---|
| size-class enum ↔ `u8` | `adapters.rs:121` | `zgc_module_integration.rs:111` |
| `ZGenerationalHeap` → `ZGenerationContext` | `adapters.rs:271` | `:1215` |
| `ZRememberedSetTable` → `ZRememberedSetView` | `adapters.rs:543` | `:1264` |

The test file **never imports `adapters`** (its use list is
`zgc_module_integration.rs:63-70`), so `adapters.rs` has no test outside its own
`#[cfg(test)]` module — and the seam layer is precisely the thing an *integration*
test exists to exercise from outside the crate.

Its copies also still assert that `src` has none.
`zgc_module_integration.rs:104-110`:

> "**This function is a finding, not a convenience.** … Nothing in `src`
> converts between them, so every future caller will hand-roll this mapping…"

The two copies have already diverged in behaviour: `adapters.rs:637-647` counts
an unresolved page base and continues; `zgc_module_integration.rs:1281-1286`
`panic!`s. `adapters.rs:654` drops a null-reading slot silently;
`:1289-1291` does the same by omission. Neither is wrong; they are two answers to
a question `adapters.rs:523-542` says must be counted rather than panicked.

Also stale: the test's module header (`:9-10`) still says "The **ten** modules"
and lists ten — `census` and `adapters` are missing.

### N21 — MED-LOW — the integration fixture no longer tests the trait default

`zgc_module_integration.rs:164-181` implements `heal_color` **unconditionally**:

```rust
    fn heal_color(&self) -> u64 {
        if self.tag_healed_words {
            vaddr::Z_COLORED_TAG | self.mask.good()
        } else {
            self.mask.good()
        }
    }
```

So `VaddrBarrierContext::new(false)` does not "use the trait default" as its call
site claims (`:505`, `// use the trait default`) — it selects a *fabricated*
broken heal color that no longer exists anywhere in `src`. The trait's real
default (`barrier.rs:603`) is exercised by no test in this file.

Finding 6 is pinned only in-module, by `the_default_heal_color_carries_vaddrs_tag_bit`
(`barrier.rs:2088`) over a `DefaultsOnlyContext` (`barrier.rs:2045`). That is a
good pin. But the whole argument for the integration binary
(`zgc_module_integration.rs:34-39`: "an integration test binary links the crate
from *outside* and sees only its **public** API… any seam that cannot be
assembled here is a seam a future `ZgcRealHeap` cannot assemble either") applies
with full force here, and the fixture defeats it.

### N22–N28 — the LOW band

**N22.** `metrics.rs:173-175` — "The TSV column count is `14 + 4 *
ZGC_PHASE_COUNT`, so this constant is the one place that number is stated —
**never write `58` (or `54`) anywhere**" — is violated by `metrics.rs:99-100`
("the TSV grows from 54 to **58** columns") and `:1173-1174` ("54 → 58
columns"). `metrics.rs:182-183`'s "64 samples per phase is 5 KB for the whole
struct" was computed at 10 phases (`RecentRing` is `[u64; 64]` + 2 × `usize` =
528 B; × 11 = 5 808 B), and `:137`'s "taken ~10 times per cycle" is the same
stale 10. The `PAUSE_HISTORY_CAP` comparison at `:182` is loose in the other
direction: `g1.rs:1238` is `1 << 16`, three orders of magnitude from 64.

**N23.** `metrics.rs:1460-1463` claims "Adding a variant over there without
deciding where its nanoseconds land is now a **compile error here**". The only
mapping between `zgc::ZgcPhase` (`zgc.rs:669`) and `metrics::ZgcPhase`
(`metrics.rs:205`) is `fn counter_for` at `metrics.rs:1471-1488`, a private
helper inside `#[cfg(test)] mod tests`. The gate binds `cargo test`, not `cargo
build`. Relatedly, nothing in `gc/src/` constructs `ZgcMetrics` at all — the
sole production reference is the TODO at `zgc.rs:3539` — so `ZgcPhaseGuard`,
`scoped_allocation_stall` and `record_cycle` are dead from `src`'s perspective,
which the module header does not say.

**N24.** `census.rs:156-161` ends "The two orderings that are *not* relaxed do
not exist: there are none." There are exactly two, forming a deliberate
release/acquire pair with its own justifying comment: `census.rs:1377`
(`fetch_add(1, Ordering::Release)`) and `:1452` (`load(Ordering::Acquire)`).

**N25.** Five modules still describe landed siblings as unlanded:
`remembered.rs:281-283` ("they are being written in parallel"), `:994-996`,
`:1119-1120` ("the in-flight page modules"); `relocate.rs:509-514` ("the
generational policy are all being written in parallel with this module");
`tlab.rs:146-153` ("a sibling module that is not on disk yet"). Each of these is
load-bearing prose: it is the stated justification for a local duplicate type
(N6) or a local trait.

**N26.** Six dead anchors. `barrier.rs:124` → `relocate.rs:1408`/`:1436`
(actual: `:1753`/`:1799`). `remembered.rs:288-290` → `page.rs:973`/`:1156`
(actual: `:1075`/`:1258`) — in a note whose rhetorical point is that it verified
those sites. `census.rs:123-126` cites `gc/src/heap.rs:1687-1714` for
`read_prim_element`'s plausibility degrade being "the top-priority fix"; that
range now documents the **repair** (`heap.rs:1693`: "A ZGC COLORED WORD IS NOT
CORRUPTION, and must never take the degrade") and `read_prim_element` moved to
`heap.rs:1828`. `census.rs:522-525` → `heap.rs:502` for the zero-fill (actual:
`:507`). `census.rs:49`/`:77` → `classloading/src/class.rs:1624` for a refusal
(actual: `:1625-1626`; `:1624` is `// stay compact.`). `tlab.rs:115` and
`gc/src/tlab.rs:388-389` both cite `ThreadRegistry::tlab_addr`, which does not
exist under that name anywhere in the tree.

**N27.** `vaddr.rs:74-79` describes the ZGC heap as "[`crate::arena::Arena`]-backed
**owned** memory", and `:877-883` builds a rebasing argument on arenas being
reallocated when the heap grows — offering that as benefit #2 of the offset
encoding (`:95-101`, "heap growth is a base-pointer update instead of a
re-mapping"). `page.rs:1177-1179` reserves one `Vec<u8>` that "is allocated once
and never resized", and `:1240` is `vec![0u8; max_capacity + granule]`. The
encoding is still right; one of its two stated benefits is for a growth mode
`page.rs` forecloses, and a reader reconciling the two files has to work that
out unaided.

**N28.** `tlab.rs:1242-1245` claims blocking on a peer's cell mutex "is what
replaces an OS-level thread-suspension handshake. When this returns, no
registered buffer holds a live chunk". The loop at `:1255-1265` drops each
guard at the end of its iteration, so a peer retired at iteration *i* may refill
immediately. `tlab.rs:112-118`, in the same doc comment, concedes the real
requirement needs "the vm-crate thread registry and an exclusion protocol this
module cannot establish on its own". The file's own post-condition
`debug_assert!` at `:1272-1276` ("`retire_all` left a reserved tail") is
therefore reachable by any caller who believes `:1242-1245`.

### Documented divergences that are *not* findings

Recorded so a future audit does not re-open them:

* **`barrier.rs` does not consume `vaddr::ZGoodMask::weak_bad`.** A weak load
  tests the ordinary bad mask and merely suppresses `mark_live`
  (`barrier.rs:808-825`). This is a performance and heal-churn divergence from
  OpenJDK, not a correctness one, and it is stated as such with the wiring cost
  named. Correct as documented.
* **`forwarding.rs`'s occupancy tag is the same bit as `vaddr::Z_COLORED_TAG`.**
  Deliberate, argued at `forwarding.rs:343-378` (INVARIANT ZFWD-1 on `ZFWD_OCCUPIED_BIT`,
  `:379`), and pinned by a `const _` assertion at `:503` whose message tells a future editor to re-read
  INVARIANT ZFWD-1 before changing it. This is the best-executed cross-module
  coupling in the subsystem.
* **`census.rs` and `metrics.rs` both emit a TSV.** Documented and reconciled
  rather than drifted (`census.rs:1666-1674` explains the `String` vs
  `&'static str` divergence and its reason; the two `set_run_label` bodies are
  equivalent).
* **The shared layout constants are clean.** No production line in any of the
  twelve modules hard-codes `HEADER_SIZE`, `SLOT_SIZE`, `REF_FIELD_SIZE`,
  `REF_ELEMENT_SIZE` or `ARRAY_DATA_OFFSET`. `page.rs:51` and `relocate.rs:280`
  take them via `crate::heap::HEADER_SIZE`, which is a `pub use` of
  `cratonvm_types` (`gc/src/heap.rs:56-61`); `forwarding.rs:756-759` and
  `remembered.rs:207-218` derive them in `const` context. The one stray literal
  is `mark.rs:2926`'s `16`, inside a `#[cfg(test)]` `object_size` double.

---

## 3. The three `#[ignore]`d tests

All three name gaps that have since been closed. None should stay as it is.

### 3a. `barrier_default_heal_color_produces_a_word_vaddr_rejects` (`:502-519`) — **stale; the gap is fixed**

Its own doc states the exit condition (`:499-501`): "either
`ZBarrierContext::heal_color`'s default becomes `Z_COLORED_TAG | good_mask()`,
or the requirement to override it is written into the trait docs. **Un-ignore
once one of those lands.**" Both landed — `barrier.rs:603` (the default) and `barrier.rs:521` (the trait
contract clause).

But un-ignoring it as written does not test what it says, because the fixture
overrides `heal_color` (N21). The test would exercise a fabricated broken
context, not the trait.

**Recommendation: rewrite, then enable.** Delete `tag_healed_words`
(`zgc_module_integration.rs:143`, `:164-181`) so `VaddrBarrierContext` inherits
the default, rename the test to the positive form
(`the_default_heal_color_produces_a_word_vaddr_accepts`), and keep the
`is_well_formed(healed)` assertion — which will then pass and become the
cross-crate pin that finding 6 does not have. The negative half already exists
in-module at `barrier.rs:2012`
(`an_untagged_heal_color_produces_a_word_vaddr_rejects`).

### 3b. `barrier_default_address_mask_covers_the_address_space_vaddr_declares` (`:536-548`) — **a category error; delete it**

The body is a single assertion:

```rust
    assert!(
        vaddr::Z_OFFSET_MASK >= vaddr::Z_MAX_ADDRESS,
```

This is the test the brief flagged, and `barrier.rs:106` names it as a
category error in as many words:

> "So `Z_OFFSET_MASK >= Z_MAX_ADDRESS` is not a property that should hold; the
> two constants are bounds on two different domains, and asserting one against
> the other **is the category error, not the finding**."

`Z_OFFSET_MASK` bounds what a *slot* holds (`vaddr.rs:234`); `Z_MAX_ADDRESS`
bounds where the *reservation may be placed* (`vaddr.rs:295`, and it is enforced
at `vaddr.rs:914-917`). The assertion can never hold and would be wrong if it
did — a 47-bit address mask would overlap `Z_METADATA_MASK` and
`destination | heal_color` would corrupt the color, which
`load_barrier_slow`'s own overlap assert (`barrier.rs:1265`) exists to catch.

The underlying question — "which domain do slots hold?" — was answered on
2026-08-07 (`barrier.rs:65-107`), and the answer is enforced at runtime by
`is_bare_offset` (`barrier.rs:1099`, called at `:1226` in release builds).

**Recommendation: delete.** If a replacement is wanted, the property worth
pinning is the one the decision actually rests on:
`Z_METADATA_SHIFT == Z_OFFSET_BITS`, i.e. that there is no room in the encoding
for a wider address mask. That is one line and it is true.

### 3c. `forwarding_to_field_holds_a_real_zpage_heap_address` (`:831-848`) — **stale; the gap is fixed**

It asserts `addr <= forwarding::ZFWD_MAX_PAYLOAD` for a raw address out of
`ZPageAllocator::alloc_object`. That was the right alarm when destinations were
absolute. They are not: `ZRelocate::encode_to` (`relocate.rs:1753`) stores
`to_absolute - heap_base + ZRELOCATE_ENCODING_BIAS`, and
`forwarding.rs:466-470` now documents `ZFWD_MAX_PAYLOAD` as "a bound on the
*encoded* value, which under the production encoding is a heap-relative
offset — **not** a bound on any absolute address."

Left enabled it would pass on Windows and fail on Linux, for a reason that is no
longer a defect. Left ignored it keeps a false claim alive: `adapters.rs:65-67`
still cites this test as evidence that "the 42-bit offset field is already known
to be too narrow for a real Linux machine address."

**Recommendation: replace.** The property that now needs a cross-module pin is
that `ZRelocate` refuses an unencodable configuration rather than shipping a
relocator that cannot forward — `relocate.rs:1554-1600`'s `to_encoding_base`
check. `relocate.rs` pins it in-module
(`linux_shaped_absolute_destination_is_unencodable`); an integration-level
version over a real `ZPageAllocator` is the honest successor to this test.
Update `adapters.rs:65-67` when it goes.

The healthy sibling next to it, `forwarding_to_field_covers_vaddrs_whole_offset_space`
(`:854-871`), is the correct form of the same question and should stay.

---

## 4. What is NOT verified

Everything above is a source read. Nothing here has been run. Specifically:

* **No compiler was invoked.** The `E0308` in N1 is derived from two concrete
  signatures (`remembered.rs:801`, `:828`) and Rust's lack of implicit integer
  widening. It is a strong inference, not an observation.
* **These modules are not adopted by `ZgcRealHeap`**, with the two exceptions in
  N8. The runtime behaviour of the ZGC backend is unchanged: `collect_garbage`
  (`zgc.rs:3317`) is still the single-threaded stop-the-world non-moving
  mark-sweep it has always been. **No finding in this document can currently
  cause a wrong answer in a running VM.** They are all latent.
* **The unit tests that do run cannot see any of this.** CI executes 265 tests
  across `gc/src/zgc/*.rs` plus 91 in `zgc.rs` and 12 in `zgc_concurrent.rs`
  (`ci.yml:470-473`). Every one of them is a module testing itself against its
  own trait doubles.

### What a real run would have caught that unit tests cannot

This is the part worth being precise about, because it is the whole argument for
prioritising adoption over more auditing.

| Finding | Why no unit test can see it | What a run catches it with |
|---|---|---|
| **N3** (`mark_live` domain) | Both sides are internally consistent. `barrier.rs`'s tests use a `HashMap` forwarding double; `mark.rs`'s use `TestMarkContext` with synthetic addresses. Neither ever holds a real heap base. | The first barrier-driven concurrent mark: `off_heap_children` (`mark.rs:459`) climbs to equal the barrier's `marks_enqueued`, and live objects are swept. |
| **N4** (Err fallback) | `forward_offset`'s `Err` path is tested for the error *value*; nothing tests what a barrier does with it, because no barrier consumes it. | A relocation that fails mid-cycle, on a quarantined page — i.e. only under real concurrent relocation, which `relocate.rs:158-170` says must not be enabled yet. |
| **N6** (`page.age` collision) | The two writers live in modules that never link in the same test. | A `ZGenerationalHeap` driven through `ZTlab`: promotion fires a cycle early, or never, because neither generation map holds the TLAB's private pages. |
| **N7** (dense page ids) | `remembered.rs`'s tests supply their own ids and get back what they put in. | The first `ZRangeGenerationContext` over a real `ZPageAllocator`: every remembered bit lands on the wrong page's set, so every old→young edge is lost. |
| **N17** (missing min-alloc clamp) | Requires an allocation smaller than a header *and* a linear page walk over the result. `tlab.rs`'s tests do neither. | A heap walk after a TLAB-served sub-header allocation — the Bug-D shape `tlab.rs:19` already names. |
| **N18** (no `ZMarkReport` producer) | The trait's doubles always produce a complete map. | The first real minor cycle: an incomplete map frees every unreported page. |
| **N19** (occupancy vs committed) | `generation.rs`'s tests size the heap so the two agree. | Any workload that retires partially-filled Medium pages — `page.rs:657-668` says that is the normal case, not a corner. |

Six of these seven are lost-root or freed-live-object bugs. That is the
signature of this subsystem's risk: **the modules are individually careful and
collectively unproven, and every unproven seam fails in the direction of
reclaiming something live.**

---

## 5. Systemic risk

Three things the parallel-authoring process keeps producing. Each shows up more
than once above, which is what makes them process problems rather than bugs.

**1. Every module documents its siblings, and the documentation outlives the
sibling.** N5, N8, N9, N12, N13, N25 and half of N26 are all the same defect:
module A states a fact about module B, B changes, and nothing links the two. The
subsystem's docs are excellent — genuinely among the best in this tree — and
that is precisely what makes this dangerous, because they read as authoritative.
`barrier.rs:120-127` even carries "*(Checked in source 2026-08-07, not
assumed.)*" on a paragraph that is now wrong in four places. A verified claim
with a date is *more* believable than an unverified one, and decays just as fast.

*What would catch it earlier:* the claims that matter are already
machine-checkable. `barrier.rs:124`'s "`ZRelocate::decode_to` returns an absolute
machine address" could be a `const _: () = assert!()` on a type, or a doc-test.
The tree already has the pattern — `forwarding.rs:494-503` pins the tag
collision at compile time with a message that tells the editor what to re-read.
Ten more of those would have prevented most of §2.

**2. The reconciliation pass fixes the module and misses the mirror.** N1 is the
purest instance: the same agent widened `HeapGenerationContext::page_of`
(`zgc_module_integration.rs:1229`) and left `:1950` in the same file. N7 is the
same shape one level up — the pass wrote the contract sentence
(`remembered.rs:1103-1105`) and did not check the module's own only impl eighty
lines below. N21 is a third: the barrier fix landed and the fixture that was
supposed to prove it was left inverted.

*What would catch it earlier:* a reconciliation pass should be required to grep
for its own symbol across the whole tree, not just the module it was assigned.
`register_old_page` has five call sites; the pass touched three.

**3. Nothing measures the seams, and the thing that was supposed to is not
built.** N2 is the root cause of the other two. The integration suite is the only
artefact in this subsystem that can fail when two modules disagree, and it has
been unbuildable and uncompiled. Meanwhile 368 unit tests pass, CI is green, and
the modules' self-reports say "reconciled".

This is the `census.rs` incident again — `ci.yml:479-480`: "which is how
`census.rs`'s 24 tests sat unexecuted until it was declared" — written into the
CI file by someone who had just fixed the previous instance, one job step away
from the next one.

*What would catch it earlier:* CI must build every target of every
feature-gated crate, not every target of the two crates someone remembered. The
narrow fix is one line; the general fix is a rule that a `--features X` job
either uses `--all-targets` or explains in a comment which targets it is
skipping and why.

**A fourth, smaller pattern worth naming:** three modules — `mark.rs`,
`census.rs`, `generation.rs` — carry no dated reconciliation note at all, and
they account for N3, N11, N12, N14, N15, N18 and N19. They were not skipped
because they were clean; they were skipped because nothing pointed at them. The
ten findings the suite reported became the eight passes' work list, and the
modules the suite could not reach were the modules nobody reconciled.

---

## 6. Confidence, and the one thing to do next

**Confidence in the twelve modules, individually: high.** These are unusually
well-built files. The reasoning is written down, the rejected alternatives are
written down with their reasons, the compile-time assertions are placed where
they will fire on the right person, and the corrections are dated and explained
rather than silently applied. `forwarding.rs`'s payload newtype, `page.rs`'s
denominator argument, `barrier.rs`'s heal-color derivation and `relocate.rs`'s
two-domain table are each better than what they replaced by a wide margin.

**Confidence in the subsystem as a composition: low, and lower than it was
before this audit.** Twenty-eight new findings from a subsystem that had just
had eight reconciliation passes is not a reassuring number, and the four HIGH
ones (N3, N4, N5, N6) plus the two CRITICAL ones are all *between* modules — the
exact place the eight passes were supposed to have swept. Six of the seven
seams in §4's table fail toward freeing something live.

**Confidence that this audit found everything: moderate at best.** I read source
and ran no compiler. The seams I could evaluate are the ones two modules both
document; a seam neither module mentions is invisible to this method, and N3 —
which is the most dangerous item here — was found only because `barrier.rs`
happened to enumerate four sibling modules and I noticed the fifth was missing.

### The single thing to do next

**Make `gc/tests/zgc_module_integration.rs` compile and put it in CI.**

Concretely: fix `:1950` and `:1961` (N1), then change `.github/workflows/ci.yml:504`
from

```yaml
        run: cargo test -p cratonvm-gc --lib --features zgc
```

to drop `--lib`.

That is two source edits and one word deleted from a YAML file, and it is worth
more than any other item in this document, because:

* it restores the only artefact in the subsystem that can fail when two modules
  disagree;
* it makes N1 impossible to repeat — the class of defect that produced roughly
  half of §2;
* it converts the three `#[ignore]`d tests from folklore into decisions someone
  can actually make; and
* every other finding here is either latent (nothing is adopted) or a doc fix,
  whereas *this* is the thing that will catch the next round of contradictions
  before an audit has to.

Do it before the next parallel wave, not after. The next wave will produce a
fresh crop of these — that is what the process does — and the only question is
whether anything is watching when it does.
