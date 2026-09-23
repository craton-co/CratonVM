# F15 — two registrars no bytecode on this JDK can name, and the mode question that shortens every reachability argument

**Date:** 2026-08-13 **Lane:** F15
**Closes:** E40-1 N2 / F9-1 N1 (`register_p67_string_template`); NOM F12-3
(the `RandomGenerator` interface rows); W7-10 §7.1 (`ProcessHandle.onExit()`);
F7 NOM 1 (`p71_bi_mag_bits`) and F7 NOM 3 (`isProbablePrime`).
**Confirms already-landed:** F7 NOM 2 (the two byte-array `BigInteger`
constructors) — **no edit needed, both validations are already in the tree.**
**Adds:** two live wrong answers found while verifying the above
(`ThreadLocalRandom.nextInt` bad-bound arms; its `i32` range subtraction).

**This lane wrote code and docs only. It did not build, did not run `cargo`,
and did not execute CratonVM.** Every CratonVM "after" below is **PREDICTED**.
Evidence is (a) source read in this working tree, (b) `javap`/`java` runs I made
myself on this host — openjdk 25.0.3+9-LTS, Microsoft-13877124 — and (c) the
edited file parse-checked with `rustfmt --edition 2021 --emit stdout` on a
**copy** (exit 0), which proves it parses and proves nothing about types.

## 0. Files edited

| file | why it is mine |
|---|---|
| `native-builtins/src/phases_late.rs` | assigned, exclusively owned |
| this document | assigned (new `.md` under `docs/known-issues/jdk-only/`) |

Line endings: the file is **CRLF**. The one edit too large for the Edit tool
(the 102-line `register_p67_string_template` excision) was done by splitting on
`` `r`n `` and writing back with `UTF8Encoding($false)`, so no BOM was
introduced and no line ending changed. Verified after every edit with `file`.

---

## 1. The question I should have asked first, and now every record can

**Which mode does this registrar even run in?** Three of the five sites here
turned out to be answered by that question before any dispatch reasoning was
needed, and I got one of my own in-file comments *wrong* by not asking it until
after I had written the comment. Traced, not assumed:

| registrar | reached from | runs in |
|---|---|---|
| `register_phase64_natives` (→ `register_p64_random_generator`) | `lib.rs::register_synthetic_overrides` only | **synthetic-JDK only** |
| `register_phase67_natives` (→ `register_p67_string_template`) | same | **synthetic-JDK only** |
| `register_p71_biginteger_extras` | `lib.rs::register_essential_natives_with_shims` | **every mode**, `--jdk-only` included |
| `register_p60_process_handle` | `register_phase60_natives` **and** two hand-written arms of `vm/src/vm/vm_init.rs` | real-JDK + synthetic; **refused** under `--jdk-only` (`SyntheticStub`) |

`register_synthetic_overrides` is `#[cfg(feature = "synthetic-jdk")]` and its
only caller is `register_builtins`, which is the synthetic path
(`lib.rs:21601-21605`). `vm/src/vm/vm_init.rs`'s real-JDK arms state it in as
many words — *"The phase bundles are synthetic-only, but SmallRye calls
`ProcessHandle.current().info()` in real-JDK mode as well"* — and then
hand-register the handful of phase natives they actually want, which is exactly
why `register_p60_process_handle` is in a different column from the rest.

**The self-correction, recorded because it is the lesson.** I first wrote, in
the `RandomGenerator` doc comment, *"a `Bridge` row is NOT dropped by
`--jdk-only`, so strict mode would have served a fabricated PRNG in silence."*
The clause about `Bridge` is true; the sentence is not, because the registrar
never runs in `--jdk-only` at all. A true general fact about categories does not
survive being applied to a registrar whose reach you have not checked. Both
deletion notes now open with the mode trace.

---

## 2. `register_p67_string_template` — deleted whole (E40-1 N2, F9-1 N1)

### 2.1 Measured, not recalled

