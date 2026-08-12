# W7-62 — three stale ratchets, six tests guarding nothing, and a fix that widened its own residual

> **2026-08-12, MEASURED — the stub ratchet now FIRES, and it is deliberately
> left firing.** First actual execution of
> `cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture`
> after the multi-lane campaign. It prints:
>
> ```
> stub-ratchet: const BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT: usize = 1261;
> ... exceeding the frozen baseline of 1253 ... 8 passed; 1 failed
> ```
>
> **1253 → 1261, i.e. +8, of which only +2 is attributed.** The +2 is the pair
> of `Cipher.getMaxAllowedKeyLength` / `getMaxAllowedParameterSpec` natives added
> to repair `RCrypto`; they sit inside `register_cipher_clinit_shim`'s
> `SyntheticStub` window and were deliberately left that kind so strict
> no-stubs mode drops that family together, matching the sibling `isRestricted`.
> **CORRECTION, same day — the +6 IS attributed, and this block was wrong.**
> `stub_ratchet.rs:466-474` names them: six `java/util/logging` registrations
> that became `SyntheticStub` by being added to `RETIRED_SHADOW_TRIPLES`
> (`retired_shadow.rs:239-240`, `:258-261`). That is a RETIREMENT — a Bridge
> re-tagged so strict mode runs the real bytecode — not a new fake, which is
> the thing the gate exists to catch. **The test PREDICTS 1259**; the
> arithmetic closes exactly: 1253 + 6 retirements + 2 Cipher natives = 1261.
> The error below was measuring the drift from the STALE frozen 1253 instead of
> from the test's own prediction. Corroborated independently: five JUL triples
> are the only rows in the registry where a stub owns the slot in Compatible
> and a surviving intrinsic owns it in strict — the fingerprint of a retirement
> mid-flight. Note also that this ratchet is a RUNTIME count and cannot be
> recomputed by scanning source. A re-freeze to 1261 is therefore justified
> **with that attribution written next to it**, which is what the gate asked
> for all along.
>
> ~~**The remaining +6 has no owner.**~~ Two candidates were checked and REFUTED:
> `register_datagram_channel` sets `Bridge` (native-io/src/lib.rs), so the ten
> new `DatagramSocketAdaptor` registrations do not count here; and the new
> `Selector.provider` / `Stream.forEachOrdered` registrations are ambient
> `Bridge` too.
>
> **Not re-frozen, on this record's own rule**: a firing gate is loud, a wrongly
> frozen one is silent forever, and the gate's own failure text says *"Make the
> new native a real Bridge/Intrinsic instead of a fake — do NOT just raise the
> baseline."* Re-freezing to 1261 without naming those six would convert a real
> signal into a permanent lie. `--dump-native-registry` produced no output when
> tried, so the per-row diff that would settle it is itself an open instrument
> question. Next lane: get a per-row `SyntheticStub` census out of a built
> binary, diff it against the control binary
> (`scratchpad/bin/cratonvm-control-44044c7e2.exe`), and name the six.

**Status: SOURCE COMPLETE 2026-08-12, NOTHING BUILT OR RUN.** No `cargo`
command and no CratonVM invocation happened in this session. The one thing that
was executed is `probes/ListItrInterfaceProbe.java` on HotSpot 25.0.3+9
(Windows x64), as the control for §3; that needs no CratonVM build. Everything
else is source and git archaeology, and every commit claim below was settled
with `git merge-base --is-ancestor`, never by comparing timestamps.

Branch: `fix/stale-ratchets-and-dead-code-tests-20260812`.

Inputs: W7-55-record-reconciliation.md §8 (which named all three items),
W7-20-refusal-laundered-into-wrong-answer.md, W7-16-arraydeque-and-linkedlist-residuals.md,
W4-1-publiclookup-allowedmodes-never-checked.md, W7-56-infercaller-strict.md.

---

## 1. The three items, and the one sentence each

1. **W7-20's frozen baselines were stale and slack-free.** Twelve rows of the
   kind map are re-frozen by hand, on the disclosure convention W7-56
   established in that file's own header three hours earlier — and one of the
   twelve was **not** frozen at what the tree produces, because what the tree
   produces there is a regression the gate is right to refuse.
