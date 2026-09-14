# Natives over real JDK classes — the facts lanes keep rediscovering

**What this is.** Eight mechanism facts about how a Rust native comes to run
instead of real JDK bytecode, how it gets registered, how it reads fields, and
what the instruments around it can and cannot see. Every one of them was
rediscovered independently by more than one lane of the `--jdk-only` strict-mode
campaign, each time at the cost of a wrong diagnosis first. None of them is a
defect record; they are the background you need before *reading* a defect record.

**What this is not.** Not a design (`docs/feature-designs/`), not a defect
(`docs/known-issues/`), not a plan. It describes the tree as it stands.

---

> ## Provenance — read this once, then apply it to every number below
>
> This document was written by a lane that was **not permitted to build or run
> anything**. Every claim here falls into exactly one of two buckets, and the
> text says which:
>
> * **Source-verified** — I read the code, the test, or the script and quote it.
>   A `file:line` with quoted text is source-verified.
> * **Lane-reported** — a measurement some other lane says it took. Those are
>   attributed by name and marked *reported*. **No lane in the pool that
>   produced this document was permitted to build**, so no lane-reported number
>   here has been reproduced by its author.
>
> Line numbers rot; anchor on the quoted text and the enclosing function name.
> Several files cited here were being edited concurrently while this was
> written. See `docs/feature-designs/synthetic-class-fallibility.md`
> ("Line numbers are a hint; the enclosing function name is the anchor").

---

## 1. Reachability — how a native comes to run instead of real JDK bytecode

The campaign's working rule was a "four doors" model: a native reaches a real
JDK receiver only if (a) the JDK method is `ACC_NATIVE`, (b) the registration is
`NativeKind::Intrinsic`, (c) the triple is in `force_native_over_real_jdk_bytecode`,
or (d) nothing in the hierarchy declares `Code`.

**That model is wrong in the direction that matters: it is too restrictive, and
it names the wrong gate.** The accurate statement:

> **On the cold interpreter paths, registration itself is the gate.** A native
> registered for `(class, method, descriptor)` beats real bytecode by default,
> with no list consulted. `force_native_over_real_jdk_bytecode` and
> `vm_exec.rs`'s `check_override` chain exist to *reinstate* that default on the
> warm, cached, reflective and JIT paths, which would otherwise prefer bytecode.
> `NativeKind` only ever **subtracts**.

Source-verified:

* `vm/src/runtime/interpreter/native_override.rs`, module banner:
  *"**A registered native wins over real bytecode unconditionally.** The
  predicates decide *which* methods are registered as overrides, not whether an
  override applies once it exists."*
* `resolve_step1_native` (`native_override.rs`, `try_stackless_invoke` step 1)
  hard-codes `compat_native_wins = true`, with the comment that it *"reproduces
  the pre-§7 'a registered native unconditionally wins here'"*. Same hard-coded
  `true` in `resolve_native_for_dispatch` (`vm/src/runtime/interpreter.rs`).
* `invoke_or_native` (`vm/src/vm/vm_exec.rs`): `compat_native_wins` is `!has_real`,
  and `has_real` can only be true for a `SyntheticStub` on an allow-listed class.
  A `Bridge` on a concrete JDK method wins outright.
* `native-api/src/registry.rs`, in the `java/lang/String` drop arm:
  *"Neither list ever decided anything, because `resolve_step1_native` … resolves
  the triple and dispatches whatever it finds before either list runs, and it has
  no list of its own. **Registration was always the real gate**."*

**Door (b) does not exist as stated.** `resolve_dispatch` (`vm/src/vm/vm_exec.rs`)
does contain *"Step 2 — a reviewed intrinsic may shadow concrete bytecode"*, but
its only non-test caller (`vm/src/runtime/interpreter/invoke.rs`) sits inside an
`if is_native {` block, and step 1 returns unconditionally for
`method.is_native()`. Steps 2–4 execute only from `vm/tests/jdk_only_dispatch.rs`.
On the live adapter (`resolve_native_dispatch_wave1`) the kind is read only
*after* `compat_native_wins`, and in `Compatible` mode it is discarded entirely
(*"Compatible: the kind is irrelevant"*). What `Intrinsic` actually buys is
exemption from the `--jdk-only` yield —
`if policy.is_jdk_only() && bytecode_available && kind != NativeKind::Intrinsic` —
plus JIT direct-bind approval and survival of two registration-time drop arms.