```text
$ javap -p java.lang.StringTemplate
Error: class not found: java.lang.StringTemplate
$ java -version
openjdk version "25.0.3" 2026-04-21 LTS (Microsoft-13877124, build 25.0.3+9-LTS)
```

The string-template API was a preview feature, withdrawn after JDK 23. No
bytecode on this JDK can name the class, so all eight rows were fabricated
compatibility stand-ins. One was additionally type-confused: `fragments` was
registered under `()Ljava/util/List;` and returned slot 0, which `of(String)`
had just filled with a `java.lang.String` (F9's finding; reproduced by reading
the two bodies side by side).

### 2.2 The re-verification the brief asked for, and why it is not a formality

`call_native` **panics** on an unregistered triple that something still
reaches, and a Rust panic is not a Java throwable — it takes the VM down. So
"F9 grepped it" is not sufficient grounds to delete a registration. Re-run here
over the whole tree (`grep -rIn StringTemplate .`, excluding `target/`, `.git/`
and `docs/`), the only surviving hits are **comments**:

* `native-builtins/src/lib.rs:24115` — the phase-67 header line;
* `vm/src/vm/tests.rs:50804-50814` — the tombstone where
  `string_template_basics_p67`, the registrar's **sole** caller, was already
  deleted whole by E40-1 §1b, and which quotes the same `javap` output.

No class declaration names `java/lang/StringTemplate` or
`java/lang/StringTemplate$Processor` on the synthetic side either, so neither
mode can construct a receiver that reaches a `StringTemplate` triple. Deleted:
the banner comment, the whole `pub(crate) fn register_p67_string_template`, and
the call site in `register_phase67_natives`, which now carries the `javap`
output and the grep result so nobody re-adds it.

**Ratchet: −8 `Bridge` registrations, synthetic-mode total only.** The
`--jdk-only`/strict total is unchanged, because these rows were never in it.
`BASELINE_SYNTHETIC_STUBS` is unaffected (these were `Bridge`, not
`SyntheticStub`). `MIN_TOTAL_REGISTRATIONS` (11,800) is a **floor**, so −8 moves
toward it; combined with §3 the synthetic total drops by 14, and if that floor
is closer than 14 it will trip — **PREDICTED, not measured; I could not run the
test.**

**One residual, out of my file:** `lib.rs:24115`'s phase-67 header comment still
lists `StringTemplate` among the phase's subjects. NOMINATION 1 below.

---

## 3. The six `java/util/random/RandomGenerator` rows — deleted (NOM F12-3)

### 3.1 Verdict: (a), delete. The reachability argument is recorded at the site.

F12 asked for "delete or justify" and I verified rather than took its word.
Its conclusion holds; **one of its supporting claims does not**, and the
correction matters more than the verdict.

**F12 said:** *"nothing dispatches on interface names."* **That is too strong.**
`vm/src/vm/vm_exec.rs`'s slow dispatch path ends with a loop the code itself
labels *"Second pass: fall back to a native registered on any interface name
(legacy behavior)"*, walking the receiver's transitive superinterface closure
and calling `native_methods.find(iface_name, …)`. An interface-keyed native
**can** fire in this VM. F12's verdict survives anyway, for reasons F12 did not
have to invoke — which is precisely why the mechanism is worth writing down.

The four gates, each read in the tree:

1. **Mode.** §1: the registrar does not run in `--real-jdk`/`--jdk-only`. Six
   rows that do not exist cannot be reached.
2. **The registry has no hierarchy.** `resolve_id`
   (`native-api/src/registry.rs`) is a digest probe plus a re-check of all three
   strings; its only fallback, `resolve_id_with_descriptor_quirks`, rewrites the
   **descriptor**. No superclass walk, no superinterface walk. So the question
   is entirely which class name the interpreter hands it.