2. **W4-1's unit tests guarded dead code.** The code is dead because it is
   **superseded**, so the code and the tests were resolved together: 332 lines
   deleted, six tests re-pointed at the gate that runs, four new arms added
   that the old ones could not express.
3. **W7-16's residual got worse as a side effect of its own fix landing.** The
   `jdk_interfaces` arm is applied; it **closes** the `ClassCastException`
   rather than moving it, and the reasoning for that verdict is in §3 rather
   than asserted.

And one item nobody had recorded: **a third stale ratchet, stale in the firing
direction** — see §2.4.

---

## 2. The ratchet and baseline inventory

Everything in the tree that freezes a number or a set and refuses a change.
Status is one of **CURRENT**, **STALE-LEGITIMATE** (the population moved for a
good reason and the freeze has not caught up) or **STALE-REGRESSION** (it moved
the wrong way).

| # | Gate | Frozen thing | Status | Direction |
|---|---|---|---|---|
| 1 | `scripts/baselines/jdk-only-kind-map-25-linux.tsv` | per-registration `kind`, `kind_stated`, `kind_chosen`, 11,636 rows | **STALE-LEGITIMATE, fixed here** (12 rows) — with one **STALE-REGRESSION** row family inside it, fixed at the source | mixed |
| 2 | `scripts/baselines/jdk-only-bridge-ratchet.json` | 5 counts, `SLACK = 0`, + a collapse floor | **STALE-LEGITIMATE, NOT fixed here** — needs a census | 2 up (fires), 1 down (passes), 2 unchanged |
| 3 | `native-builtins/tests/stub_ratchet.rs` | `BASELINE_SYNTHETIC_STUBS` 1263/1253, `SLACK = 0` | **STALE-LEGITIMATE, NOT fixed here** — needs a `cargo test` run. **Nobody had recorded this one.** | up, so it FIRES |
| 4 | `native-builtins/tests/duplicate_registration_gate.rs` | `BASELINE_SHADOWED` 1201/1150, `BASELINE_KIND_DISAGREEMENTS` 51/51 | **CURRENT**, best available evidence | — |
| 5 | `native-builtins/tests/lock_discipline_ratchet.rs` | `BASELINE_RAW_LOCKS = 432` | **CURRENT** on this session's changes; unaudited against the 182-commit range | — |
| 6 | `native-builtins/tests/eintr_ratchet.rs` | four exact per-file site counts (29/2/2/3), no slack, `==` | **CURRENT**; nothing in this session or in the retag wave touches TLS I/O | — |
| 7 | `native-builtins/src/jca/provider_chain.rs` | advertised-vs-serviceable `Cipher` set equality | **CURRENT** — and it is why W4-3's Patch E must never be applied | — |
| 8 | `scripts/baselines/jdk-only-strict-corpus-25-linux.txt` | 2 diverging `(probe, arm, section)` lines; fails when the SET grows | **UNKNOWN — needs a three-arm run.** Its own header documents `*/vthreads` as intermittent | — |
| 9 | `scripts/baselines/jdk-only-dead-everywhere.tsv` · `-GATED.tsv` · `jdk-only-gated-never-delete.tsv` | 299 / 283 / 234 rows | **NOT GATES.** No script reads them; they are cited from four comments in `vm/src/vm/tests.rs` as governing a deletion decision | — |
| 10 | `scripts/baselines/jdk-only-check-override-admissions.tsv` | 27 admitted triples | **NOT A GATE**, and its own header says so in capitals ("THIS IS NOT A DELETION LIST") | — |
| 11 | `tools/jdk-only-blockers/baselines/` | the design-§6 blocker pair | **EMPTY** — only a `.gitkeep`. `scripts/jdk-only-census.sh` will `--check` against it | — |
| 12 | `regression-suite/perf/baselines/` | perf reliability gate | **EMPTY** — `README.md` + `TEMPLATE.json` only | — |

