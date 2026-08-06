# VM-internal and generated classes are deliberately mislabelled `ClassOrigin::CompatibilityStub` to avoid flipping `is_synthetic_stub`

**Status:** OPEN, and narrowed to **one class**. Filed 2026-07-31; re-verified
2026-08-04; **re-scoped 2026-08-05 by JDK-only wave-2 lane L7**, which measured
the flip and found it is a dispatch change — through read sites this record did
not name. Not a correctness bug in `Compatible` mode; it keeps one row in the
class-origin census labelled as a compatibility substitution when it is not.

## Where this stands

| class | verdict | when |
|---|---|---|
| `cratonvm/synthetic/AnonymousObject$N` | **MIGRATED** to `ensure_generated_class(VmInternal)`, verified | 2026-08-04 |
| the 11 `cratonvm/internal/Unmodifiable*` | **STAYS `CompatibilityStub`** — adjudicated, see below | 2026-08-04 |
| `java/lang/reflect/Proxy$Instance` | **OPEN.** Origin question answered (`VmInternal`); the flip is a dispatch change and is not attempted | 2026-08-05 |

### `cratonvm/synthetic/AnonymousObject$N` — MIGRATED, and verified

The minting site in `vm_exec`'s allocation path calls
`ensure_generated_class(name, n, ClassOrigin::VmInternal)`.

Why it was safe to flip ahead of the other one: this class is **inert at every
`is_synthetic_stub` read site**. Nothing is registered as a native on it,
neither real-protected-stub allow-list names it, and `fabricate_class`'s two
by-name special cases (`Proxy$Instance`, the collection iterators) key on the
*name*, not the origin.

Verified against a real JDK 21 image, `--jdk-only`, before and after:

| | before | after |
|---|---|---|
| `compatibility-stub` | 14 | **13** |
| `vm-internal` | 0 | **1** |
| total rows | 415 | 415 |