Doors (a), (c) and (d) survive. (d) is real in three places: the
`method.is_abstract()` disjunct that heads `check_override`, `resolve_dispatch`'s
documented `method.code() == None` deviation, and the `has_own_bytecode` early
return in `invoke.rs`'s step-1 hierarchy walk.

**The practical consequence.** "Is this native registered as `Intrinsic`?" is the
wrong first question when a native unexpectedly runs — or unexpectedly does not.
The right ones are: *is it registered at all for this exact triple*, *which
registrar registered it last* (§3), and *which dispatch path is this call
taking*. `check_override`'s own census comment puts the count at ~302 disjuncts,
of which exactly one is a no-`Code` door and the rest are name exceptions.

Related: `docs/architecture/member-resolution.md`,
`docs/jdk-only-native-review.md` (the `SyntheticStub` → `Bridge`/`Intrinsic`
checklist).

## 2. The Cargo feature is not the runtime mode

`register_builtins` → `register_synthetic_overrides` → `register_phase50..72_natives`
run under a **double** gate, and the outer one is not the interesting one.

Source-verified, `vm/src/vm/vm_init.rs`:

```rust
        #[cfg(feature = "synthetic-jdk")]
        {
            if config.use_synthetic_jdk {
                // Synthetic mode: register all ~5,200 Rust stubs for full JDK API coverage
                register_builtins(&mut native_methods);
```

with the design intent stated ten lines below: *"on `use_synthetic_jdk`, not on
the Cargo feature, so a feature-enabled binary running real-JDK mode is
unaffected."* The `else` arm reaches `register_essential_natives_with_shims`; a
separate `#[cfg(not(feature = "synthetic-jdk"))]` block is the default build's
path to the same registrar.

Two corollaries that between them explain a whole class of this campaign's bugs:

1. **A `#[cfg(feature = "synthetic-jdk")]` gate is a build-time answer to a
   runtime question.** A feature-enabled binary running `--real-jdk` compiles
   every synthetic registrar and must decline them at runtime. Layout selection
   is likewise a runtime predicate, never a `cfg` — see
   `cl_has_synthetic_layout` (`native-builtins/src/classloader.rs`) and the
   `drop_real_layout_synthetic` arms in `NativeMethodRegistry::register`.
2. **`synthetic-jdk` is in no crate's default feature set.** Source-verified:
   `vm/Cargo.toml` `default = ["awt", "management"]`; `vm-cli/Cargo.toml`
   `default = ["mimalloc"]`; `native-builtins`, `native-collections` and
   `cratonvm-embed` all `default = []`. `vm/Cargo.toml` says so in a comment
   (*"NEW-11: `synthetic-jdk` is intentionally NOT in the default feature set."*).
   So in a plain CLI build those registrars **never compile in at all**, and
   `vm/src/native/builtins.rs` supplies no-op shims so inline call sites still
   resolve. Asking for the mode without the feature is a hard error
   (`VmConfig::require_synthetic_jdk`), not a silent downgrade.

Watch the default asymmetry when reading a test: `VmConfig::default()` (tests,
embedding) is **synthetic**; `VmConfig::for_launcher()` (the CLI) is **real**.
See `vm/src/config.rs` (`EMBEDDED_DEFAULT_JDK_MODE` / `LAUNCHER_DEFAULT_JDK_MODE`)
and memory of this hazard in `docs/synthetic-vs-real-explained.md`.

## 3. `register()` is last-registration-wins

Source-verified, `NativeMethodRegistry::register` (`native-api/src/registry.rs`):

```rust
        match prior_slot {
            Some(idx) => {
                if let Some(slot) = self.slots.get_mut(idx as usize) {
                    slot.callback = callback;
```

The file states it in prose too: *"`register()` is last-registration-wins"* and
*"Re-registration of a key we have already seen UPDATES THE EXISTING SLOT IN
PLACE."* Duplicates are never rejected and never warned about. One nuance: the *kind* is not blindly overwritten — a prior **chosen**
`NativeKind` survives a later registration that expressed no opinion. The
**callback** is always overwritten.

**The diagnostic method — this is the part lanes kept getting wrong.** Compare
duplicate registrations by **enclosing registrar function**, not by raw line
order. Line order within a file is meaningless across functions; what decides is
the order `vm_init` calls the registrars. Two live shapes:

