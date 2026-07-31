# JDK-only mode — open wave-2 work list

**Status:** OPEN. Filed 2026-07-31 from wave-1 implementation findings.

Normative contract: [`docs/feature-designs/jdk-only-mode.md`](../../feature-designs/jdk-only-mode.md)
(owned by the orchestrator; do not edit). Wave 1 is **measurement, not
deletion** (contract §10). Everything in this directory is a gap wave 1
deliberately deferred rather than papered over, with the evidence that makes it
actionable.

Related non-known-issue docs: [`docs/jdk-only-runtime-services.md`](../../jdk-only-runtime-services.md),
[`docs/jdk-only-audit.md`](../../jdk-only-audit.md),
[`docs/jdk-only-native-review.md`](../../jdk-only-native-review.md),
[`docs/jdk-only-migration.md`](../../jdk-only-migration.md).

---

## ⚠ Wave-1 revert — read before trusting a `file:line` citation

On 2026-07-31, between the verification pass for these records and their being
written, the **uncommitted wave-1 edits were reverted out of the shared working
tree** at `C:\craton\cratonvm` (branch `dev`, HEAD `0c54a9184`). Tracked files
returned to their committed state; untracked new files (`types/src/compat.rs`,
`classloading/src/class_origin.rs`, the `jdk_only_*` tests, `difftest/src/census.rs`,
the `docs/jdk-only-*.md` set) survived and are now orphaned — their `pub mod`
declarations went with the revert.

Specifically lost from the working tree: the wave-1 edits to
`types/src/{error,lib,flag_groups}.rs`, `native-api/src/{registry,lib}.rs`,
`classloading/src/{class,class_manager,lib}.rs`, `vm/src/config.rs`,
`vm/src/vm/{vm_exec,vm_init}.rs`, `vm/src/runtime/interpreter/invoke.rs`,
`vm-cli/src/main.rs`, `libcratonvm/src/lib.rs`, `cratonvm-embed/src/lib.rs`,
`native-builtins/tests/stub_ratchet.rs` and the `difftest` crate. Work that
survived (files still modified at the time of writing): `jit/src/lib.rs`,
`jit-api/src/lib.rs`, `vm/src/native/jni.rs`, `vm/src/runtime/interpreter.rs`,
`vm/src/vm.rs`, `vm/src/vm/{vm_object,vm_util}.rs`, and the `docs/book` set.

**What this means for these records:**

* Every record states its own evidence provenance. Findings verified against
  **pre-existing** code (the duplicated dispatch lists, the ambient
  `NativeKind`, the object-layout drift, `CachedInvokeTarget`, the JIT items)
  are re-verifiable in the tree today and are unaffected.
* Findings whose evidence was the **wave-1 code itself** (`ensure_synthetic_class`'s
  refusal path, `synthetic_name_origin`, `real_declaring_method`,
  `requested_by`, `--trace-jdk-only`) quote code that is no longer present.
  They are still accurate descriptions of what wave 1 built and why it stopped
  where it did — re-landing wave 1 is a prerequisite for acting on them.
* Line numbers everywhere are approximate and were taken on 2026-07-31 against
  HEAD `0c54a9184` with uncommitted work in flight. **Anchor on the function
  name and the quoted code, not the number.**
* `dev` HEAD has since advanced past `0c54a9184` via background `save 31-07`
  auto-commits, which sweep untracked files into commits. Those commits are
  purely additive — the reverted wave-1 edits were never committed and are
  therefore **not recoverable from git history**. Only the untracked new files
  survived.

---

## Ranked work list

Ranked by *danger*, not by effort. The first tier causes **silent wrong
behaviour** — no exception, no log line, no failing test.

### Tier 1 — silent misbehaviour

| # | Record | Why it is dangerous |
|---|---|---|
| 1 | [`NativeKind` is ambient and defaults to `SyntheticStub`](native-kind-is-ambient-and-defaults-to-syntheticstub.md) | `register()` takes no kind; it is inherited from a mutable registry field defaulting to `SyntheticStub`. A genuine bridge registered outside a `with_category` scope is silently classified a stub — and under `CRATONVM_NO_STUBS` / `--jdk-only` is then **dropped**. Already caused one boot regression (2026-07-14, `java.util.Properties`). Likely mechanical root cause of mis-tagged bridges inside the 157 baseline. |
| 2 | [Fabricated object layouts leak into native code](fabricated-object-layouts-leak-into-native-code.md) | Index-based field access against assumed synthetic layouts. On real bytes the index still resolves and points at a different field. `StringJoiner.add()` silently no-ops; `EnumSet.of()` returns an object with a null iterator. Two `breaks-under-strict` and two `unknown` sites are marked; three whole crates were never swept. |
| 3 | [The forced-native `String` policy exists twice, in opposite forms](forced-native-string-policy-two-lists-that-disagree.md) | A 21-name positive list (cold path) versus a 7-pair exclusion (warm path), each asking to be hand-synced with the other. The disagreement has already made a landed, measured h2-bnf performance fix into **statically unreachable code**. |
| 4 | [`ensure_synthetic_class` cannot enforce policy, only record it](ensure-synthetic-class-cannot-enforce-only-record.md) | Returns a bare `ClassId`, so under `--jdk-only` it records the violation and fabricates anyway, across 64 live call sites in 28 files. Strict boot *silently loses* `Enumeration$Impl` / `Comparator$Native` / the unmodifiable-view carriers instead of failing. The `load_class` chain does enforce; this API does not. |
| 5 | [VM-internal classes are mislabelled `CompatibilityStub`](vm-internal-classes-mislabelled-compatibility-stub.md) | `AnonymousObject$N` and `Proxy$Instance` are stamped `CompatibilityStub` to avoid flipping the derived `is_synthetic_stub` bool that ~160 read sites across 17 files depend on. Correct deferral — but it makes contract §11's zero-stub criterion unachievable by construction, and two of those read sites gate native-vs-bytecode dispatch. |
| 6 | [Cached invoke targets drop the `NativeKind`](cached-invoke-targets-drop-the-nativekind.md) | `CachedInvokeTarget::{Native,VirtualNative}` store a callback and no kind, so a cache *hit* cannot re-apply the policy. The hit path re-derives it by name — but only for classes on a hard-coded allow-list; everything else is served unchecked. Cached dispatches are also uncounted, making §11's "zero synthetic-stub invocations through **any** path" unverifiable on the hot path. **The JIT has the same hole twice** (see item 10 §1). |
| 7 | [The real-protected-stub allow-lists diverge](real-protected-stub-allowlists-diverge.md) | Two copies, 11 classes vs 10: one includes `java/util/StringJoiner`, the other deliberately omits it with a documented heap-corruption reason — and the including copy carries no comment saying so. Wave 2 must **reconcile**, not merge; both naive directions reintroduce a known defect. |
| 8 | [The `ThreadPoolExecutor.execute` receiver-shape case is copied eight times](threadpoolexecutor-execute-receiver-shape-special-case-copies.md) | Wave 1's markers name four. There are **eight** dispatch sites in the `vm` crate plus one unconditional `force_native` arm they all exist to override. A mechanical "delete every marked site" sweep leaves half the duplication enforcing a policy the other half no longer applies. |

