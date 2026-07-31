# The `--jdk-only` observability surface has three unfilled holes: `real_declaring_method`, `requested_by`, and a polling `--trace-jdk-only`

**Status:** OPEN — JDK-only wave-2 work items, filed 2026-07-31. None of these
causes wrong behaviour. They matter because they are the instruments the
*dangerous* items in this directory have to be measured with — every one of the
silent-misbehaviour records here says "needs runtime evidence from the census",
and these three are the reason that evidence is not yet obtainable.

> **Evidence provenance.** All three findings were read directly from the
> working tree of `C:\craton\cratonvm` (branch `dev`, HEAD `0c54a9184`) on
> 2026-07-31, in `vm-cli/src/main.rs` and
> `classloading/src/class_manager.rs`. Those uncommitted wave-1 edits were
> subsequently reverted out of the working tree — see the *Wave-1 revert* note
> in [`README.md`](README.md). The quotations below are from the code as it
> stood; re-landing wave 1 is a prerequisite for all three items.

---

## 1. `real_declaring_method` is `null` on every row of the schema-2 native census

### What is wrong

Contract §9 bumps the native census to `"schema_version": 2` with a
`real_declaring_method` object per entry. `write_native_registry_census_v2` in
`vm-cli/src/main.rs` emits the key with a literal `null` on every row:

```rust
// See the JDK-ONLY-NOTE above: null, never fabricated.
out.push_str("      \"real_declaring_method\": null\n");
```

The field is the one that decides whether a registered native is a legitimate
bridge or a stub shadowing real bytecode. Its contract shape is
`{present, acc_native, has_code}` — *"does the real JDK image declare this
method, is it `ACC_NATIVE`, does it carry a `Code` attribute"*.

### Why it was deferred

From the in-code note:

> Answering it needs a **non-initiating** lookup of `(class, name, descriptor)`
> against the boot image: resolving it through the ordinary class-loading path
> at shutdown would load hundreds of classes that the run never touched and
> change what the census reports about itself. §9 explicitly says to emit `null`
> rather than invent data, so the key is present with a null value (a consumer
> can tell "not answerable yet" from "schema changed"). Wiring it up needs a
> `ClassManager`-side "peek at the image without loading" accessor, which no
> wave-1 contract section defines.

That reasoning is correct and should not be second-guessed: a shutdown-time
census that loads classes measures itself, not the run.

### What must change

A `ClassManager` accessor that reads the class file's method table from the
image (jimage / module path / classpath entry) **without** defining the class,
initiating loading, or touching the class store — returning
`Option<{present, acc_native, has_code}>`. Then populate the column from it.

### How to verify

* Two runs of the same workload must produce identical
  `--dump-class-origins` output whether or not `--dump-native-registry` was
  requested. If requesting the census changes the class-origin census, the probe
  is initiating loading and the fix is wrong.
* Spot-check against `javap` on the same JDK image for a handful of known
  `ACC_NATIVE` methods and a handful of concrete ones.

### Why it matters

Without this column, "is this `SyntheticStub` registration shadowing real
bytecode?" cannot be answered in bulk — which is exactly the question the
157-entry baseline reclassification (see
[`NativeKind` is ambient](native-kind-is-ambient-and-defaults-to-syntheticstub.md))
has to answer 157 times.

---

## 2. `ClassOriginEntry::requested_by` is `null` on every row

### What is wrong

`classloading/src/class_manager.rs`, `dump_class_origins`:

```rust
.map(|class| ClassOriginEntry {
    name: class.name.to_string(),
    origin: class.origin.as_str().to_string(),
    reason: class.origin.reason().map(str::to_string),
    requested_by: None,          // <-- always
    real_bytes_found: class.origin.has_real_bytes(),
    loader_id: class.loader_id.to_native_id(),
})
```

The same hole exists on the violation side —
`JdkOnlyViolation::CompatibilityClassRequested::requester` is likewise always
`None`.

### Why it was deferred

From the in-code note on `record_compatibility_class_violation`:

> The requesting `owner/Class.method(Desc)` is known to the **interpreter's
> resolution path**, not to `ClassManager` — plumbing it here means threading a
> requester through `load_class`, which is called from ~200 sites in files owned
> by other agents. Deferred to a later wave; the violation is still actionable
> without it because it names the class and the reason.

### What must change

Either thread an `Option<&RequesterRef>` through `load_class` (a wide,
mechanical, cross-agent change), or — cheaper and probably better — have the
*interpreter's* resolution path attach the requester to the violation after
`ClassManager` records it, since that path already knows the owning method and
descriptor. The second option keeps `ClassManager`'s signature stable.

### How to verify

A `--jdk-only` run against a workload that requests a known-absent enterprise
class must name the requesting method in both the violation and the census row,
and the named method must match what a stack trace at that point would show.

### Why it matters

"Class `X` was fabricated" is actionable. "Class `X` was fabricated **because
`org/foo/Bar.baz(…)` resolved it**" is a work item. With ~64
`ensure_synthetic_class` call sites plus the whole `load_class` chain, the
difference is whether the census is a to-do list or a list of names.

---

## 3. `--trace-jdk-only` is a polling trace, not a live one

### What is wrong

The flag is documented (contract §9) as *"Log every violation as it happens."*
It does not. `vm-cli/src/main.rs`'s `JdkOnlyTraceCursor` carries the note:

> this is a **polling** trace, not a callback. `--trace-jdk-only` is documented
> as "log every violation as it is recorded", and the launcher can only
> approximate that by draining the two append-only logs
> (`NativeMethodRegistry::refused_registrations`,
> `ClassManager::origin_violations`) at the points where it holds the VM:
> immediately after `Vm::new` (which is where every registration refusal is
> produced) and at shutdown. Class-origin violations recorded mid-run are
> therefore reported at shutdown rather than at the instant of recording. A true
> as-it-happens trace needs a VM-scoped sink the recording sites can push to;
> that is not in the wave-1 contract, so it is not invented here.

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

## Combined blast radius

These are additive diagnostics, so the risk is low — with three exceptions:

* The `real_declaring_method` probe **must not initiate class loading**. If it
  does, the census silently changes the run it is measuring, and every
  conclusion drawn from a census-bearing run becomes suspect. This is the one
  place where a "harmless observability fix" can invalidate the data set.
* Threading a requester through `load_class` touches ~200 call sites. A
  mechanical error there is a compile error, not a silent one — but the churn is
  large enough to collide with any other in-flight work on those files.
* The live-trace sink must be VM-scoped. A process-global sink would break
  multi-VM-in-one-process runs and would be a direct violation of contract §2.