* Both registrars on the real-JDK arm — the later `register*` call in
  `vm_init`'s sequence wins, regardless of which file it lives in.
* One registrar reachable only from `register_synthetic_overrides` — then it
  **cannot register at all** in real-JDK mode (§2), and the other copy wins by
  default rather than by ordering. A "shadowing" verdict that ignores this is
  backwards.

Two instances this campaign found, both worth knowing as patterns:

* **`ClassLoader.defineClass2`** — two registrations, and the **shadowed** body
  was the one carrying the hardened `checked_add` ByteBuffer decode; the winner
  hard-coded slot 0 as the backing `byte[]` and *clamped* an out-of-range
  `(off, len)` instead of rejecting it. Named in
  `native-builtins/tests/duplicate_registration_gate.rs`. Scoped: the shadowing
  registrar is reachable only via `register_synthetic_overrides`, so this bit in
  **synthetic-JDK mode only**. A tombstone comment in
  `native-builtins/src/classloader.rs` (search `W7-13`) records the convergence
  onto the single hardened decoder; the duplicate *registration* still exists.
* **`Integer.toString(II)`** — two registrations, and the winner
  (`native-builtins/src/lang_math.rs`, `native_integer_to_string_radix`, reached
  from `register_wrapper_natives`) performs **no radix validation** at all, while
  the loser (an inline closure in `native-builtins/src/lib.rs`) implements Java's
  `if (radix < 2 || radix > 36) radix = 10` clamp. Source-verified consequences
  of the winner: `toString(5, 40)` panics inside `char::from_digit`,
  `toString(5, 1)` never terminates, `toString(5, 0)` divides by zero.
  **The campaign's write-up of this one was wrong about the symptom**: the
  "handles only radix 2/8/10/16, prints `ffffffff` for `-1`" body is
  `BigInteger.toString(int)` in `native-builtins/src/math_bignum.rs`, a different
  method. Sign handling is correct in both `Integer` copies. Cite the shadowing,
  not the `ffffffff`.

## 4. A by-name field read cannot report "absent" — and three readers disagree

The campaign rule was "`get_field_by_name` on an absent field answers `Int(0)`".
**In production it does not.** Source-verified, `vm/src/vm/vm_exec.rs`:

```rust
    fn get_field_by_name(&self, obj: ObjectRef, field_name: &str) -> Value {
        …
        if let Some(index) = resolve_field_index_in_hierarchy(class_id, field_name, &cm.class_store)
        {
            self.shared.mem.heap.get_field(obj, index)
        } else {
            Value::Object(None)
        }
    }
```

There are **three** implementations of the accessor and they do not agree:

| implementation | absent field answers |
|---|---|
| production, `vm/src/vm/vm_exec.rs` | `Value::Object(None)` |
| `native-api/src/test_mock.rs` | `Value::Object(None)` |
| `MockNativeContext`, `native-builtins/src/test_utils.rs` | **`Value::Int(0)`** |

`test_utils.rs` says why — *"Unknown names are treated as absent (returning
`Int(0)` rather than silently shadowing slot 0, which would corrupt slot-0 test
state)"* — which is a reasonable choice for that mock and a trap for anyone who
reads it as VM behaviour. A native unit-tested against `MockNativeContext` takes
a **different branch in production**, and several in-tree comments and records
assert the mock's convention as a VM fact (§9 lists the ones corrected).

**The real hazard is still there, with two different producers:**

1. **A present-but-unwritten reference slot decodes as `Int(0)`.** `Value::Object`
   carries a `NonNull` niche, so a zeroed slot reads back as discriminant 0. This
   is the documented, live defect class — `docs/feature-designs/by-name-field-reads.md`
   §1 and its semantics table — and its confirmed victim was a BouncyCastle
   `protected int rounds`. An `if let Value::Int(m)` arm swallows it and every
   fallback below is unreachable. The campaign's rule was right about the
   *consequence* and wrong about the *cause*.
2. **`Object(None)` for an absent field is indistinguishable from a real null.**
   This one is sharper than the version the campaign wrote down, because it
   silently inverts the layout discriminators built on it. A guard spelled
   `matches!(ctx.get_field_by_name(obj, "prevLookupClass"), Value::Object(_))`
   is intended to mean "the real JDK class declares this field". It matches
   `Object(None)` too, so **it takes the real-layout arm on the synthetic layout
   as well** — the exact case it was written to exclude.

**The remedy, and it also answers the descriptor question.** Ask the *class*, not
the value:

