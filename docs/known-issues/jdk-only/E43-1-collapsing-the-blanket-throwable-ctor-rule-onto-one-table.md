# The blanket throwable-`<init>` rule was at five sites, not four; collapsing the three in `lib.rs` onto E34's table

> **Status: FIXED (registrar side), PREDICTED.**
> Every HotSpot number is **MEASURED** on Microsoft OpenJDK 25.0.3+9-LTS.
> Every CratonVM number is **DERIVED FROM SOURCE** or **PREDICTED** — this lane
> may not build and may not run the VM.
>
> Files changed: `native-builtins/src/lib.rs` and this record.
> Nominations for four files this lane does not own are in §7.

Lane E43, 2026-08-13. Answers E34-1 §9 **N2** and **N3**, which had to move
together, and reports a fifth site E34-1 did not find.

---

## 1. In one line

E34 replaced site 1 with a per-class `<init>` table derived from the JDK by
reflection; three *other* copies of the blanket-four rule in
`native-builtins/src/lib.rs` ran **later**, and `register` is last-write-wins,
so the measured table was being buried for ~52 classes and 85 descriptors that
JDK 25 does not declare stayed registered. All three are now gone or reduced to
their measured residue.

| | before | after |
|---|---:|---:|
| `register()` calls for throwable `<init>` in the whole tree | **531** | **178** |
| distinct `(class, "<init>", descriptor)` triples registered | **258** | **173** |
| of those, triples the real JDK 25 class **does not declare** | **85** | **0** |
| duplicate (shadowed) throwable-ctor registrations | **268** | **0** |

Both totals are **DERIVED FROM SOURCE** — they are the union/multiset of the
registrar tables read out of the working tree, not a census of a running VM.

---

## 2. The five sites, and which run when

`register` has no unregister API, so the *last* registrar of a triple owns it.

