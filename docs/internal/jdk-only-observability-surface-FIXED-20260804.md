# The `--jdk-only` observability surface — CLOSED 2026-08-04

**Status:** FIXED. All three holes filled; moved out of `docs/known-issues/`
per that directory's own rule (a record moves here when it is fixed, not when
it is planned).

## What closed, and the evidence

### 1. `real_declaring_method` — closed during the wave-1 re-land

Unchanged from the filing below.

### 2. `ClassOriginEntry::requested_by` — closed 2026-08-04

The first run of the instrument proved the record had aimed it at the wrong
half of the population. `ClassManager` is called with a bare name, so the
filing (and the in-code marker) concluded the requester was the *interpreter's*
to supply — the frame that ran the `new` / `checkcast` / `Class.forName`. That
was implemented: `SharedVm::load_class_concurrent_for` takes the violation
count before the load and calls `ClassManager::attach_origin_requester` after,
under the same write lock, so no other thread's fabrication can be
misattributed; the requester is three borrowed `&str`s and nothing is formatted
or allocated unless a violation was actually recorded.

**And `requested_by` was still `null` on all 415 rows.** Every one of the 14
compatibility classes a `--jdk-only` boot fabricates arrives through the
*direct* `ensure_synthetic_class` API — a native asking for an allocation shape
— which has a Rust caller and no Java frame at all. The Java-frame instrument
was correct and measured a population that is very nearly empty.

So `admit_compatibility_class` is now `#[track_caller]`, threaded through
`fabricate_class` and all three `ensure_*` entry points, and records the **Rust
call site**. The Java frame, where one exists, is layered on top as
`"org/foo/Bar.baz(Desc) via native-builtins/src/lib.rs:36608"`. Both halves
answer different questions and both are kept: the Java frame says which
application code depends on the fabrication, the Rust site says which VM code
performed it.

Verified on a `--jdk-only` boot against a real JDK 21 image: 14 of 415 rows
carry a requester, and they name three call sites —

| Class(es) | Requester |
|---|---|
| the 11 `cratonvm/internal/Unmodifiable*` | `vm/src/vm/vm_init.rs:1227` |
| `cratonvm/synthetic/AnonymousObject$N` | `vm/src/vm/vm_exec.rs:9814` |
| `java/util/Enumeration$Impl` | `vm/src/vm/vm_init.rs:1052` |
| `java/util/Comparator$Native` | `vm/src/vm/vm_init.rs:1110` |

That table is the finding, not just the fix. The `ensure_synthetic_class`
record scopes its migration at "52 live call sites in 27 files"; **three of
them fire on a strict boot.** A migration driven by this census is a different
size of job from one driven by a grep, which is exactly what the record meant
by "whether the census is a to-do list or a list of names".

### 3. `--trace-jdk-only` was a poll — closed 2026-08-04

`ClassManager` carries an optional VM-scoped violation sink — a field, not a
process global (contract §2), so two VMs in one process see only their own —
which `vm-cli` installs immediately after the `vm-init` drain. The drain then
advances its origin watermark *without* re-rendering, so each violation prints
exactly once.

Verified: a class fabricated mid-run appears interleaved with the program's own
output at the point it happens —

```
PROBE: start
[cratonvm][jdk-only:live] compatibility class requested: cratonvm/synthetic/AnonymousObject$3
PROBE: forName threw java.lang.ClassNotFoundException
PROBE: about to exit
```

The remaining four recording sites stay polled, and that is now recorded as a
decision rather than a deferral. Registration refusals all happen inside
`Vm::new`, which the first drain immediately follows, so the poll already
reports them at their real time of occurrence. The three JIT/dispatch sinks are
**process**-global (see additional-wave2-markers §2); giving them a per-VM live
sink means giving them a VM first, and a per-VM sink hung off process-global
state would report another VM's refusals as this one's — worse than reporting
them late.

---