```rust
ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(obj), "<name>")
```

Both are on the `NativeContext` trait (`native-api/src/registry.rs`) with VM
impls in `vm/src/vm/vm_exec.rs`. This resolves the **declared** field, so it is
not fooled by a same-named field of another type — the defect
`by-name-field-reads.md` §6.2 specifies a descriptor-aware reader for. It is
already the house pattern at several sites; `native-builtins/src/lookup_define.rs`
uses it as a disjunct with the old value-shape test, and
`native-builtins/src/field_read.rs` (`slot_of`, `declares_field`, `int_field_strict`)
is the module that exists for this.

**One caveat the remedy does not remove**: by-name resolution returns the
**most-derived** declaration, so it is only safe where the name cannot be
shadowed by an application subclass. `by-name-field-reads.md`'s header records
what happened when `Enum.name()` was "fixed" this way. For a field declared by a
`java.lang.*` base class, prefer the known index.

## 5. A slot index against a real layout is not a wrong answer — it is heap corruption

The worked example, and the one to keep in your head. Real JDK 25
`java.lang.invoke.MethodHandles$Lookup`, instance fields in declaration order
(`javap -p`, quoted in `docs/known-issues/jdk-only/W6-3-slot-index-species-residuals.md`):

```text
  0 lookupClass            (ref)  private final Class<?>
  1 prevLookupClass        (ref)  private final Class<?>
  2 allowedModes           (int)  private final int
  3 cachedProtectionDomain (ref)  private volatile ProtectionDomain
```

CratonVM's synthetic layout is `0 lookupClass | 1 allowedModes | 2 previousLookupClass | 3 lookupMode`.
Writing the synthetic indices onto a real object puts an `Int` into two slots the
GC scans as oops. `native-builtins/src/lookup_define.rs` states the consequence
in place: *"both REFERENCES the GC scans as oops, so `Int(0x5F)` in either is a
bogus pointer for the collector to mark and move."* Slot 0 is `lookupClass` in
both layouts, which is why the shallowest probes never noticed.

**The pattern worth generalising is where the second instance was found.**
The success branch of `alloc_lookup_for` was fixed in wave 4. The *failure*
branch — the `if !modes_landed` re-assert — kept writing the synthetic indices
unconditionally, and was found in wave 7. The in-file comment names it exactly:
*"the same defect the block above was written to fix, reintroduced through the
failure branch — pinning only the positive half a second time."*

> **A fix that pins only the positive half hides what it unmasked.** After
> repairing a success path, read every `else`, every error branch, and every
> "re-assert if that did not land" fallback in the same function before calling
> it done. The fallback is precisely the code a green transcript never executes.

Both named instances are fixed in the tree. **Two residuals of the same species
were found while verifying this document and are not covered by any existing
record** — reported in §9.

Existing records, cross-referenced rather than restated:
`docs/known-issues/jdk-only/W4-4-slot-index-species-sweep.md` (the species and
the standing "resolve by name first, index as synthetic fallback" remedy),
`W6-3-slot-index-species-residuals.md` (the `javap` oracle, `java.nio.ByteOrder`
as a second instance), `W4-1-publiclookup-allowedmodes-never-checked.md` (the
Lookup case end to end), `jdk-only-object-layout-audit.md` (the site census).

## 6. Declaring a `CRATONVM_*` flag — four files, and it is bidirectional

**The canonical procedure is in the tree, not in `docs/`.** It is the doc comment
on `INVENTORY` in
[`types/src/flag_groups.rs`](../../types/src/flag_groups.rs), under the heading
*"Adding a `CRATONVM_*` flag: the four files, all of them"*. Read it there; it is
maintained next to the table it describes, and duplicating it here would only
create a second copy to go stale.

The two things worth knowing before you open it, because they change how you
plan the work:

* **The enforcement is `cargo test`, not `cargo check`.** `flag_groups.rs` says
  so itself: *"The enforcing tests are `cargo test` assertions, not compile
  errors, so `cargo build --all-targets` is green while any of these is
  missing."* Source-verified in `types/tests/flag_declaration_guard.rs`,
  `types/tests/flag_surface.rs` and `types/tests/flag_docs_generated.rs` — all
  `#[test]` + `assert!`. A green `cargo check --all-targets` is not evidence.
