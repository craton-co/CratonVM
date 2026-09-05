# H20-1 — the direct-call plan is a second thing every door builds, and filtering it downstream would have traded an open door for a wild jump

**Status: FIXED IN SOURCE, NOT YET VERIFIED BY AN ARM.** Lane H20, 2026-08-21.

**Provenance.** Lane H20 wrote ~950 lines across four files and died to an
infrastructure fault (`stream watchdog did not recover`) immediately before
running its probe, having written no record. **Reconstructed by lane H0 from the
lane's own module documentation**, which carries its reasoning verbatim. **No
claim here was re-measured by H0**, and the probe the lane was about to run has
still not been run.

---

> **VERIFIED AGAINST A BINARY 2026-09-04. §4's owed acceptance measurement has
> been run and it passes.** Status was *"FIXED IN SOURCE, NOT YET VERIFIED BY AN
> ARM"* — a page reconstructed by lane H0 from module documentation after lane
> H20 died to an infrastructure fault, with *"the probe the lane was about to
> run"* still unrun.
>
> §4 states the acceptance in one sentence: *"the 298,000 figure must go to zero
> under `--jdk-only` and stay unchanged under `--real-jdk`."*
>
> ```text
> CRATONVM_INTRINSIC_STATS=1 <vm> --jdk-only  -cp <dir> OsrDoor    0        (was 298,000)
> CRATONVM_INTRINSIC_STATS=1 <vm> --real-jdk  -cp <dir> OsrDoor    298,000  (unchanged)
> ```
>
> Both halves, to the digit, on three runs each (0/0/0 and
> 298,000/298,000/298,000). `probes/OsrDoor.java` is the probe the two records
> describe, transcribed and now kept in the tree.
>
> **§2's hazard is the thing that did NOT happen, and that is the result.** This
> record exists because the brief's approach — filtering the direct-call plan
> downstream — *"would have traded an open door for a wild jump"*. The counters
> show the trade was avoided: `OSR 1` is still reported under `--jdk-only`, so
> the method is still OSR-compiled and only the `bridge` bind is refused. A fix
> that had closed the door by breaking the compile would show `OSR 0`, and a fix
> that had filtered downstream would not show a clean zero here.
>
> **What this does NOT verify — and §5 is right that the list is long.** This
> page is a RECONSTRUCTION: *"No claim here was re-measured by H0."* One
> acceptance measurement passing does not re-derive the ~950 lines across four
> files that lane H20 wrote, nor the reasoning §§1-3 attribute to its module
> documentation. Nothing here witnesses a wrong value or a wild jump — the
> hazard is argued, and the argument is not tested by this counter. §6's
> nominations are untouched.

## 1. What it was asked to do, and what it found instead

The brief proposed transplanting `compile_gate.rs`'s type-level
`CompileAdmission` token onto `JitDirectCall`, to make the bind-time refusal
compiler-enforced rather than reviewer-remembered — and said explicitly that if
the analogy did not hold, **that** was the finding.

It does not hold, and the reason is the valuable part:

> *"The `CompileAdmission` trick does not transplant onto this. It works for the
> backend entry because the refusal is safe at a choke point downstream of every
> door: 'do not compile' is a fallback every caller already has. **A direct-bind
> refusal has no downstream point at all.**"*

## 2. The hazard, which is worse than the bug

`x64/driver.rs`'s `reserve_stack_floor` walk defines a **raw self-call** as *an
`invokestatic` pc with neither an invoke-info entry nor a direct-call plan*.
Every ladder pushes its row and then `continue`s past the `invoke_info`
construction for that pc.

So a row dropped **after** the door leaves that pc with no metadata at all, and
the backend compiles `Thread.currentThread()` **as a call to the enclosing
method**.

> *"Filtering downstream would trade an open door for a wild jump."*

That is the obvious fix, it is what a reviewer would suggest, and it would have
turned a latent policy hole into a live miscompile. This project already has a
standing record on exactly this failure mode — *a JIT-baked direct-call target
must be pinned or refused* — and this is the second independent arrival at it.