3. **The interpreter hands it the resolved DECLARING class.**
   `vm/src/runtime/interpreter/invoke.rs` — the native site takes
   `&class.name` from `cm.get_class(declaring_id)`, and the second site's
   `class_name_arc` likewise; neither reads the constant pool.
   `find_method_recursive` (`classloading/src/class.rs`) returns on the first
   **concrete** superclass-chain match and enters its interface BFS only when
   there is none. `javap -p java.util.Random` declares all six concretely, so
   Phase 1 answers `java/util/Random`. In synthetic mode
   `class_manager.rs:12831`'s `"java/util/Random" => instance_fields(2)` has no
   `implements` clause at all, so `RandomGenerator` is not in the hierarchy and
   the interface-name loop of gate 4 cannot even enumerate it.
4. **The interface-name fallback loop is preceded by a pass that beats it.**
   That loop runs only after the concrete-chain lookup has failed, and the pass
   immediately before it prefers a **non-abstract interface default** and
   dispatches to its bytecode. Measured:

   ```text
   $ javap -p java.util.random.RandomGenerator
     public abstract long nextLong();          <- the ONLY abstract method
     public default int  nextInt();            public default int  nextInt(int);
     public default long nextLong(long);       public default double nextDouble();
     public default float nextFloat();         public default boolean nextBoolean();
   ```

   `nextLong()J` being the sole abstract means **every legal implementor must
   declare it concretely**, so gate 3 catches it; the other five are `default`,
   so the earlier pass routes them to the JDK's own bytecode before the native
   loop is consulted. Each of the two gates closes the other's case.

### 3.2 What was wrong with them, which is why guarding was not the answer

All six were backed by `p64_simple_random()`, which **takes no receiver**. Any
seeded `Random`/`SplittableRandom` that ever reached them would have had its
entire stream silently replaced by an unrelated xorshift — a wrong answer with
no null and no exception in it. And `nextInt(I)` divided by `bound as u64` with
no guard: `bound == 0` is a Rust integer divide-by-zero, i.e. a **panic that
kills the VM**, and a negative bound sign-extended into a huge modulus and
returned garbage.

Guarding the divide would have kept six receiver-ignoring bodies alive under the
one category `--jdk-only` does *not* drop. Deletion is the end state.
`java/util/random/RandomGenerator` now occurs nowhere in this repository outside
the JDK jars. **Ratchet: −6 `Bridge` registrations, synthetic total only.**

### 3.3 The sibling that is NOT dead — two live wrong answers, fixed

The `[1 of 10 callsites]` shape F12 named cuts the other way too. The
`ThreadLocalRandom` rows in the same registrar are keyed on a **class** name;
`javap -p` shows `ThreadLocalRandom` is `final` and declares `nextInt(int)` and
`nextInt(int,int)` concretely, so gate 3 makes them the **winning** registration
wherever registered — synthetic mode, per §1. Their guards returned a number
where HotSpot throws. Measured on this host:

| call | HotSpot 25.0.3+9 (measured) | was | now |
|---|---|---|---|
| `TLR.nextInt(0)` | `IllegalArgumentException: bound must be positive` | returned `0` | IAE, message verbatim |
| `TLR.nextInt(-5)` | `IllegalArgumentException: bound must be positive` | returned `0` | IAE |
| `TLR.nextInt(5,5)` | `IllegalArgumentException: bound must be greater than origin` | returned `5` | IAE, message verbatim |
| `TLR.nextInt(5,1)` | `IllegalArgumentException: bound must be greater than origin` | returned `5` | IAE |
| `TLR.nextInt(1,5)` | ok | ok | ok |
| `Random.nextInt(0)` / `SplittableRandom.nextInt(0)` | `IllegalArgumentException: bound must be positive` | — | — (not this registrar) |

**The two messages are different**, which a paraphrase would have flattened:
they are `RandomSupport`'s `BAD_BOUND` and `BAD_RANGE`. A caller that had
computed an empty range got a plausible index instead of the exception that says
its range is empty.

**A third defect, found while editing that line and not previously filed.**