*The original filing follows unchanged, for the reasoning that produced these
three items.*

---


**Status:** OPEN — JDK-only wave-2 work items, filed 2026-07-31 as three holes,
re-verified against the re-landed tree the same day and reduced to two. None of
these causes wrong behaviour. They matter because they are the instruments the
*dangerous* items in this directory have to be measured with — every one of the
silent-misbehaviour records here says "needs runtime evidence from the census".

> **Evidence provenance.** All three findings were originally read from a
> working tree that was subsequently destroyed and re-landed. Everything below
> is re-read from `C:\craton\wt-jdk-only` (branch `feat/jdk-only-mode`) on
> 2026-07-31. **`vm-cli/src/main.rs` was being edited by another agent during
> this pass**, so its citations are given by function and marker name only, with
> no line numbers.

---

## 1. `real_declaring_method` — CLOSED, with a caveat worth reading

### What changed

The original filing recorded that `write_native_registry_census_v2` in
`vm-cli/src/main.rs` emitted `"real_declaring_method": null` on every row, and
that filling it in needed a `ClassManager`-side "peek at the image without
loading" accessor that no wave-1 contract section defined.

Two things happened in the re-land, and they happened together:

**The two divergent schema-2 writers were unified.** There were, briefly, two
independently written `schema_version: 2` census writers — one in
`vm/src/vm/vm_init.rs` and one in `vm-cli/src/main.rs`. That is a genuine
consumer hazard: one `schema_version` with two shapes means a reader that works
against one silently mis-reads the other. `SharedVm::dump_native_census_json`
(`vm/src/vm/vm_init.rs` ~3851) is now **the only** native-census writer, and
`vm-cli`'s `write_jdk_only_dumps` calls it. Its doc comment records the three
points on which the two disagreed and how each was resolved: the top-level
`"mode"` key is kept (it is what lets `registry-real.json` and
`registry-no-stubs.json` be told apart by content, which is the whole point of
`scripts/jdk-only-census.sh`); registration order is kept as the tie-break for
duplicate triples rather than the launcher's `registered_by` sort (because
`registered_by` order destroys the overwrite chronology *and* makes row order
depend on the build machine); and `real_declaring_method` is filled in rather
than emitted `null`. The sibling writers `dump_class_origins_json` (~4021) and
`dump_jdk_only_report_json` (~4088) moved the same way. The schema-1 writer
`dump_native_registry_json` (~3750) remains as a thin `verbose = false` wrapper
for the `vm` crate's own tests.

**The column is populated** (~3949):

```rust
let declaring = cm.get_loaded_class_id(&row.class).and_then(|id| cm.get_class(id));
let method = declaring.and_then(|c| c.find_method(&row.name, &row.descriptor));
// "loaded": declaring.is_some(), "declared": method.is_some(),
// "acc_native": …is_native(), "has_code": !is_native() && !is_abstract()
```

The `has_code` derivation is deliberately from access flags, not from
`ClassFileMethod::code()`, because the latter returns `None` for a
not-yet-force-decoded lazy attribute — a decode-state artefact, not a fact about
the class (JVMS §4.6). The whole loop takes **one** class-manager read lock
rather than one per row.

The original concern — that the probe must not perturb the run it measures — was
respected: `get_loaded_class_id` is a lookup through `&self`, not an initiating
load.

### The caveat: `"loaded": false` is not the same as "not in the image"

Because the probe reads the **loaded class store** rather than the boot image,
a class this run never touched reports `loaded: false, declared: false,
acc_native: false, has_code: false`. That is an honest answer to a different
question than contract §9's. For a shutdown census over the full registry, most
rows will be exactly that, so:

* **A row of all-`false` means "this run did not exercise the class", not "the
  JDK does not declare this method".** Do not read it as evidence either way in
  the 157-entry reclassification.
* The census is only as informative as the workload is broad. Take it from a
  run that actually loads the classes you are adjudicating.