## 3. What was built instead

The door refuses **at the bind site**, in the shape `try_compile_inner` already
uses (`if entry != 0 { push; continue; }` — a refusal *falls through*):

| piece | role |
|---|---|
| `DirectCallPolicy` | the witness. **No `Default`**, and **three** states rather than two, so *"never asked"* stays representable and cannot be silently read as *"Compatible"*. |
| `CompileAdmission::declare_direct_call_policy` / `::admits_direct_bind` | the rule, stated once. |
| `CompileDoor::builds_direct_calls` | an **exhaustive `match`**, so a fourth compile door cannot be added without answering the question. |
| `undeclared_direct_bind_rows` | the counter, mirroring `ungated_backend_entries`. It does not stop the accident; **it makes the bypass a number instead of a silence.** |
| `vm/src/jit/helpers.rs::admit_direct_native_entry` | the VM half — the only place the registry's `NativeKind` can actually be read. |

**The exhaustive `match` is the part that answers the brief.** The defect being
fixed is that a third door existed and nobody asked it the question; a `match`
with no wildcard means the compiler asks on behalf of the fourth.

`DirectCallPolicy` carries unit tests for the declare/read/admit cycle,
including that an undeclared admission reads `None` rather than a default.

## 4. The measurement this replaces, and the one still owed

MEASURED by `H12-1` on 2026-08-20 and independently reproduced by H0 — probe: a
`static` method called **once** containing a 300 000-iteration loop, so only OSR
can tier it:

| mode | `Thread.currentThread` direct calls | single-pass | OSR |
|---|---:|---|---:|
| `--jdk-only` | **298,000** | **0/7** | **1** |
| `--real-jdk` | **298,000** | 0/0 | **1** |
| `--jdk-only --nojit` | 0 | 0/0 | 0 |

The MethodEntry door refused all seven sites it examined — that guard is real
and fired. The OSR door bound anyway, and `--real-jdk` is identical, so that
door never consulted the mode.

**The acceptance measurement has NOT been run.** It is: the 298,000 figure must
go to **zero** under `--jdk-only` and stay **unchanged** under `--real-jdk`.

```
CRATONVM_INTRINSIC_STATS=1 <vm> --jdk-only  -cp <dir> OsrDoor
CRATONVM_INTRINSIC_STATS=1 <vm> --real-jdk  -cp <dir> OsrDoor
```

## 5. NOT VERIFIED — the list is long, treat the page accordingly

* **No build and no arm** has run against these 950 lines.
* **The probe was never run.** The lane died one step before it.
* **No wrong VALUE was ever witnessed**, and `H12-1` leads with that:
  `native_hashmap_get_exact` walks the **real `HashMap.table` field**, so
  `H4-1`'s "silently empty map" does not transfer to this door. This is an open
  door, not a live miscompile. It becomes live exactly when the rows it binds
  stop being `bridge` — which is what the retag plan proposes.
* The `reserve_stack_floor` self-call claim in §2 is **ARGUED from source by the
  lane**, not demonstrated by producing a wild jump. It should be — deliberately
  dropping a row downstream in a scratch build and observing the miscompile
  would turn the strongest claim on this page from reasoning into evidence.

## 6. NOMINATIONS

* **N1 — run §4's two commands.** Until then "fixed" is a source claim.
* **N2 — demonstrate the wild jump.** See §5. It is the difference between a
  reviewer believing this page and a reviewer being able to check it.
* **N3 — `undeclared_direct_bind_rows` should be asserted zero from the VM**,
  not merely counted, once the count is observed at zero. The lane's own comment
  says so. A counter nobody asserts on is `H1-1`'s capped sink again.
* **N4 — `RJitMapTierDiff` would have passed as originally specced** (`H12-2`),
  and four changes make it discriminating: hot region in a once-invoked method,
  receiver declared `HashMap` not `Map`, force the overlay, and assert `put`'s
  **return value**. Its expected values must be re-taken against this change.