```rust
let range = (bound - origin) as u64;                 // i32 subtraction
origin + ((p64_simple_random() as i64).unsigned_abs() % range) as i32
```

`bound - origin` in `i32` overflows for any range wider than
`Integer.MAX_VALUE`. `nextInt(Integer.MIN_VALUE, Integer.MAX_VALUE)` is the
whole-domain call and is **legal** — measured on this host, it returns an
ordinary int. Rust checks integer overflow on subtraction in **release** builds
too, so that line turned the JDK's widest legal range into a panic, and the
`origin + …` on the next line had the same defect. Both are now computed in
`i64` and narrowed once at the end. This is not reachable only through a
hostile argument: `nextInt(-2_000_000_000, 2_000_000_000)` is an ordinary
application call.

---

## 4. W7-10 §7.1 — `ProcessHandle.onExit()` (the third task, chosen for severity)

Chosen over the other open `phases_late.rs` nominations (E16-2, E25-1, E28-4,
E32-4, E37-2, E35-2) because those are guard/census work, and per the F5
severity ordering a **wrong answer** outranks a blind guard. E25-1 turned out to
be already landed anyway — §6.

### 4.1 What it answered, and the three ways that was wrong

The row returned an already-**completed** `CompletableFuture` carrying `null`,
for every receiver and every process state. It was the one row in
`register_p60_process_handle` that neither delegated nor measured. Measured on
this host:

```text
ProcessHandle.current().onExit()   !! java.lang.IllegalStateException: onExit for current process not allowed
alreadyExitedChild.onExit()        -> CompletableFuture, isDone()=false   (isAlive()=false)
```

1. For the **current** process the JDK refuses — a process cannot wait for
   itself — where this said it had already exited.
2. The JDK's future completes with **the ProcessHandle**
   (`.handleAsync((exitStatus, unused) -> this)`), never with `null`.
3. It is **not complete when handed back** — note the second row: even for a
   child that has *already exited*, HotSpot returns `isDone()==false`
   synchronously and completes it from the reaper. "Completed immediately" is
   not a state HotSpot returns at all.

### 4.2 The blocker W7-10 stated, verified rather than believed

W7-10 exempted this row because delegating *"registers a reaper against
`ProcessHandleImpl.completions` and `waitForProcessExit0`"* and so *"changes
process-reaping behaviour rather than just an answer"*. Per the standing rule
that a doc comment in this tree can be confidently wrong, I checked the stated
blocker. **The machinery it names is present, implemented, and already measured
against HotSpot:** `native-io/src/process.rs` registers
`ProcessHandleImpl.waitForProcessExit0(JZ)I`, whose own-child arm blocks in
`wait_for_handle` inside a `begin_blocking_region`, and whose foreign-pid arm
returns the JDK's `NOT_A_CHILD` (−2) so the JDK's own `isAlive0` poll loop takes
over — pinned by `probes/ForeignHandleProbe.java` against HotSpot 25, with the
before/after rows recorded in that function's own doc comment. It is the same
path `Process.onExit()` on a spawned child already runs. Delegation therefore
adds no reaper this VM was not already running.

### 4.3 What landed

1. **Current-process refusal, first, by pid** —
   `IllegalStateException("onExit for current process not allowed")`, HotSpot's
   message verbatim. Answered here rather than left to fall out of the real
   `ProcessHandleImpl.onExit()`'s `this.equals(current)` test, for two reasons:
   it is the only arm that must also hold in synthetic-JDK mode, where there is
   no `ProcessHandleImpl`; and `p60_handle_destroy` in the same file already
   refuses the current process by pid with the JDK's own message, so this is the
   established shape rather than a new one.
2. **Delegate**, via `p60_delegate_to_real_handle` — exactly what `children`,
   `descendants`, `info` and `parent` in the same registrar already do.
3. **Fallback** (synthetic-JDK, no `ProcessHandleImpl`): complete with **the
   receiver handle** instead of `null`.