* The writer's own doc adds one more caveat: the lookup is requester-less, so
  for a name no built-in loader has defined it can answer from a lone
  user-defined loader's copy.

A true image probe — reading the class file's method table from jimage / module
path / classpath entry without defining the class — is still unwritten, and is
still the right long-term shape. But the column is no longer a blocker: it
answers for every class the run touched, which is the population that matters
for the reclassification.

---

## 2. `ClassOriginEntry::requested_by` is `null` on every row — STILL OPEN

### What is wrong

`classloading/src/class_manager.rs`, `dump_class_origins` (~2421), with the
marker at ~2428:

```rust
.map(|c| ClassOriginEntry {
    name: c.name.to_string(),
    origin: c.origin.as_str().to_string(),
    reason: c.origin.reason().map(|r| r.to_string()),
    requested_by: None,          // <-- always
    real_bytes_found: c.origin.has_real_bytes(),
    loader_id: c.loader_id.to_native_id(),
})
```

The same hole exists on the violation side — `admit_compatibility_class`
(~2455) builds `CompatibilityClassRequested` with `requester: None` and a
`JDK-ONLY-NOTE` at ~2488 pointing back at `dump_class_origins`.

### Why it was deferred

From the marker:

> `requested_by` stays `None` here, permanently as far as this crate is
> concerned. The requesting `owner/Class.method(Desc)` is known to the
> *interpreter* — it is the frame that ran the `new` / `checkcast` /
> `Class.forName` — and is not reachable from `ClassManager`, which is called
> with a bare name. Populating it means threading the current frame through the
> load path, which belongs to the interpreter agent's half of the contract
> (§7); the field is in `ClassOriginEntry` so that half can fill it without
> another schema change.

Note the re-land sharpened this from "deferred to a later wave" to "permanently
as far as this crate is concerned" — the marker now names the owner of the fix
rather than just deferring it.

### What must change

Either thread an `Option<&RequesterRef>` through `load_class` (a wide,
mechanical, cross-agent change), or — cheaper and probably better — have the
*interpreter's* resolution path attach the requester to the violation after
`ClassManager` records it, since that path already knows the owning method and
descriptor. The second option keeps `ClassManager`'s signature stable and is
what the marker's "so that half can fill it" is pointing at.

One wrinkle to plan for: `admit_compatibility_class` dedupes violations by class
name (`origin_violations_seen`), so only the **first** requester of a given
class can ever be attached. If the goal is "which call sites depend on this
fabrication", one requester per class is a starting point, not the answer.

### How to verify

A `--jdk-only` run against a workload that requests a known-absent enterprise
class must name the requesting method in both the violation and the census row,
and the named method must match what a stack trace at that point would show.

### Why it matters

"Class `X` was fabricated" is actionable. "Class `X` was fabricated **because
`org/foo/Bar.baz(…)` resolved it**" is a work item. With 52 live
`ensure_synthetic_class` call sites plus the whole `load_class` chain, the
difference is whether the census is a to-do list or a list of names.

---

## 3. `--trace-jdk-only` is a polling trace, not a live one — STILL OPEN

### What is wrong

The flag is documented (contract §9) as *"Log every violation as it happens."*
It does not. `vm-cli/src/main.rs`'s `ViolationWatermark` carries the
`JDK-ONLY-NOTE`:

> `--trace-jdk-only` is a **poll**, not a live trace. The two recording sites
> (`ClassManager::origin_violations`,
> `NativeMethodRegistry::refused_registrations`) are append-only vectors, so the
> launcher can only drain them at the points it holds the VM: right after
> `Vm::new` (which is when registration refusals actually happen — draining only
> at shutdown would report them minutes late, after the failure they caused) and
> again at shutdown. A genuinely live trace needs a VM-scoped sink installed at
> the recording sites themselves; that is a wave-2 change to `classloading` and
> `native-api`, not something the launcher can fake.