Exactly one class moved, and it is the intended one — this record's own
acceptance criterion (*"must drop by exactly the number of classes reclassified
— if it drops by more, something else was swept up"*), met verbatim.

There is a second, unlisted benefit: the old origin cost a full-classpath
rescan per fresh field count, hunting a class file that cannot exist
(`fabricate_class`'s `is_synthetic` branch; `synthetic_upgrade_known_absent`
memoised it away after the first, but the first still ran).

### The `cratonvm/internal/Unmodifiable*` family — adjudicated, deliberately NOT migrated

The `requested_by` census shows these eleven are the largest single group of
compatibility classes on a strict boot, all from one call site
(`vm_init.rs`'s bootstrap block). They are tempting: no class file exists under
those names, which is the `VmInternal` shape.

**Leave them.** They stand in for `java.util.Collections$UnmodifiableList` and
friends — the real `Collections.unmodifiableList()` bytecode is not running,
and that is a compatibility substitution whatever the stand-in is called.
Reclassifying them is the *dangerous* direction this record's blast-radius
section describes: it silences the violation, keeps fabricating, and makes the
zero-stub census report green while the substitution continues. The census
should keep saying so.

L7 acted on that verdict rather than reversing it: the same call site is now
**fallible**, so under `--jdk-only` the eleven are refused instead of
fabricated behind a recorded violation. See
[`ensure_synthetic_class` cannot enforce](ensure-synthetic-class-cannot-enforce-only-record.md).

## `java/lang/reflect/Proxy$Instance` — the measured answer

### The origin question: `VmInternal`, and the in-code marker says so

The original `fabricate_class` marker prescribed `GeneratedProxy`. That is
wrong, for two reasons both already in the tree:

* `ClassOrigin::GeneratedProxy` carries `interfaces: Arc<[ClassId]>`, which the
  shared *supertype* — as opposed to a concrete `$ProxyN` — has no meaningful
  value for.
* `class_manager.rs`'s own `is_generated_proxy_name` says so in terms: *"this
  VM's own `java/lang/reflect/Proxy$Instance` is exactly that, and must NOT be
  counted as a generated proxy"*. The `GeneratedProxy` origin belongs to the
  `$ProxyN` classes it generates.

The in-code marker is corrected. Nothing else about this half is open.

### The safety question: it is a dispatch change — and not through the two predicates this record named

This record said *"Two of those read sites are load-bearing for dispatch …
both real-protected-stub predicates … short-circuit on `if cls.is_synthetic_stub
{ None }`. Flipping the bool changes which natives yield to bytecode."*

**Measured 2026-08-05: flipping `Proxy$Instance` changes neither of those two.**
Both are still there — `native_override.rs`'s
`synthetic_stub_kind_should_yield_to_real_bytecode` and an inline second copy in
`vm_exec::invoke_or_native` — but each tests `real_protected_stub_class(name)`
*before* it reads `is_synthetic_stub`, and that allow-list is eleven literal
class names plus the `CRATONVM_REAL` selector (unset by default, so
`prefers_real` is false for everything). `Proxy$Instance` is on neither list.
The `is_synthetic_stub` read is unreachable for it.

That also settles this record's *"dispatch check that actually matters"* for
this one change by construction rather than by run: the
`synthetic_stub_should_yield_to_real_bytecode` verdict for `Proxy$Instance` is
`false` before the flip and `false` after, because the gate above it is `false`
in both.

**It is still a dispatch change**, through three read sites this record does not
name — which is the reason it is being handed on rather than done:

* **`vm_exec.rs`'s synthetic-stub retarget** (`class_name.starts_with(
  "cratonvm/internal/") || cls.is_synthetic_stub || cls.is_interface()`). This
  branch exists so that a method registered as a native under a class's *own
  exact name*, but not declared in its method table, is still found — instead
  of the hierarchy walk landing on an inherited `java/lang/Object` body.
  `Proxy$Instance` is exactly that case: it has a NATIVE-flagged `<init>` from
  `synthetic_stub_ctor_methods` and native registrations in
  `reflect_annotations.rs`, and it matches neither other disjunct. Flip the
  origin and it leaves the branch.
* **`vm_exec.rs`'s `if !class.is_synthetic_stub { → invoke_on_class_shared }`**
  arm, which would newly route `Proxy$Instance` receivers to bytecode-style
  dispatch on a class whose only methods are the synthetic ctor entries.
* **`vm_object.rs`'s all-classes native-method scan**, which skips stubs and
  would newly include `Proxy$Instance.<init>`.

### Why the obvious fix does not work either

The root cause is that `is_synthetic_stub` answers two different questions:

1. *is this a compatibility substitution?* — the census and policy question,
   and the one `ClassOrigin` is authoritative for;
2. *does this class have no bytecode, so dispatch must look for natives under
   its own name?* — the question all three read sites above are actually
   asking.

Separating them is the prerequisite. The obvious separation —
`!origin.has_real_bytes()` — does **not** work as a drop-in: `VmInternal` and
`VmArray` both answer "no real bytes", so every already-`VmInternal` class
would change branch. That includes `cratonvm/synthetic/AnonymousObject$N` (the
allocation shape behind every `HashMap` node in the VM) and every
`cratonvm/synthetic/AmbiguousName$…` stand-in, both of which today take the
*non*-stub arm at the `invoke_on_class_shared` site. Swapping the predicate
would move them, which is a `Compatible`-mode behaviour change on the busiest
allocation shape in the VM.

So the sequence is: introduce a predicate that means (2) exactly and prove it
equals today's `is_synthetic_stub` at each of the three sites, *then* flip
`Proxy$Instance`. That is a dispatch change with its own evidence, not a
one-line origin edit, and it wants its own lane.

### One more thing that lowers the priority

`Proxy$Instance` is no longer the default proxy supertype. `real_proxy_super()`
defaults **on**, so generated `$ProxyN` classes extend the real
`java.lang.reflect.Proxy`; the `Proxy$Instance` super is the opt-out
(`CRATONVM_REAL_PROXY_SUPER=0`). It is fabricated on none of the three
workloads L7 measured (strict boot, `JdkOnlyCensusLoadProbe`,
`JdkOnlyBreadthProbe` — zero rows in all three class-origin censuses).

## The read-site count

`is_synthetic_stub` appears 187 times across 24 files (ripgrep, 2026-08-05).
That figure is the wrong instrument, and this record should stop quoting it as
if it were a work list: the overwhelming majority are `is_synthetic_stub:
false` struct-literal initialisers and test assertions. **Production *read*
sites number about 35**, and the audit above enumerates every one that can
observe `Proxy$Instance`.

## What specifically must change

1. ~~Migrate `AnonymousObject$N`~~ — done 2026-08-04.
2. ~~Decide the right origin for `Proxy$Instance`~~ — done: `VmInternal`,
   marker corrected.
3. Split `is_synthetic_stub`'s two meanings (see above), then flip
   `Proxy$Instance` and re-run the census.
4. Audit any `ensure_synthetic_class` caller whose name matches
   `generated_class_origin_for_name` (`$$Lambda` → `GeneratedLambda`,
   `$ProxyN` → `GeneratedProxy`, `Generated*Accessor*` → `ReflectionAccessor`).
   None fires on the three measured workloads.
5. Only after that: delete `is_synthetic_stub` and let `origin` be the single
   source of truth (contract §5: *"Convert it to a pure derived mirror now,
   delete it in a later wave."*).

## How to verify a fix

* `--dump-class-origins` must show `vm-internal` (not `compatibility-stub`) for
  `java/lang/reflect/Proxy$Instance`, and `generated-proxy` /
  `generated-lambda` / `reflection-accessor` for the name-recognised flavours.
* The `--jdk-only-report` `counts.compatibility_classes` must drop by exactly
  the number of classes reclassified — if it drops by more, something else was
  swept up.
* **The dispatch check that actually matters:** before and after, dump the set
  of `(class, method)` pairs for which
  `synthetic_stub_should_yield_to_real_bytecode` returns `true`. It must be
  identical. (For `Proxy$Instance` alone this is answerable by reading — see
  above — but it is not for a change that touches the predicate itself, which
  step 3 does.)
* Full regression suite green, and `native-builtins/tests/stub_ratchet.rs`
  moving only by the number of registrations the change deliberately retags —
  this record's own change touches class origins, not native registrations, so
  for it the ratchet must not move at all.

## Blast radius if done wrong

* Flipping `is_synthetic_stub` for a class that some read site uses as a
  "trust this receiver's layout" proxy silently changes which code path runs
  for that class. The three sites named above are the known instances for
  `Proxy$Instance`; a change to the *predicate* reaches all ~35.
* Reclassifying too eagerly hides a genuine compatibility substitution behind a
  legitimate-looking origin, which makes the zero-stub gate report green while
  the substitution continues. That is the same failure mode as the
  `ensure_generated_class` misuse described in
  [`ensure_synthetic_class` cannot enforce](ensure-synthetic-class-cannot-enforce-only-record.md),
  and it is only guarded by a `debug_assert!`.