### 4.4 The residual I deliberately did NOT remove

Completing at all still asserts "this process has exited" for a process that may
be running. I did **not** convert the fallback to an incomplete future, even
though that is closer to HotSpot, because `p60_pid_is_alive` answers a flat
`true` for every foreign pid on non-unix — it says so in its own comment, it has
no portable probe — so on Windows that would turn a wrong answer into a silent
forever-hang in `get()`: a worse failure with a harder diagnosis. Removing the
last of it needs what §7.2 needs, a process waiter `native-builtins` can call
without a real JDK. Stated in place, in the `p60_unmeasurable_process_tree`
convention.

**Divergence introduced, stated:** in the delegating path the future completes
with the **real** `ProcessHandleImpl`, not with the minted receiver. Strictly
better than `null`, and after W7-10's own delegation fixes no path mints a
bare-interface handle when `ProcessHandleImpl` is loadable — but it is not
`==`-identical to the receiver, and if a fixture asserts
`h.onExit().get() == h` that is where it will show.

---

## 5. F7's three nominations

### 5.1 F7 NOM 1 — `p71_bi_mag_bits` deleted, `BigInt::magnitude_bits` called

Applied as written. `magnitude_bits` is `pub(crate)` in `bigint.rs:704` and
carries the `bitLength()`-vs-magnitude warning and F2's
`(-2).shiftLeft(MAX-2)` measurement, so the local doc comment was dropped rather
than duplicated. One rule, one owner.

I re-read `math_bignum.rs` rather than take the hand-off on trust: F7's own
`bi_checked_shl` **and** that registrar's `shiftLeft`/`shiftRight` rows are gone
(`:1470` records it), so `p71_bi_checked_shl` really is the single
implementation. `p71_bi_checked_shl` lives in `register_p71_biginteger_extras`,
which §1 places in `register_essential_natives_with_shims` — **every mode**, so
the orchestrator's "reached in every mode" is confirmed, not assumed.

### 5.2 F7 NOM 3 — `isProbablePrime`. Applied, plus one row F7's record lacks

I re-measured all of F7's rows on this host before writing the fix; **every one
reproduced**. Two more that F7's record does not carry, and one of them decides
the ordering of the fix:

| call | HotSpot 25.0.3+9 (measured) | old body |
|---|---|---|
| `(-7).isProbablePrime(10)` | **true** | false |
| `(-2).isProbablePrime(10)` | **true** | false |
| `(-4).isProbablePrime(10)` | false | false |
| `(-1).isProbablePrime(10)` | false | false |
| `0.isProbablePrime(10)` | false | false |
| `4.isProbablePrime(0)` | **true** | false |
| `4.isProbablePrime(-1)` | **true** | false |
| `4.isProbablePrime(1)` | false | false |
| **`0.isProbablePrime(0)`** | **true** | false |
| **`(-1).isProbablePrime(0)`** | **true** | false |
| `(1000003*1000033).isProbablePrime(100)` | false | false (this file was already correct here) |

The last two are the addition. `certainty <= 0` short-circuits **before any
inspection of the value** (JDK 25 `BigInteger.java:1156`), so even zero and −1
answer `true`. The guard must therefore precede `bi_read_int`, not sit beside
it — which F7's proposed body does correctly, and which a body written from the
first nine rows alone might not have.

The negative-value half: `BigInt::is_probable_prime` opens `if self.neg ||
self.is_zero() { return false; }`, so calling it directly made
`(-7).isProbablePrime(10)` false. The JDK takes `this.abs()` first and sign has
no part in primality. Zero survives the `abs()` and still answers false, which
matches.

**Note for the merge:** this file's copy was *not* the one with the
cap-returns-true trial division — that was `math_bignum`'s, which F7 fixed. This
copy already delegated to the validated `BigInt::is_probable_prime` (trial
division below 1000, then Miller-Rabin), so `1000003 * 1000033` was already
answered correctly here. The two copies now agree, and each wins in a different
mode.