* **Declaration is bidirectional.** A literal with no row fails the guard, and a
  **row with no reader** fails check 5 of `tools/flag-census/check-surface.sh`
  (*"these keys are declared in INVENTORY but no Rust source reads them"* — the
  `CRATONVM_JIT_UNBAN_JUNITCORE` failure mode, where the last reader was deleted
  and the knob stayed in three generated docs). **Land the declaration and its
  consumer together.** Check 5 harvests only `on_key`/`off_key`, so `SCALARS`
  and group variables are outside it.

`types/src/flags.rs` needs no per-flag field: `VmFlags::legacy_var_os` serves
every declared name from one map.

## 7. The duplicate-registration census is scoped, not total

`native-builtins/tests/duplicate_registration_gate.rs` carries its own warning,
and it should be quoted rather than paraphrased:

> *"The census is **scoped, not total**. It observes exactly the registrations
> made by the registrars `vm_init_real_jdk_boot_path` calls, minus the ones
> `register` silently discards. A `0` here therefore means *'no duplicates among
> the registrars this test calls, under this process's flags'* — never *'no
> duplicates exist'*."*

Three consequences, all source-verified in that file:

1. **The synthetic-JDK registration graph is entirely unmeasured**, because the
   test replays the real-JDK arm (§2). A static scan of
   `native-builtins/src/lib.rs` *alone* finds **154** triples registered by both
   `register_essential_natives_with_shims` and `register_synthetic_overrides`;
   none can appear in the census. (My independent re-scan says 155 — a
   one-triple methodology difference, not a correction.)
2. **A dropped registration leaves no row at all.** Every drop arm in `register`
   returns before the push to `registrations`, so a discarded native is not a
   shadowed row, it is *no* row. Verified: the drop arms all `return` above the
   first push. One qualification — the `JdkOnly` refusal arm does push a
   `JdkOnlyViolation::SyntheticNativeRegistered` to `refused` first. That is a
   separate violation log; for the census the statement holds.
3. **`BASELINE_SHADOWED = 0` is a seed, not a measurement.** The file is explicit:
   *"this constant is `0` and the gate is therefore RED until the first run
   pastes the real number in."* Same for `BASELINE_KIND_DISAGREEMENTS`. And
   `duplicate_registration_gate` appears **nowhere in `.github/workflows/ci.yml`**
   — `stub_ratchet` and `bridge-ratchet.sh` are wired, this is not. So the seed
   has never been taken by CI either.

The same scoping trap has a sibling with teeth: **`regression-suite/bridge-ratchet.sh`
takes its census in Compatible mode** (`--real-jdk`; the frozen artefact
`scripts/baselines/jdk-only-bridge-ratchet.json` records `"mode": "compatible"`).
A registration reachable only from `register_synthetic_overrides` therefore
**cannot move that ratchet by any amount**. See §9 for the record this corrects.

**Before quoting any census number, answer three questions:** which registrars
did it call, which mode was it taken in, and what did `register` drop before it
could be counted.

## 8. Measurement discipline

Four rules this campaign paid for more than once. Each already has a home in the
tree; this section exists so they can be found from one place.

* **HotSpot is the oracle; the other CratonVM mode is not.** A vector that fails
  on HotSpot is a broken vector, not a VM defect —
  `docs/known-issues/jdk-only/L10-rjdkprocess-vector-overassertion.md` is the
  worked instance (*"There is no CratonVM bug here to fix"*; every prior
  `RJdkProcess` result void). The three-arm protocol and the rule that a broken
  control voids the measurement are in `scripts/jdk-only-strict-probes.sh`
  (*"ERROR: the HotSpot CONTROL exited … Nothing below is a measurement"*), and
  the no-golden-values policy is in `regression-suite/README.md`. **A
  "HotSpot fails it too" verdict is only as good as the invocation** — the
  campaign's own `RJdkModule` write-off was reversed because the ad-hoc HotSpot
  arm passed the wrong module name.
* **A suite that only asserts the positive cannot see an indiscriminate
  implementation.** `SSLContext.getInstance` accepted every protocol string and
  the entire positive half of the vector passed
  (`docs/known-issues/jdk-only/W3-7-sslcontext-bogus-protocol.md`); the shape is
  named in `W4-1-publiclookup-allowedmodes-never-checked.md`, one level worse —
  a suite asserting *both* polarities of a function nothing calls. The general
  taxonomy of tests that read green while measuring nothing is
  `W6-5-vacuous-tests.md`.