Two of the twelve are worth a sentence beyond the row. Row 9's files look
exactly like gates — three committed TSVs of measured rows in `scripts/baselines/`,
under a `README.md` that says "one file per gate" and "never hand-edit a number"
— and nothing reads them. That `README.md` table lists exactly one file. Row 11
is an empty baseline directory a live script checks against; whether that
refuses or passes vacuously was not determined here, and it is the shape worth
looking at next.

### 2.1 The kind map — twelve rows, and why each moved (item 1, FIXED)

The file was frozen at `1c4377b5f` (2026-08-11 12:20) and hand-amended once
since, at `20f1327d4` (2026-08-12 03:23, W7-56, four rows). Three commits
change a registration's kind after the freeze and before HEAD:

| commit | when | what |
|---|---|---|
| `6ae3ca634` | 08-11 20:35 | `LinkedListSnapshotListItr` → `VM_SERVICE_RECEIVERS` + the VM-internal mint |
| `4eaa5d321` | 08-11 21:19 | `LogManager.{getLogManager, getLogger}` tagged `Bridge` |
| `3b20b83b5` | 08-11 22:34 | four `LogRecord` source-pair rows + `Formatter.formatMessage` tagged `Bridge` |

**The header's arithmetic was wrong, and this is the third time in three days
that a count taken from a commit message rather than from its diff has cost
something.** `3b20b83b5`'s message says it retags six rows; its diff retags
**five**. W7-56's header read "six", subtracted its own four, and concluded the
remainder was "`Formatter.formatMessage` and its neighbour". There is no
neighbour. The other two stale rows are `4eaa5d321`'s, which that header did not
look for at all. Corrected in place.

* **(a) Nine `cratonvm/internal/LinkedListSnapshotListItr` rows,
  `synthetic-stub` → `bridge`. LEGITIMATE.** The receiver left
  `VM_MINTED_STAND_IN_RECEIVERS`, so `receiver_declared_by_no_supported_image`
  answers `false` and `register()`'s re-tag arm no longer fires. This is the
  measured half of the fix W7-20 required to land in one commit with the mint.
* **(b) `java/util/logging/Formatter.formatMessage`, `intrinsic` → `bridge`.
  LEGITIMATE.** An intrinsic is the kind that cannot give an answer the bytecode
  would not, and this one did — `one={0} two={1}` against HotSpot's
  `one=A two=B`, in both modes. `kind_stated` stays `0`: a `with_category` scope
  is a *chosen* kind, not a *stated* one. The triple is not in
  `RETIRED_SHADOW_TRIPLES`, so the retirement arm does not fire and it stays
  `bridge`.
* **(c) `LogManager.{getLogManager, getLogger}` ordinal 0, `intrinsic` →
  `synthetic-stub`, `kind_stated` 0 → 1. LEGITIMATE.** Both triples are already
  in `RETIRED_SHADOW_TRIPLES`, so the `Bridge` tag `4eaa5d321` applied makes
  `register()`'s retired-shadow arm fire on the way past. That is the *point* of
  that commit: `Intrinsic` is exempt from the `java/util/logging/` retirement,
  so tagging them `Bridge` is what lets the retirement see them at all.

### 2.2 The row that was NOT frozen at what the tree produces

The nine rows in (a) also lose `kind_stated`, `1` → `0`, and that is a
**regression**, not drift.

The kind-map gate has two assertions, and only the first is about the kind. The
second is one-way: `kind_stated` and `kind_chosen` may go `false → true` freely
and **never** `true → false`, because a row that goes back to inheriting its
kind is a row the next ambient `set_category` edit moves in silence again. The
nine had `kind_stated = true` because the re-tag arm *stated* it (a measurement
adjudicated them). Excluding the receiver restored the ambient category of
`register_linked_list_natives`, and an ambient category is unstated.

So freezing `bridge 0 1` would have re-frozen the loss — a baseline update that
converts a gate into a rubber stamp for exactly the defect it exists to catch.
The nine registrations became
`register_with_kind(..., NativeKind::Bridge)` instead, and the baseline is
frozen at `bridge 1 1`. Same kind, same dispatch, no behaviour change in either
mode; only the census column moves. **That row's frozen value now depends on
that source edit** — the baseline header says so, and if the edit is reverted
these nine must read `bridge 0 1` and the gate must be allowed to fire.