| # | site | domain | order | this lane |
|---|---|---|---|---|
| 1 | `lang_misc.rs::register_throwable_subclass_natives` | 62-class JDK-derived table | `register_essential_natives_with_shims`, line ~8618 | untouched |
| 5 | `lib.rs`, same function, ~300 lines **earlier** (line ~8326) | blanket **two** (`()V`, `(String)V`) over 14 classes | **before** site 1 | reduced 28 → 2 |
| 3 | `lib.rs::register_synthetic_overrides` | blanket four over 30 classes | after site 1 (synthetic mode) | **deleted** |
| 4 | `lib.rs::register_exception_extras_natives` | blanket four over 53 classes | after site 1, in **both** modes | reduced 212 → 5 |
| 2 | `class_manager.rs::synthetic_stub_ctor_methods` | the stub-declaration mirror | class-load time | untouched (not this lane's) |

Site 4's ordering is worth stating precisely, because E34-1 §7 asserted it
without showing the path:

* **synthetic-JDK mode** — `register_builtins` = `register_essential_natives`
  (→ site 1) *then* `register_synthetic_overrides` (→ site 3, and → site 4 both
  directly and via `register_annotation_overrides`).
* **real-JDK mode** — `register_essential_natives_with_shims` reaches site 1 at
  line ~8618 and then `register_annotation_overrides` at line ~19243, whose body
  calls `register_exception_extras_natives`. Same order, no feature gate.

So site 4 won the slot in **both** shipping modes, which is why E34's table was
data with no consumer for every class the two lists shared.

### 2.1 One correction to E34-1's numbers

E34-1 §7 records site 4's list as **57 classes**. A source parse of the array
(`^\s+"…",$` inside the `let exceptions = [ … ];` region, so the class names
quoted inside its two long "deliberately NOT in this list" comments are not
counted) gives **53**. E34-1 appears to have counted the two commented-out names
and two more quoted strings from those comment bodies — the same over-count
shape E34-1 §2.1 caught in E23-1's "69 entries".

---

## 3. The rule, applied without simplifying it

> register a descriptor iff the class **DECLARES** it **AND** (it is public
> **OR** it was already registered)

`scratchpad/e43/Removals43.java` takes the 89 distinct `(class, descriptor)`
pairs that sites 3, 4 and 5 register and E34's table does not carry, reflects
over each on the running JDK, and reports declaration + visibility.
**MEASURED**, JDK 25.0.3+9-LTS, `rows=89`:

```
rows=89  NOT-DECLARED(safe to remove)=85  DECLARED(must keep)=4  unresolvable=0
--- DECLARED rows (these must NOT be removed) ---
java/lang/VirtualMachineError  ()V                                        public
java/lang/VirtualMachineError  (Ljava/lang/String;)V                      public
java/lang/VirtualMachineError  (Ljava/lang/String;Ljava/lang/Throwable;)V public
java/lang/VirtualMachineError  (Ljava/lang/Throwable;)V                   public
```

The rule's second conjunct (public **or** already-registered) never had to fire
as a tie-breaker here: everything that survives the first conjunct is public.
The six non-public descriptors E34 retains — `AssertionError(String)` private,
`CompletionException()`/`(String)`, `ExecutionException()`/`(String)`,
`InvocationTargetException()` protected — are all rows of the table, i.e. they
are site 1's and this lane does not touch them.

**`AssertionError(String)` stays, and is not dead.** Re-verified here by
disassembly rather than by reading modifiers, because E23-1 called it dead and
that was measured false:

```
$ javap -p -c java.lang.AssertionError
  public java.lang.AssertionError(java.lang.Object);
       2: invokestatic  #10  // String.valueOf:(Ljava/lang/Object;)Ljava/lang/String;
       5: invokespecial #16  // "<init>":(Ljava/lang/String;)V   <-- the private one
```

Site 3 and site 4 both registered `AssertionError.<init>(Ljava/lang/String;)V`
and both were deleted here; the descriptor is still registered, by the table,
with the same body. Nothing about that path changed.

### 3.1 The 85 removals, and why no `javac` could have called any of them

One sentence covers all 85, and it is the strongest form the sentence can take:
**`Class.getDeclaredConstructors()` on JDK 25 does not contain them.** A
`javac`-emitted `invokespecial C.<init>:D` requires `C` to declare `D` at
compile time and resolution requires it at run time; a descriptor no version of
the class ever declared cannot be the target of a compiled call site, so these
registrations could only ever have been reached by a hand-written or generated
class file that would fail verification against the real JDK anyway.

Grouped by shape (all **MEASURED**):

| n | dropped | classes |
|--:|---|---|
| 29 | `(String,Throwable)`, `(Throwable)` | ArithmeticException, ArrayIndexOutOfBoundsException, BrokenBarrierException, CancellationException, ClassCastException, CloneNotSupportedException, EOFException, FileNotFoundException, IllegalAccessException, InaccessibleObjectException, IndexOutOfBoundsException, InputMismatchException, InstantiationException, InterruptedException, MalformedURLException, NegativeArraySizeException, NoClassDefFoundError, NoSuchFieldException, NoSuchMethodException, NotSerializableException, NullPointerException, NumberFormatException, OutOfMemoryError, StackOverflowError, StringIndexOutOfBoundsException, TimeoutException, UnknownHostException, UnsupportedEncodingException, VerifyError |
| 3 | **all four** | MissingResourceException, ParseException, UncheckedIOException |
| 3 | `(Throwable)` | AssertionError, ClassNotFoundException, LinkageError |
| 2 | `()`, `(String)`, `(Throwable)` | MatchException, TypeNotPresentException |
| 1 | `(String)`, `(String,Throwable)`, `(Throwable)` | FormatterClosedException |
| 1 | `(String)`, `(String,Throwable)` | InvocationTargetException |
| 1 | `(String,Throwable)` | ExceptionInInitializerError |

The three "all four" rows are the sharp ones: `UncheckedIOException`,
`ParseException` and `MissingResourceException` were each advertising four
constructors while declaring **none** of them. Their real constructors
(`(IOException)`, `(String,IOException)`, `(String,int)`,
`(String,String,String)`) are the ones E34 added to the table.

### 3.2 Why the removals only bite now

E34-1 §5 established the asymmetry and it is what bounds this change:
`vm_exec.rs`'s final native-registry fallback probes `(class, name, descriptor)`
up the dispatch and receiver chains **without requiring the class to declare the
method**. So E34's *additions* already worked — the 88 fixtures were fixed by
site 1 alone. A *removal* only takes effect when both the registry and the stub
declaration drop it, which E34 did for site 1 and site 2. This lane removes the
registrations that sites 3/4/5 were keeping alive behind E34's back. Nothing
here re-opens a call site; it closes 85 that the stub had already stopped
declaring.

---

## 4. What each site became

### Site 3 — `register_synthetic_overrides`: deleted

All 30 of its class names are rows of the table (**MEASURED**: the set
difference is empty), and site 1 runs first on that path, so the loop was 120
registrations of which 82 restated the table and 38 were among the 85. Deleting
it leaves no descriptor unregistered.

Two behaviour changes fall out, and they are improvements rather than
bookkeeping:

* `java/lang/AssertionError` — the (Object) overload and the six primitive
  overloads that E34 added were **not** in site 3's blanket, so they were
  already answered by the table. What changes is that the four blanket
  descriptors are no longer re-bound over it.
* `java/lang/reflect/InvocationTargetException.<init>()V` and
  `(Ljava/lang/Throwable;)V` — site 3 (and site 4) were overwriting these with
  the *generic* message/cause bodies, so the wrapped throwable landed in
  `cause`, not the JDK's `target` field. The table's
  `native_invocation_target_exception_init_target` now wins. `target` **is** a
  declared field on the synthetic stub (`class_manager.rs::synthetic_stub_fields`,
  slot 6), so the `set_field_by_name` write lands in synthetic mode too; and
  `native_invocation_target_exception_get_target` already falls back to `cause`,
  so neither shape regresses.

### Site 4 — `register_exception_extras_natives`: constructors out, bridges kept

The 53-name list stays, because it is also a `getMessage()`/`toString()` bridge
list and those bridges are **not** the rule this lane is collapsing. Their two
"deliberately NOT in this list" comments (`PatternSyntaxException`,
`InvalidClassException`) are about the *getMessage* override, so they stay with
the list they actually explain. Deleting them would have re-opened
`a-bridge-in-front-of-an-overridden-getmessage-FIXED-20260805.md`.

Two constructor blocks remain, both named rather than looped:

1. **`java/lang/VirtualMachineError`**, the one name in the list outside the
   table's measured 62. It keeps its historical bodies byte-for-byte, so this is
   a no-op for it, and it also happens to be a class for which the blanket four
   were *right* — `javap -p` shows all four declared and public. It is abstract,
   so no `new` reaches them directly; the reachable path is a subclass
   constructor's `super(...)`, an `invokespecial` on this exact triple.
2. **`java/lang/NullPointerException.<init>(Ljava/lang/String;)V`**, which is
   the ctor-body diff E34-1 N3 asked for, resolved.

### 4.1 The ctor-body diff N3 asked for

| pair | verdict |
|---|---|
| `native_exception_init_empty` vs `lang_misc::native_exc_init_noargs` | **identical** — `write_throwable_cause(this, this)` then `capture_throwable_trace(this)` |
| `native_exception_init_msg` vs `lang_misc::native_exc_init_message` | identical **plus** the opt-in `CRATONVM_DBG_NPE_TRACE` surefire forensic, which fires only for `NullPointerException` whose message is exactly `"Name is null"` |
| `(String,Throwable)V`, `(Throwable)V` | site 4 already delegated to `lang_misc`'s — not twins at all |

So there was no loser to delete: one of the four pairs was a strict superset of
its twin, for exactly one class. Rather than delete a diagnostic while claiming
to remove a duplicate, the superset body is re-registered by name on the single
triple it can affect. `native_exception_init_empty` and
`native_exception_init_msg` both retain other callers
(`UnsatisfiedLinkError`/`ExceptionInInitializerError` earlier in
`register_essential_natives_with_shims`, `phases_late/beans_jndi.rs`,
`serialization.rs`), so nothing became dead code.

### Site 5 — the fifth copy E34-1 did not find

`register_essential_natives_with_shims` contains a **blanket two** — `()V` and
`(Ljava/lang/String;)V` over 14 class names, labelled "RKC16N-RECON" — about
300 lines *above* its own call to `register_throwable_subclass_natives`, with no
early return in between.

**MEASURED**: all 28 of those descriptors are declared and public on JDK 25, so
this copy was never *wrong*; it was **dead**. Thirteen of the fourteen classes
are table rows carrying both descriptors bound to these same two bodies, so 26
of the 28 registrations were overwritten by an identical pair a few hundred
lines later, on every boot, in both modes. They are deleted.

`java/lang/IllegalThreadStateException` is the fourteenth name and is **not** in
the table, so its pair is the one that had to stay; `javap -p` confirms it
declares `()V` and `(Ljava/lang/String;)V`, both public, so it satisfies the
same rule.

---

## 5. App throwables outside the measured set: what preserved them

Nothing in this change narrows the fallback, and there are three independent
reasons, none of which this lane touched:

1. **The stub declaration side is untouched.**
   `class_manager.rs::synthetic_stub_ctor_methods` still declares the historical
   four for any name ending in `Exception`/`Error` that the table does not
   carry — that is E34's deliberate `[flag≠mode drops it]` guard and it is not
   this lane's file.
2. **Sites 3, 4 and 5 never listed an application class.** All three were fixed
   lists of `java/...` names. A `…/DbException` was never registered by any of
   them, so deleting them cannot have removed its answer.
3. **`vm_exec.rs`'s registry fallback still resolves it.** `DbException extends
   RuntimeException`, and `java/lang/RuntimeException` keeps all four
   descriptors in the table (it really declares all four). The fallback walks
   the dispatch chain without requiring the class to declare the method, so
   `DbException.<init>(Ljava/lang/String;)V` still lands on
   `native_exc_init_message` exactly as before.

The one JDK class in this position — `VirtualMachineError`, outside the measured
62 — kept its blanket four for the same reason, with its historical bodies
intact (§4).

---

## 6. Anti-drift: what replaced what

E34's source witness `throwable_ctor_table_matches_the_stub_declarations`
(`classloading/src/class_manager.rs`) compares the **two table copies**
(`lang_misc.rs` ↔ `class_manager.rs`). This lane removed neither copy, so that
witness still has both sides to compare and is untouched. The `#[rustfmt::skip]`
one-row-per-class shape and the `Gen34.java` regeneration header are likewise
untouched.

What this lane adds is a witness for the failure mode it just fixed, which the
existing one cannot see: a *sixth* copy of the rule appearing next to one of the
class lists that remain in `lib.rs`.

`the_blanket_four_ctor_loops_stay_collapsed_onto_the_table`
(`native-builtins/src/lib.rs`, bottom) is a source witness with six assertions:

1. the table in `lang_misc.rs` parses to **≥ 60 rows** (else it is comparing
   against nothing);
2. `register_synthetic_overrides`' body no longer contains the site-3 loop
   binder — and the extracted body must be **> 50 000 bytes**, so a broken body
   scan fails instead of passing on an empty string;
3. site 4's `for exc in &exceptions` loop body contains **no `<init>` literal**;
4. **every class in site 4's list is a row of the table** or is on a named
   allow-list — this is the assertion that makes the two remaining lists of
   throwable class names unable to drift apart again, which is the actual defect
   E34-1 §7 described;
5. the allow-list cannot be padded: each entry must really be registered there,
   **and** must really be absent from the table, so the exemption expires
   automatically the moment a regeneration absorbs `VirtualMachineError`;
6. the number of `<init>` literals in site 4 is pinned at
   `4 × allow-list + 1`.

It is a **source** witness rather than a registry assertion on purpose: the two
boot paths that reach these sites live in different feature configurations
(`register_synthetic_overrides` is `#[cfg(feature = "synthetic-jdk")]`; the
real-JDK path goes through `register_annotation_overrides`), and a
`#[cfg(feature = …)]` test guards only the configuration it compiles into — the
`[synjdk mod rots]` / `[2cfgs]` shape. A source scan guards both, needs no VM
boot, and its six assertions were **executed** against the post-edit working
tree by re-implementing them line-for-line in Python (`scratchpad/e43/`, all six
pass). That is not a substitute for `cargo test`; it is evidence that the
witness is not vacuous and does not fail on the tree it ships with.

**Formatting:** `rustfmt --check --edition 2021 native-builtins/src/lib.rs`
reported 77 diffs inside `lib.rs` before this change and reports **75** after —
the two my new module introduced are fixed, and no pre-existing one was
disturbed. (The crate-wide figure is 1 771 across all of `native-builtins/src`,
which the CI `fmt` job will report for any change to this file. That is
pre-existing and is `[fmt blocks CI]`, not this change.)

---

## 7. Nominations

### N1 — `classloading/src/lib.rs`: E34-1 N5's one-line export — **not** a blocker for this lane

E34-1 N5 asks for

*old, verbatim* (`classloading/src/lib.rs`, line 78):

```
pub use class_manager::is_bootstrap_appended_class;
```

*new:*

```
pub use class_manager::is_bootstrap_appended_class;
// E34-1 §6: the Throwable `<init>` descriptor table, so
// `native-builtins/src/lang_misc.rs` can consume it instead of keeping a second
// copy guarded by a source-witness test.
pub use class_manager::jdk_throwable_ctor_descriptors;
```

(plus `fn jdk_throwable_ctor_descriptors` → `pub fn` in `class_manager.rs`).

**Stated explicitly because the task asked:** this lane's work does **not**
depend on that export landing. E43 removes *duplicate registrars*, not table
copies. `lib.rs` never held a copy of the table and does not need to read one —
its two remaining sites are now defined by *not* registering constructors at
all, plus five named triples. If N5 lands, the E34 witness in `class_manager.rs`
can be deleted and the E43 witness in `lib.rs` stays exactly as it is, because
it reads the table wherever the marker comment is. If N5 never lands, nothing
here degrades.

### N2 — `native-builtins/tests/duplicate_registration_gate.rs`: re-seed both baselines downward

This change removes **268 shadowed (duplicate) throwable-ctor registrations**
from the boot registry: 26 at site 5 and 242 across sites 3/4 whose triples had
an earlier registrant. `BASELINE_SHADOWED_*` are one-way ratchets whose
assertion is `<=`, so **the gate will not go red** — but the file's own rule is
"removing a duplicate is welcome and requires lowering the number in the same
change to lock the improvement in", and this lane may not run cargo.

The exact new numbers must come from a run, not from me. The starting points:

```
const BASELINE_SHADOWED_MANAGEMENT: Option<usize> = Some(1201);
const BASELINE_SHADOWED_NO_MANAGEMENT: Option<usize> = Some(1150);
```

**PREDICTED**, and to be replaced by whatever the run prints: both drop by the
number of removed duplicates that the *real-JDK* boot path
(`vm_init_real_jdk_boot_path`) actually reaches. Sites 3 is synthetic-only, so
the real-JDK path sees site 5's 26 plus site 4's overlap with site 1 (126 of its
212 rows restated table entries, and 4 of those 126 had a third registrant
earlier still). The honest statement is: **expect a drop of order 150, do not
paste a guess**, and note that `BASELINE_KIND_DISAGREEMENTS_*` (51/51) will also
move, because site 4 registered under the caller's ambient category while site 1
registers `Bridge`.

### N3 — `docs/known-issues/jdk-only/E34-1-*.md` §7: two corrections

* site 4's list is **53** classes, not 57 (§2.1 above);
* the rule is implemented at **five** sites, not four — §7's table is missing
  the blanket-two in `register_essential_natives_with_shims` (§4, site 5). It
  was harmless but it was 26 dead registrations and it is the same shape, so a
  reader following that table to "find all the copies" would have missed one.

Also worth adding to §7: the claim "sites 3 and 4 keep re-registering four
descriptors on ~55 of my 62 classes" is now measured exactly — 82 + 126 = 208
restatements over 52 distinct classes, and **85** distinct descriptors of E34's
97 removals were still live in the registry until this change.

### N4 — `scripts/jdk-baseline/classes.txt`: add `java/lang/VirtualMachineError`

E34-1 N1 nominates moving the 62-class table into the baseline mechanism. When
that happens, `java/lang/VirtualMachineError` should go in with it: it is the
only throwable this crate now registers constructors for by hand, its four
descriptors are **MEASURED** public on JDK 25, and the E43 witness's allow-list
is written to fail the moment the table absorbs it — so absorbing it is the
designed exit, not a conflict.

### N5 — a runtime companion to the E43 witness

The witness in §6 is a source scan. The strictly stronger form is a registry
assertion — build `register_essential_natives` (+ `register_synthetic_overrides`
under the feature) and assert that for every class in the table, the set of
registered `<init>` descriptors is **exactly** the table's set. This lane did
not write it because it cannot run `cargo test` and so cannot tell a real
divergence from an unknown sixth registrar in another module; shipping a test
whose first run might be red is worse than shipping the scan. Whoever can run
the suite should add it, in both feature configurations.

---

## 8. What is predicted to change

* **Synthetic-JDK mode.** 85 descriptors that HotSpot also refuses stop being
  registered, matching HotSpot. `InvocationTargetException(Throwable)` starts
  storing into `target` instead of `cause` (§4). Everything E34 added stays
  added — this lane registers nothing new except `VirtualMachineError`'s
  historical four, which it also had before.
* **Real-JDK mode.** Same 85 removals, and they are inert here for the same
  reason E34's were: the methods do not exist, so nothing could resolve to them.
  `AssertionError.<init>(Ljava/lang/Object;)V` remains the one real-JDK
  shadowing E34 introduced; this lane does not add another.
* **Nothing gains a registration it did not have**, with one exception of zero
  width: `NullPointerException.<init>(Ljava/lang/String;)V` is registered once
  more, to the body that already won that slot.

### The measurement to demand of whoever can run it

1. In synthetic-JDK mode, a fixture whose failure path is
   `throw new AssertionError(msg)` must still print `msg` — E34-1's own success
   criterion. If it now prints the `NoSuchMethodError` again, this lane deleted
   a registration the table does not actually carry, and the class to diff is
   `AssertionError`.
2. `new UncheckedIOException(new IOException("x"))` must construct; the four
   descriptors it used to advertise must now raise `NoSuchMethodError`, which is
   what HotSpot does.
3. A reflective `InvocationTargetException` round-trip must report
   `getTargetException() == getCause() ==` the wrapped throwable in **both**
   modes — that is the one behaviour this change flips rather than removes.
4. `duplicate_registration_gate` must report a *lower* shadowed count; N2 asks
   for the printed number, not a guess.

---

## 9. Reproducing

```
cd scratchpad/e43
javac -d . Removals43.java
java -cp . Removals43 removals.txt     # the 89 -> 85 / 4 adjudication
javap -p java.lang.VirtualMachineError # the four public ctors kept
javap -p -c java.lang.AssertionError   # the private (String) ctor, still reached
```

`removals.txt` is generated from the working tree (the three class lists parsed
out of `lib.rs`, differenced against the table parsed out of `lang_misc.rs`), so
the oracle can never be measuring a different set than the one that ships.

### What this proves and what it does not

It proves the **data**: of the 89 `(class, descriptor)` pairs that the three
`lib.rs` sites registered beyond E34's table, exactly 85 are constructors JDK 25
does not declare and exactly 4 are real, all four on one class, all four public
— so the retained residue is the complete residue and the removals are the
complete removals. It proves the **witness is non-vacuous**, by executing its
six assertions against the post-edit tree.

It proves **nothing** about a CratonVM binary — none carrying these changes
exists. The Rust is unrun and uncompiled by this lane. Every "after" in §8 is
predicted from the registration order read in the source.