* **Fixed wall-clock bounds and fixed line-bands are latent flakes.** The rule is
  written in the differential-harness design:
  *"Do not add fixed wall-clock bounds to any check. Both directions flake: an
  upper bound fails under contention, and a lower bound can pass while measuring
  nothing."* Line-bands rot the same way and have already done so in this
  campaign — `jdk-only-census-one-class-one-platform-FIXED-20260810.md`
  records its own line numbers as stale against `dev`, and
  `STRICT-CORPUS-CAMPAIGN-20260807.md`'s
  `run.sh` citations are stale as written (§9). Source-witness tests that scan a
  fixed window (`vm/tests/t11_safety_conformance.rs`'s five-line `// SAFETY:`
  window, `vm-cli/tests/no_diag_eprintln.rs`'s four-line window) are the same
  hazard with a test around it.
* **A vector can exist, compile, and never execute.** `regression-suite/run.sh`
  compiles with a glob (`"$JAVAC" … "$HERE"/src/*.java`) and runs a
  hand-maintained word list (`CORE_CLASSES`, `JDKONLY_CLASSES`). `run.sh` says
  what that cost: *"a `src/*.java` vector named in no list … looks like coverage
  and is not. This is how RJdkPhaser — 240 checks — arrived inert."*
  Source-verified today: 57 `src/*.java`, 31 + 23 + 3 names across the three
  lists, every file in exactly one list — but a **default** invocation runs 31 of
  57, because `JDKONLY_CLASSES` is scheduled only when `CRATONVM_ARGS` names
  `--jdk-only` or `SUITE` is `jdk-only`/`all`, and the coverage guard is off
  unless `STRICT_COVERAGE=1`. The same failure mode reaches fixtures: a test
  whose fixture is missing returns green in 0.00 s unless
  `CRATONVM_REQUIRE_E2E` is set, and **no workflow sets it** (§9).

---

## 9. What this document corrected, and what it could not

Corrections landed in `docs/` by this lane, each verified against the tree
before editing:

| Record | Was | Now |
|---|---|---|
| `known-issues/jdk-only/W6-1-varhandle-vartype-coordinatetypes.md` | bridge-ratchet "must be re-frozen", counters "each rise by 2" | the registrar is synthetic-only, the ratchet is taken in Compatible mode, the counters move by 0 |
| `known-issues/jdk-only/W4-1-publiclookup-allowedmodes-never-checked.md` | "an absent field answers `Int(0)`" | `Object(None)`; the discriminator built on it is inverted (§4) |
| `known-issues/jdk-only/W6-3-slot-index-species-residuals.md` | "the `Int(0)`-for-absent convention" | same correction |
| `known-issues/jdk-only/W3-2-non-nestmate-hidden-class-nest-host.md` | non-zero flag test justified by `Int(0)`-for-absent | the justification is wrong; the guard is still correct for a different reason |
| `known-issues/jdk-only/W4-4-slot-index-species-sweep.md` | `Intrinsic` = "the one kind allowed to shadow bytecode" | §1 |
| `known-issues/jdk-only/L13-arrayitr-remove-writethrough.md` | same overstatement | §1 |
| `known-issues/jdk-only/W6-4-duplicate-registration-gate.md` | gate "ships"; StampedLock served by two implementations | not in CI, baselines unseeded; the losing StampedLock registrar is disabled at its call site (see `W6-12`) |
| `known-issues/jdk-only/W6-5-vacuous-tests.md` | `require_fixture` "not done here"; `probes/FjpProbe.java` as the durable home | it exists; that path does not |
| `known-issues/jdk-only/W6-12-stampedlock-split-brain.md` | "no regression-suite vector covers `Phaser`" | `RJdkPhaser` exists and is scheduled — but is untracked |
| `STRICT-CORPUS-CAMPAIGN-20260807.md` | "+2" propagated; `run.sh` line citations; "21 vectors" | corrected in place |
| `README.md` | cites `probes/ExecProbe.java` | `apps/executor_probe/ExecProbe.java` (tracked) |

Separately, seven records in `known-issues/jdk-only/` (`L5`, `L8` ×2, `W4-2`,
`W5-4`, `W6-5` ×4, `W6-8`) cited internal records by their path inside the
internal archive, which
`types/tests/doc_citation_paths.rs::no_source_file_links_into_docs_internal`
forbids: those records are not published, so the path is a link no public reader
can follow. Each now cites the record by its path **relative to the internal
tree's own root**, which is the form that test prescribes. No finding was
dropped — where a citation was the only support for a claim, the claim is now
stated inline as well (`W6-5` §3.1).

**A correction to this lane's own first draft, kept as an instance of §8.** An
earlier version of the `W6-5` note asserted that `class_cv_args` gives
`RPriorityQueueGc` only `--nojit`. It does not: `regression-suite/run.sh` gives
it `--nojit --Xmx 64m`, and `RTreeRangeGc` `--Xmx 64m` without `--nojit`. The
claim came from a sub-agent's read of a file that was being edited concurrently,
and it survived one review before a direct re-read caught it. **Re-read the file
yourself before writing down a negative finding about it** — especially in a
tree where other lanes are landing changes while you write.

**Could not be corrected — outside this lane's ownership (Rust, tests, CI):**

* `vm/tests/rbigdec1_arithmetic.rs` — *"so the gate is now live in CI."* It is
  not, three times over: the test opens with `None => return` (a silent skip);
  the fixture `apps/probes/BdProbe.java` is **untracked** (`?? apps/probes/BdProbe.java`),
  so a fresh clone does not have it, and the alternative path
  `apps/bigdecimal_probe/BdProbe.java` does not exist; and `require_fixture` only
  panics under `CRATONVM_REQUIRE_E2E`, which no workflow sets. The adjacent
  claim in the same file that *"`probes/` IS tracked"* confuses the tracked
  directory with the untracked file.
* `vm/tests/common/mod.rs` — *"CI sets it to assert that a green run was a real
  one."* `CRATONVM_REQUIRE_E2E` appears in no workflow file.
* `native-builtins/src/lookup_define.rs` and `native-builtins/src/classloader.rs`
  — comments asserting *"an ABSENT field answers `Int(0)` from
  `get_field_by_name`"* as the rationale for a discriminator. The discriminators
  are now correct (the class-side witness was added as a disjunct); the stated
  reason is not, and the next person to "simplify" them back to the value-shape
  test will be doing it on a false premise.
* ~~**Two un-recorded §5 residuals**~~ — **BOTH RESOLVED. Row re-run and
  corrected by the lane that owns
  `known-issues/jdk-only/W7-13-strict-mh-insert-wrapper.md`; kept here because
  what it demonstrates is this section's own rule.** The row read: *"`lang_invoke.rs::lk_write_allowed_modes`
  still does a bare `ctx.set_field(obj, 1, Value::Int(modes))` on its error
  branch with no class-side witness … and `classloader.rs::lk_previous_lookup_class`
  reads slot 2 unconditionally and hands an `Int` back from a
  `()Ljava/lang/Class;` native."* Neither half is true of the tree. Both
  functions now open on the CLASS-side witness
  (`resolve_field_index_by_class_id(..., "allowedModes")` /
  `(..., "prevLookupClass")`), and the fixed-index arm each keeps is reachable
  only after that witness has said the receiver does NOT carry the real
  `MethodHandles$Lookup` layout — i.e. on the fabricated 2-field stub, where the
  fixed index is the right one. `lk_previous_lookup_class` additionally coerces a
  non-reference read to `Value::Object(None)`, so the `()Ljava/lang/Class;`
  descriptor can no longer return an `Int` on any layout. **The rule this row now
  illustrates: an audit row can be stale while its neighbours are live, and the
  two halves of one bullet can rot independently — run every row, and re-read the
  function rather than the citation.** (The source-only audit that wrote this row
  was honest about being source-only. The `lk_write_allowed_modes` half was
  already repaired when it was written, and the `lk_previous_lookup_class`
  half was repaired at some point after,
  carrying a doc comment that recites the old defect as the thing it replaced. No
  commit is cited for the second because this lane ran no `git` command; the
  evidence is the code.)
* `regression-suite/jdk-only-coverage.txt` and `regression-suite/README.md`
  state that `run.sh` *"prints a SKIP line with the reason"* for unscheduled
  `--jdk-only` vectors. `run.sh` has no such code.
* `apps/probes/BdProbe.java`, `regression-suite/src/RJdkPhaser.java` and
  `regression-suite/src/RJdkFieldModule.java` are **untracked** while `run.sh`
  schedules the latter two. A fresh clone schedules classes whose sources do not
  exist. `apps/` is `.gitignore`d with fixtures force-added, which is how this
  keeps happening.