### 2.3 Why the bridge-ratchet JSON was NOT re-frozen

Its five ratchets are counts over an image adjudication, and no census was
taken. The derived movement is in the file's own `note` and is repeated here:

| ratchet | frozen | derived | direction |
|---|---|---|---|
| `bridge.without_acc_native` | 8912 | 8922 | **UP by 10** — nine carrier rows (no image declares `cratonvm/…`) plus `formatMessage` |
| `bridge.shadows_bytecode_anywhere` | 6066 | 6067 | **UP by 1** — `formatMessage` only; JDK 25 declares it concrete (`javap`-confirmed), the carrier is absent from every image so it shadows nothing |
| `bridge.stated_shadows_bytecode` | 24 | 24 | unchanged |
| `superseded.kind_disagreements` | 52 | 50 | **DOWN by 2** — `getLogManager`/`getLogger` ordinal 0 stop disagreeing with the ordinal-2 winner |
| `superseded.stub_lost_to_admitted` | 4 | 4 | unchanged |

Two go up, so the gate fires. **The numbers were still not written in**, and
the rule is the one this campaign already applies to the strict-corpus
baseline, whose header records two hand edits with the justification *"dropping
two measured lines can only tighten the ratchet"*: a hand edit to a slack-free
ratchet is admissible **only in the tightening direction**. A count written too
high widens it and admits that many violations in silence, which is the same
defect as sizing a tolerance to admit its own bug. Every figure above is a
LOWER BOUND — 182 commits separate the freeze from HEAD and any of them may add
or remove registrations.

Settle it with `bash regression-suite/bridge-ratchet.sh`, which takes one
census and scores both gates from it, then re-freeze both with `--note`. Note
that this also regenerates the kind-map baseline's header, replacing the two
hand-written `# amended:` blocks — which is correct once the file is measured
again, but read them first.

### 2.4 The third stale ratchet, which nobody had recorded

`native-builtins/tests/stub_ratchet.rs` was frozen at `167bf048c`
(08-11 22:05). `git merge-base --is-ancestor` says that freeze **includes**
`6ae3ca634` (so the nine rows leaving the stub population are already counted)
and **excludes**:

* `4eaa5d321` — `LogManager.{getLogManager, getLogger}` `Bridge`, both retired,
  so both land on `SyntheticStub`. **+2**
* `3b20b83b5` — nothing retired at that commit. **+0**
* `01cfc2609` (08-12 03:18, W7-56) — adds the four `LogRecord` source-pair
  triples to `RETIRED_SHADOW_TRIPLES`, so their `Bridge` becomes
  `SyntheticStub`. **+4**

All are in scope: `register_phase54_logging_extras` is reached from
`register_essential_natives_with_shims`, the first entry in `VM_INIT_SEQUENCE`.
So expect **1269 / 1259** against a frozen 1263 / 1253, with `SLACK = 0` — the
gate fires.

**LEGITIMATE, and the file already contains the precedent.** Its own
`939 -> 1038, 2026-08-11: the first §1.4 shadow RETIREMENT` section says the
motion out loud: *"This ratchet's own message says 'make the new native a real
Bridge/Intrinsic instead of a fake — do NOT just raise the baseline', and that
is the right instruction for the case it was built for: a NEW stub arriving.
This rise is the opposite motion and the ratchet cannot tell them apart,
because it counts stubs and both directions move the count."* Six registrations
moved to `SyntheticStub` and not one is new; each stands in front of
`java/util/logging/` bytecode the image declares with a `Code` attribute.

Not re-frozen, for the §2.3 reason, with a `# PENDING` block written above the
constant. Recount with
`cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture`
and paste the constant the run names.

**2026-08-12, second pass — two things this section could not know.**