### 5.3 F7 NOM 2 — **already landed. No edit made.**

The `[dup-fix]` merge-time shadow check paying off: I read the two constructors
before editing them and both validations are **already in the working tree**,
landed by F2. `<init>([B)V` has the `NumberFormatException("Zero length
BigInteger")` length check; `<init>(I[B)V` has the signum-range check ahead of
the zero-magnitude shortcut and the mismatch check.

That mismatch check is also **subtler than F7's summary**, and correctly so.
F7's text says it "does not reject signum 0 with a non-zero magnitude"; the
landed code tests the decoded **bytes** for a non-zero one, not the array for
non-emptiness. My own measurement says that is required:

```text
new BigInteger(0, new byte[]{1})    !! NumberFormatException: signum-magnitude mismatch
new BigInteger(0, new byte[]{0,0})  = 0            <- LEGAL, non-empty, all zero
new BigInteger(1, new byte[0])      = 0            <- LEGAL, empty
new BigInteger(new byte[0])         !! NumberFormatException: Zero length BigInteger
```

An implementation written from "reject signum 0 with a non-empty magnitude"
would have rejected row 2, which HotSpot accepts. **Nothing to do; recorded so
the next lane does not re-apply it.**

---

## 6. E25-1 — also already landed; only its prose was stale

NOM E25-1 is marked **BLOCKING** in E25-R11. Checking it before picking a task:
the row is already `("getInstance", "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Mac;", true, "")`
and the overload really is registered
(`phases_late/ssl_security.rs:396`). **The tree is not red from this.**

What was left was the **doc comment above the test**, which still said the
seventeenth overload *"is NOT registered anywhere in the tree"* and is *"carried
below as an explicit `false` row"*. Stale in the direction that matters: the
reverse ratchet makes a closed gap a *failure*, and a reader who trusted the
paragraph over the table would have read the assertion as a standing exemption —
the one thing the ratchet exists to prevent. Rewritten, keeping the sentence
E25 said must survive: **the population is `javap`'s, not the registry's.**

---

## 7. What I predict, split by claim type

The brief asks these apart, so they are apart. **I built nothing; all of it is
predicted.**

### 7.1 Predicted to FLIP (red→green or wrong→right), given a build

| check | why |
|---|---|
| `RJdkProcess.java:145-150` (`current().onExit()` must throw `IllegalStateException`) | §4.3 arm 1 answers it directly, in both modes, independent of delegation |
| any `--only=bigint` row asserting `(-7).isProbablePrime(10)`, `(-2).isProbablePrime(10)`, `4.isProbablePrime(0)`, `4.isProbablePrime(-1)`, `0.isProbablePrime(0)`, `(-1).isProbablePrime(0)` | §5.2; every-mode registrar |

### 7.2 Predicted to become REACHABLE, outcome UNKNOWN — a different claim

| check | why unknown |
|---|---|
| `ProcessHandle.of(pid).onExit()` on a real handle | now runs the JDK's own reaper path instead of returning a completed future. The natives beneath it are measured (§4.2); this specific composition has never executed |
| any `--only=random` / synthetic-mode `ThreadLocalRandom` bad-bound row | previously got a number, now gets an `IllegalArgumentException`. If a fixture asserted the number, it flips **red** — and that would be correct |
| `ThreadLocalRandom.nextInt(MIN, MAX)` | previously a release-mode overflow panic, now returns. Nothing exercises it today that I found |

### 7.3 Predicted to move a COUNT, not a verdict

* **−14 `Bridge` registrations** on the **synthetic-mode** total (8 + 6).
  `MIN_TOTAL_REGISTRATIONS` (11,800) and `STRICT_MIN_TOTAL_REGISTRATIONS`
  (10,900) are **floors**; the strict one cannot move (§1), the synthetic one
  moves down by 14 and will trip **if it is within 14 of the floor**. I could
  not measure it. `BASELINE_SYNTHETIC_STUBS` is unaffected — every deleted row
  was `Bridge`.