### Tier 2 — the instruments the tier-1 items must be measured with

| # | Record | Why it matters |
|---|---|---|
| 9 | [The observability surface has three unfilled holes](observability-surface-has-three-unfilled-holes.md) | (a) `real_declaring_method` is `null` on every census row — the field that decides bridge-vs-shadowing-stub, needing a **non-initiating** image probe; (b) `ClassOriginEntry::requested_by` is `null` — the requester is known to the interpreter, not `ClassManager`; (c) `--trace-jdk-only` polls two append-only logs after `Vm::new` and at shutdown, so mid-run class-origin violations surface late. None causes wrong behaviour; all three are why tier-1 items say "needs runtime evidence from the census". |

### Cross-cutting

| # | Record | Contents |
|---|---|---|
| 10 | [Additional wave-2 markers not in the original inventory](additional-wave2-markers-not-in-the-original-inventory.md) | 13 further findings, including: the JIT's MIC slots have the same missing-`NativeKind` hole (twice); `JIT_COMPATIBILITY_MODE` and `JNI_NATIVE_METHODS` are process globals contract §2 forbids; five JIT-reachable dispatch paths bypass `resolve_dispatch` and are uncounted; seven JIT "thin direct call" ladders bake native reimplementations into emitted code (two of them `String` methods — making that policy's copy count **three**); the interpreter substitutes a *different class's* native for unresolvable interface calls; `check_override`'s exception chain is ~2,600 lines; and three load-bearing code comments cite known-issue doc paths that no longer resolve. |

---

## Dependency order for wave 2

The items are not independent. The order that avoids doing work twice:

1. **Re-land wave 1.** Several records describe APIs (`try_ensure_synthetic_class`,
   `ensure_generated_class`, `ClassOrigin`, the schema-2 census, `resolve_dispatch`)
   that are currently orphaned or absent — see the revert note above.
2. **Item 9** — build the instruments. In particular the non-initiating
   `real_declaring_method` probe, without which item 1's 157-entry
   reclassification is guesswork.
3. **Item 1** — make every native's kind an explicit, per-registration fact,
   with `registered_by` provenance captured first so "chosen" and "inherited"
   can be told apart. Nothing else in tier 1 can be done safely before this:
   items 3, 6, 7, 8 and item 10 §4/§11 all end with "let `resolve_dispatch`
   decide from `NativeKind` + `Method::code()`", which requires the kinds to be
   true.
4. **Items 6 and 10 §1 together** — store the kind (and a `NativeMethodId`) in
   both the interpreter's invoke cache and the JIT's MIC slots. Doing one alone
   buys nothing.
5. **Items 3, 7, 8, and item 10 §4/§8/§9/§11** — delete the hard-coded lists,
   each with its own regression corpus.
6. **Items 4 and 5** — migrate the `ensure_synthetic_class` callers and correct
   the VM-internal origins, then delete `is_synthetic_stub`.
7. **Item 2** — finish the layout sweep across `native-builtins`,
   `native-collections`, `native-io` and `vm/src/native/`. Independent of the
   rest and can run in parallel, but it is the item most likely to surface new
   blockers.

## Standing constraints for anyone working this list

* `native-builtins/tests/stub_ratchet.rs` asserts `BASELINE_SYNTHETIC_STUBS =
  157` **exactly**, with `SLACK = 0`, and separately asserts only
  `total >= 8_000` as a vacuity floor. The floor is not a claim about the exact
  total — do not cite one.
* `Compatible` mode must remain byte-for-byte unchanged (contract §5, §10). Most
  of the dangerous mistakes catalogued here are `Compatible`-mode behaviour
  changes made while intending to fix strict mode.
* No process globals for this feature's state (contract §2). Two of the items in
  this directory are existing violations; do not add a third.
* `docs/known-issues/` holds **unfixed** issues only. A record moves to
  `docs/internal/` when it is fixed, not when it is planned.