* **The +6 is not the only pending contribution, and the other two move it by
  different amounts.** The four new scalar `StringBuilder.insert` overloads
  (`IZ`/`IJ`/`IF`/`ID`, on three receiver class names) move this ratchet by
  **zero** — the ambient kind at that registration site is `Bridge`, so the
  twelve rows are outside this census's population entirely, while they do move
  row 2 and row 1. The unlanded 7-row `java/io/Print*` shadow retirement would
  move it **up by as much as seven**, in the same direction as the +6 and for
  the same reason. Both are now written above the constant, apart, so the run's
  diff can be attributed instead of averaged. Neither is a value anyone may
  paste.
* **Row 3's companion instrument — the source witness that guards this gate's
  SCOPE — was blind, and had been since it landed.** It located `vm_init`'s
  real-JDK arm with a `contains` match that hits a COMMENT quoting the
  attribute 44 lines above the attribute itself, so it scanned 39 lines of the
  sibling synthetic arm, observed 8 registrars instead of 48, and passed. The
  identical code was in row 4's file. Both are fixed and collapsed into one
  shared model; the account is in
  W7-30-stub-ratchet-boot-path-scope.md §9. It changes no number here — a
  source scan is not a count over the registry — but it means row 3's and row
  4's "the scope is checked" status was, until 2026-08-12, the same kind of
  claim this table exists to distrust.

### 2.5 The one gate whose kind-flip population this session checked and cleared

`duplicate_registration_gate.rs` froze at `b6f0bca44` (08-11 21:54), which
**includes** `6ae3ca634` and `4eaa5d321`. The two commits it excludes
(`3b20b83b5`, `01cfc2609`) touch only triples registered once — `formatMessage`
and the four `LogRecord` source rows are all ordinal 0 with no second
registrar, per the kind map — so neither the shadowed count nor the kind
disagreements move. Its ratchet is `if n <= b { return; }`, so a decrease would
pass anyway. **CURRENT.**

---

## 3. W7-16 — the `jdk_interfaces` arm (item 3, FIXED IN SOURCE)

### 3.1 The mechanism, checked before the edit

A synthetic class minted with no `jdk_interfaces` arm implements **nothing**.
Three things had to be true for the recorded one-line patch to be the right
fix, and none of them was safe to assume, because the record predates the door
the carrier now uses:

* **`jdk_interfaces` is read on the VM-internal path, not only the
  compatibility path.** It is called from `fabricate_class`, which is the
  shared body of *all three* `ensure_*_class` entry points — including
  `ensure_generated_class`, which is what `ensure_vm_internal_class` funnels
  into. So the arm reaches the `ClassOrigin::VmInternal` mint `6ae3ca634`
  installed. If it had only been read by the compatibility door, the patch
  would have been inert in exactly the mode that regressed.
* **Nothing else was already supplying interfaces.** `fabricate_class` has one
  other source, `is_synthetic_collection_iterator`, and its match list is six
  `HashMap`/`TreeMap` iterator names. It does not match this carrier, so the
  table was the only source and the carrier's interface list was genuinely
  empty.
* **The interfaces are loaded after the class is registered**, in the loop at
  the tail of `fabricate_class`, so declaring `java/util/ListIterator` cannot
  recurse into minting the carrier again.

### 3.2 Does it CLOSE the CCE or MOVE it? — closes

* `Class::is_assignable_to_name_inner` recurses through interfaces
  transitively, so `(Iterator) x` would work from the `ListIterator` entry
  alone. Both are listed anyway, matching the `java/util/ArrayList$ListItr` arm
  two entries above.
* `ListIterator`'s nine abstract methods are **exactly** the nine natives
  registered on the carrier, and dispatch probes the registry from the
  receiver's own class name first — so `invokeinterface` through the new
  declaration lands on the same bodies that `invokevirtual` reached before.
  This is the trap W7-9 is about (a minted class declaring an interface whose
  abstract methods it cannot answer) and it does not apply.
* `synthetic_stub_access_flags` keys the interface bit on a `$` in the name;
  `LinkedListSnapshotListItr` has none, so it stays a concrete class.