* `bridge_shadows_bytecode`, if it counts `Bridge` rows shadowing loadable
  bytecode: the six `RandomGenerator` rows shadow real default methods and may
  be in it; the eight `StringTemplate` rows cannot be, since the class does not
  exist to shadow. **Direction down, magnitude 0–6, unmeasured.**

### 7.4 Explicitly NOT claimed

Nothing here changes `--jdk-only` **behaviour** except by way of §5 (the
BigInteger rows, which are essential-path). The two deletions are synthetic-mode
hygiene; the `onExit` fix is refused by strict mode's `SyntheticStub` policy and
matters in `--real-jdk`.

---

## 8. NOMINATIONS

### NOM F15-1 — `native-builtins/src/lib.rs:24120` — the phase-67 header still lists a deleted subject

Not my file. **Located by symbol, not by line number**, and the line number is
re-read here: older records cite `:24115`, which today is
`register_phase65_natives(registry);` — `lib.rs` is being edited concurrently,
so anchor on the text below, not the number. OLD (verified unique in the working
tree today, LF, four-space indent):

```rust
    // --- Phase 67: StructuredTaskScope, ScopedValue, Gatherer, AsyncChannels, ForeignMemory, StringTemplate ---
```

NEW:

```rust
    // --- Phase 67: StructuredTaskScope, ScopedValue, Gatherer, AsyncChannels, ForeignMemory ---
```

`register_p67_string_template` no longer exists (§2). Cosmetic, but it is the
last place in non-doc source that advertises a `StringTemplate` capability.

### NOM F15-2 — `vm/src/vm/vm_exec.rs` — the interface-name native fallback has no census and no test

Not my file, and **not a proposed code change** — a proposed *instrument*. §3.1
found that the loop labelled *"Second pass: fall back to a native registered on
any interface name (legacy behavior)"* is a live dispatch route that two
consecutive lanes (F3, then F12) reasoned about the registry without knowing
existed. F3 filed the hazard as "recorded, not investigated"; F12 investigated
and concluded *"nothing dispatches on interface names"*, which is false as
stated and true only for the family it was looking at.

Ask: a counter on that loop, or a test that pins which `(interface, method,
descriptor)` triples can currently reach it. Today the answer to "which
interface-keyed natives are live?" requires re-deriving §3.1's four gates by
hand, and the same re-derivation has now been done three times.

### NOM F15-3 — `docs/known-issues/jdk-only/W8-F7-1-*.md` — NOM 2 should be marked already-landed

Not my file (F7's record). Its NOMINATION 2 asks for two `BigInteger`
byte-array-constructor validations that are **already in the working tree**
(§5.3), and its summary of the mismatch rule would, if implemented literally,
reject `new BigInteger(0, new byte[]{0,0})`, which HotSpot accepts. Suggest
marking it CLOSED-ALREADY-LANDED with the four measured rows from §5.3, so the
next taker does not re-apply a check that is present and slightly wrong.

---

## 9. What I deliberately did not do

* **Did not touch `math_bignum.rs`, `lib.rs`, `vm_exec.rs`, `vm_init.rs`, or
  `ssl_security.rs`.** Everything I needed there is a NOMINATION above.
* **Did not re-apply F7 NOM 2** — already landed, §5.3. Applying it blind would
  have been a duplicate at best and, taken from the record's wording rather than
  the measurement, a regression on `new BigInteger(0, new byte[]{0,0})`.
* **Did not convert `onExit`'s synthetic fallback to an incomplete future** —
  §4.4, it would be a Windows hang.
* **Did not touch the BigInteger shift *registrations*** beyond the one-line
  `magnitude_bits` swap F7 asked for. The bodies are F2's and are measured.
* **Did not build, run `cargo`, or execute CratonVM**, per the brief. The file
  parses (`rustfmt` on a copy, exit 0); it has not been type-checked, and
  §7 is predictions.