The mechanism is `trace_jdk_only_violations(shared, watermark, phase, explain)`,
which skips past each log's watermark, renders the new entries (long-form under
`--explain-jdk-only`, redacted one-liners otherwise) and prints them prefixed
`[cratonvm][jdk-only:<phase>]`.

Registration refusals are fine — they all happen inside `Vm::new`, and the first
drain immediately follows it. **Class-origin violations are the problem**: they
occur throughout the run and all surface at shutdown, detached from whatever the
program was doing.

### Why it was deferred

A live trace needs a VM-scoped sink (per contract §2: *no process globals*) that
the recording sites in `classloading` can push to and the launcher can subscribe
to. That is a new cross-crate interface, and no wave-1 contract section defines
one. Wave 1 correctly approximated rather than inventing an API other agents
were coding against.

### What must change

A VM-scoped violation sink — a callback or channel installed on `VmConfig` /
`SharedVm` at init, invoked at the two recording sites. Not a global; the repo
has an explicit history of process-global native caches leaking across VMs in
one process.

### How to verify

* Run a workload that fabricates a class at a known point mid-run. The trace
  line must appear interleaved with the program's own output at that point, not
  after it.
* Two VMs in one process must see only their own violations. That is the
  regression the "no process globals" rule exists to prevent, and a sink is
  exactly the kind of thing that gets made global by accident.

### Why it matters

A shutdown-batched trace cannot correlate a violation with the code that caused
it — which, combined with hole 2 above (`requested_by` is `null`), means a
mid-run class fabrication currently arrives with neither a timestamp nor a
requester. Fixing either one alone recovers most of the value.

---

## A fourth instrument arrived that was not in the original filing

`jfr/src/jdk_only.rs` (new, and **untracked** in git as of 2026-07-31 — treat it
as landed-but-not-committed) adds seven aggregate counters derived at report
time from the artefacts above: violation totals by `kind`, class origins by
`origin`, native registrations and invocations by `kind`, real-bytecode shadow
attempts, missing natives by `module`, and generated classes by `generator`.

It is deliberately *not* a telemetry subsystem — no collector, no background
thread, no state of its own, nothing incremented on a hot path. It folds over
data the VM already keeps. Two properties are worth knowing before using it as
evidence:

* **Disabled by default**, and a disabled aggregate ignores every `add_*` call
  without iterating its input. See `TELEMETRY_ENABLING_FLAGS` for the opt-in.
* **Every label is a `&'static str` from a `const` table in that file.** There
  is no code path from an owned `String` to a label, so a class name, jar path,
  argument or environment value *cannot* be emitted. That is a structural
  guarantee, not a convention, and it is also a limitation: the counters can
  tell you *how many* `compatibility-class-requested` violations a run produced
  and never *which classes*. For the "which", you still need the census files.

The counter block carries its own `counter_schema_version`, independent of the
report envelope's `schema_version: 1` and the census's `schema_version: 2`.

---

## Combined blast radius

These are additive diagnostics, so the risk is low — with three exceptions:

* Any future **image** probe for `real_declaring_method` must not initiate class
  loading. The current loaded-store probe does not; a replacement that resolves
  through the ordinary class-loading path would silently change the run it is
  measuring, and every conclusion drawn from a census-bearing run would become
  suspect. This is the one place where a "harmless observability fix" can
  invalidate the data set.
* Threading a requester through `load_class` touches ~200 call sites. A
  mechanical error there is a compile error, not a silent one — but the churn is
  large enough to collide with any other in-flight work on those files.
* The live-trace sink must be VM-scoped. A process-global sink would break
  multi-VM-in-one-process runs and would be a direct violation of contract §2.

## Related

* [`System.exit(N)` bypasses the JDK-only census entirely](jdk-only-system-exit-census-FIXED-20260804.md)
  — the instruments in this record are only as good as the exit paths that
  reach them, and one whole class of run reaches none of them.