**What it does not close**, and must not be read as closing:
`listIterator().getClass()` still answers
`cratonvm.internal.LinkedListSnapshotListItr` — deliberately, because the real
name resolves to the real 5-field class whose layout mangles the carrier's
`Int` cursor into the `next:Node` slot; and a real `ListItr.remove()` against a
native `LinkedList` still leaves `size` stale. Both are the collections
ownership defect behind the carrier, which an interface list cannot touch.

### 3.3 The RED, and the instrument

`probes/ListItrInterfaceProbe.java`, with the HotSpot 25.0.3 control
transcript in `probes/ListItrInterfaceProbe.expected.txt` (16 scored rows, all
green on this host, run rather than asserted). The CratonVM arms are **stated,
not measured**, and labelled so in the file:

```text
BEFORE, in BOTH modes:   ll.instanceofListIterator false   want=true   FAIL
                         ll.castListIterator ClassCastException        FAIL
                         ll.castIterator     ClassCastException        FAIL
                         ll.idx.castListIterator ClassCastException    FAIL
                         SUMMARY pass=11 fail=5, exit 1
AFTER, in BOTH modes:    identical to the HotSpot transcript, pass=16 fail=0
```

`--jdk-only` reaching those rows at all is the regression: before `6ae3ca634`
the strict arm died earlier, on `NoClassDefFoundError` at the mint.

Two anti-vacuity mechanisms, because without them a green run here would mean
nothing:

* **Every cast row launders its reference through `opaque(Object)`.** A
  reference held in a variable already typed `ListIterator` compiles to no
  `checkcast` at all, and the probe would then pass on a VM where the class
  implements nothing — reporting green for the exact defect it exists to find.
  `selfTestNoCheckcast` is the calibration row that makes the distinction
  visible.
* **Three `selfTestRed` rows whose EXPECTED answer is the
  `ClassCastException`**, taken against a bare `java.lang.Object`. Every other
  row reports PASS when a cast *succeeds*, so a helper that swallowed the
  exception would turn the whole file green. These are the only rows that can
  detect that.

`al.*` is a control family throughout: `java/util/ArrayList$ListItr` already
has an arm, so an `al.*` row going red on a CratonVM arm means the instrument
is broken and the `ll.*` rows say nothing that run.

---

## 4. W4-1 — six tests guarding nothing (item 2, FIXED)

### 4.1 Why the code is dead: SUPERSEDED

The three candidate causes have three different correct answers, so this was
settled before anything was touched:

* **Not a `#[cfg]` gate.** There is none on the block.
* **Not a vanished caller.** The functions have callers *within the dead
  block*; what they have no registration.
* **SUPERSEDED.** All ten `find*` triples — `findVirtual`, `findStatic`,
  `findConstructor`, `findGetter`, `findSetter`, `findStaticGetter`,
  `findStaticSetter`, `findSpecial`, `findVarHandle`, `findStaticVarHandle` —
  are registered by `lang_invoke::register_p63_method_handles_lookup`, and
  `classloader.rs`'s own registration site says so in its comment: *"do NOT
  re-register here as that would overwrite the real implementations with
  incompatible stubs"*. There is no mode in which these bodies are registered:
  not `--real-jdk`, not `--jdk-only`, not `synthetic-jdk`. `dead_code` is
  allowed crate-wide, so nothing warned.

That is W4-1's own headline restated at one remove. A fully written access
check for *"`publicLookup()` must not reach a private method"* sat in
`classloader.rs` passing its own unit tests, while the VM ran a different copy
in `lang_invoke.rs` that did not perform the check. The green tests are part of
how it shipped.

### 4.2 Code and tests resolved together

Deleting the tests and leaving the code is how the next reader concludes the
feature exists; deleting the code and leaving the tests does not compile. Both
moved:

* **332 lines deleted** from `native-builtins/src/classloader.rs`:
  `lk_member_access_flags`, `enforce_lookup_access`, and eleven `lk_find_*`
  bodies. A tombstone in their place names what supersedes them.
* **`lk_public_lookup` was NOT deleted.** W4-1's patch block lists it for
  deletion and is **wrong**: it is registered, on
  `MethodHandles$Lookup.publicLookup()`. Applying that line would have dropped
  a live registration. A §2.4-species stale prescription inside the record's
  own patch block, found by grepping every name on the list for a registration
  rather than trusting the list.
* **The tests were six, not four** (W7-55 counted the line citations, and
  `:13064`/`:13087` share a citation). All six moved to `lang_invoke.rs`'s test
  module, aimed at `lk_enforce_find_access` — the first statement of all ten
  `lookup_find_*`.
* Three stale comments elsewhere that named `enforce_lookup_access` as the
  enforcing gate were corrected, including one in `lang_invoke.rs` itself.

### 4.3 The old tests were worse than "aimed at dead code"

Every one of them built its Lookup with `LK_PUBLIC` (0x01). W4-1's own
measurement section, four hundred lines above them, records that
`publicLookup().lookupModes()` is **0x20** — `UNCONDITIONAL`, and it does not
carry the `PUBLIC` bit at all. So the five tests named `..._with_public_lookup_...`
were asserting about a Lookup shape the JDK never hands out.

Four arms were added, each because it is a way this gate can be wrong that none
of the others notices:

| new row | what it catches |
|---|---|
| `publiclookup_is_refused_a_private_method` with modes `0x20` | the defect W4-1 is named after, at the real mode word |
| `publiclookup_is_refused_a_public_member_of_a_non_public_class` | `UNCONDITIONAL`'s rule is about the target CLASS (`fd86485c6`) |
| `a_zero_mode_lookup_is_refused_even_a_public_member` | `6dd552ce2`'s arm, refused before the member walk |
| `an_unreadable_mode_word_stays_permissive` | `Some(0)` vs `None`. **Every other row in the block would still pass with the two collapsed**, and collapsing them turns every Lookup shape this VM does not model into an `IllegalAccessException` |

Plus the positive private case. `classloader.rs`'s test module carried a NOTE
saying it *"cannot be exercised through these natives in-unit"* because
*"`lk_modes_of` always reports mode 0 under the mock"*. **That note was false at
the time it was read.** It describes the by-name-first reader that W6-3
replaced: the mock has no `allowedModes` in `mock_field_slot` and no declared
field for it, so the class-side witness answers `None` and the reader falls
through to the synthetic slot the test wrote. A comment outliving its defect,
and the cost was a coverage gap somebody had documented as impossible.

`MockNativeContext::set_class_access_flags` was added, because the mock's
default class flags are `0` (package-private) and `UNCONDITIONAL` asks about
the class: a `publicLookup()` test left at the default is refused before the
member is looked at, and would then pass for the wrong reason on every row
whose assertion is "this throws".

**Where the tests run is unchanged.** They were in
`native-builtins/src/classloader.rs`'s `#[cfg(test)] mod tests` and are now in
`native-builtins/src/lang_invoke.rs`'s — the same crate, the same
`cargo test -p cratonvm-native-builtins` invocation, and not
`vm/src/vm/tests.rs`, which is synthetic-jdk-only and would have gone dark on a
default build.

---

## 5. Which mode each change affects

| change | `--jdk-only` | `Compatible` (`--real-jdk`) |
|---|---|---|
| the nine `register_with_kind` calls | unchanged — same kind, same dispatch | **unchanged.** Only the census column `kind_stated` moves |
| the twelve kind-map rows, the JSON note, the `stub_ratchet` note | neither is code | neither is code |
| deleting the dead lookup block + moving six tests | unchanged — nothing deleted was registered in any mode | **unchanged**, same reason |
| `jdk_interfaces` for `LinkedListSnapshotListItr` | the `ClassCastException` this mode gained on 2026-08-11 goes away | **TOUCHED, and it qualifies under the §5 freeze as a HotSpot-parity fix.** Real `java.util.LinkedList$ListItr` implements `ListIterator`, so every cast this admits is one HotSpot admits, and `instanceof` stops answering `false` for a question the JDK answers `true` |

---

## 6. Verification, once this is built

```sh
# item 1 — both gates from one census; expect the kind map to PASS and the
# bridge ratchet to fire with exactly the §2.3 movement and nothing else.
#
# MUST BE RUN ON LINUX AGAINST JDK 25. Both artefacts are keyed
# `<jdk-feature>/<os>`; `jdk-only-bridge-ratchet.py`'s `host_os()` and this
# script's `--os` both come from the running host, so on Windows the gates look
# up `25/windows`, find no baseline, and exit 2 — "REFUSING", which is neither a
# pass nor a fail and cannot re-freeze anything.
bash regression-suite/bridge-ratchet.sh

# item 1 — the third ratchet; expect 1269 (management) and re-freeze from the
# printed line, not from this record
cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture

# item 2 — the re-pointed tests
cargo test -p cratonvm-native-builtins --lib lang_invoke

# item 3 — both arms, against the committed HotSpot transcript
javac -d /tmp/probeout probes/ListItrInterfaceProbe.java
for M in --real-jdk --jdk-only; do
  target/release/cratonvm $M --java-home "$JDK25" -cp /tmp/probeout ListItrInterfaceProbe
done
```

## 7. Falsifying observations

* **If the kind-map gate fires on any row other than an `add`/`remove`**, a
  fourth commit changed a kind after `1c4377b5f` that the ancestry check
  missed, and §2.1's list is incomplete.
* **If it fires on the nine carrier rows with `bridge 0 1`**, the
  `register_with_kind` edit did not take effect — check that
  `register_with_kind` still routes through `register`, because the retired-shadow
  and no-image arms live there and bypassing them would be a much larger defect
  than a census column.
* **If the bridge ratchet moves by anything other than +10 / +1 / −2**, the
  extra movement is a finding from one of the other 181 commits, not an error in
  §2.3 — attribute it before re-freezing.
* **If `stub_ratchet` reports anything other than 1269 / 1259**, the same.
  Do not paste 1269; paste what it printed.
* **If `ll.castIterator` passes while `ll.castListIterator` still raises**, the
  `jdk_interfaces` arm took effect but assignability is not walking
  super-interfaces transitively, and the defect is in
  `Class::is_assignable_to_name_inner`.
* **If an `al.*` row is red on any CratonVM arm**, the probe is broken and its
  `ll.*` rows are not evidence.
* **If the six re-pointed tests are green on a binary with
  `lk_enforce_find_access`'s body replaced by `Ok(())`**, they are as vacuous as
  the ones they replaced. Four of them assert a refusal, so they should not be —
  but that is the check, and it costs one line to run.

## 8. Findings for other people

* **`probes/LaunderProbe.java` does not exist.**
  W7-20-refusal-laundered-into-wrong-answer.md's Part 1 table is a measurement
  taken with it, and its Probes section says it was "written for this record".
  It is not in `probes/` and not in `regression-suite/src/`. This is the second
  instance of that exact species in two days — W7-53 had to write
  `probes/AsyncCloseProbe.java` fresh for the same reason. A measurement whose
  instrument is not in the tree cannot be re-run, which is close enough to not
  having been measured.
* **A commit message's count is not its diff's count.** `3b20b83b5` says six and
  retags five, and one baseline header plus one campaign record both inherited
  the error. `git show <commit> -- '*.rs'` costs one command.
* **`scripts/baselines/README.md` describes one gate and the directory holds
  seven files.** Four of them (`jdk-only-dead-everywhere.tsv`,
  `-GATED.tsv`, `jdk-only-gated-never-delete.tsv`,
  `jdk-only-check-override-admissions.tsv`) are read by no script at all. They
  are governed data, and two of them say so in their headers, but sitting in
  `baselines/` under a `README.md` whose rules are all about gates makes them
  read as gates.
* **`tools/jdk-only-blockers/baselines/` is empty except for `.gitkeep`, and
  `scripts/jdk-only-census.sh` runs the blocker gate `--check` against it.**
  Whether that refuses or passes vacuously was not determined here. It is the
  next thing to look at in this directory.
* **`scripts/jdk-only-kind-map.py` named a runner that has never existed**
  (`regression-suite/native-kind-map.sh`). Corrected to
  `regression-suite/bridge-ratchet.sh`, with a note that an `--update-baseline`
  run regenerates the kind-map header and drops the hand-written `# amended:`
  blocks.
