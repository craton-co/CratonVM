# F12 — NOM F3-1 applied, and a seventh divergence the argument-kind sweep found: `getInstance`'s argument ORDER

**Date:** 2026-08-13 **Lane:** F12
**Closes:** NOM F3-1 (all six sites).
**Adds:** a seventh site in the same file, on the same axis, found by the sweep
F3's lesson asked for.

**This lane did not build or run CratonVM, and did not run `cargo`.** Every
CratonVM "after" below is **PREDICTED**. Evidence is (a) source read in this
working tree, (b) the JDK 25 source checkout at `C:\craton\jdk25src`, and
(c) **HotSpot 25.0.3+9 runs I made myself** on this host
(`scratchpad/f12/Oracle.java`, 46 rows). I re-measured every row I changed
rather than taking F3's table on faith; all six agreed.

## 0. Files edited

| file | why it is mine |
|---|---|
| `native-builtins/src/securerandom.rs` | assigned, exclusively owned |
| this document | assigned (new `.md` under `docs/known-issues/jdk-only/`) |

---

## 1. The six sites of NOM F3-1, applied

Each row re-measured on OpenJDK 25.0.3+9 before the edit, and cross-checked
against the JDK 25 source line that produces it.

| # | call | HotSpot 25 (measured) | JDK source | was | now |
|---|---|---|---|---|---|
| a | `Random.nextBytes(null)` | NPE `Cannot read the array length because "bytes" is null` | `Random.java:462` opens `bytes.length` | silent | NPE, that message |
| b | `SecureRandom.nextBytes(null)` | NPE, **message null** | `SecureRandom.java:774` `Objects.requireNonNull(bytes)` | silent | NPE, `message: None` |
| c | `SecureRandom.setSeed((byte[])null)` | NPE, **message null** | `SecureRandom.java:724` `Objects.requireNonNull(seed)` | silent | NPE, before the receiver check |
| d | `new SecureRandom((byte[])null)` | NPE, **message null** | `SecureRandom.java:266` `Objects.requireNonNull(seed)` | silent | NPE, arity-gated |
| e | `SecureRandom.getInstance(null)` | **NPE** `null algorithm name` | `SecureRandom.java:391` `Objects.requireNonNull(algorithm, …)` | IAE, same message | NPE |
| e′ | `SecureRandom.getInstance("")` | **NoSuchAlgorithmException** `" SecureRandom not available"` | `GetInstance` dead-end | IAE `null algorithm name` | NSAE, via the existing `format!` |
| f | `SecureRandom.generateSeed(-1)` | IAE `numBytes cannot be negative` | `SecureRandom.java:878` verbatim | IAE `numBytes must be non-negative` | corrected |

Two details worth keeping, because both are the kind of thing a plausible
"fix" gets wrong:

* **(b)/(c)/(d) genuinely have a null message.** `Objects.requireNonNull(obj)`
  with no message argument is what the JDK calls, so `message: None` is the
  faithful answer and not a shortcut taken because the string was unknown.
  Only `nextBytes` on `java.util.Random` (a) carries a message, and it carries
  it because HotSpot's *helpful NPE* synthesises one from the bytecode — the
  JDK source has no `requireNonNull` there at all, just a bare `bytes.length`.
* **(e′) is subtractive, exactly as F3 said.** The file already builds
  `format!("{algo} SecureRandom not available")` below, which for `algo == ""`
  is `" SecureRandom not available"` — HotSpot's answer character for
  character, leading space included. I confirmed the empty name reaches it:
  `secure_random_static_provider("")` normalises to `""` and matches no arm, so
  `secure_random_algorithm_supported("")` is false and the existing branch
  fires. **No new string was added.** Deleting the `if algo.is_empty()` early
  return was the whole of that half.

---

## 2. The seventh site — `getInstance`'s argument ORDER

F3's lesson was *sweep by argument kind, not by method*. Doing that on the
**nullable-reference** axis across all 27 registered triples turned up one more
divergence, which no per-method audit would have reached because the method
in question rejects both of its arguments correctly — **in the wrong order.**

Measured on OpenJDK 25.0.3+9:

```text
getInstance(null, "SUN")               -> NullPointerException: null algorithm name
getInstance(null, "NOPE")              -> NullPointerException: null algorithm name
getInstance(null, (String) null)       -> NullPointerException: null algorithm name
getInstance(null, (Provider) null)     -> NullPointerException: null algorithm name
getInstance("SHA1PRNG", (String) null) -> IllegalArgumentException: missing provider
getInstance("SHA1PRNG", "")            -> IllegalArgumentException: missing provider
getInstance("SHA1PRNG", "NOPE")        -> NoSuchProviderException: no such provider: NOPE
```

All three 2-arg overloads OPEN with
`Objects.requireNonNull(algorithm, "null algorithm name")` —
`SecureRandom.java:439` (`String` provider) and `:481` (`Provider`). The
provider is examined only afterwards. `native_secure_random_get_instance_with_provider`
ran `check_named_provider_arg` **first**, so rows 2–4 answered
`NoSuchProviderException` / `IllegalArgumentException` where HotSpot answers
NPE. Note row 1 as well: a *valid* provider name plus a null algorithm still
had to travel through two provider checks before the null was noticed.

Fixing (e) alone does **not** fix this. The 1-arg body is reached only after
both provider checks have already had their chance to throw, so the check has
to be hoisted into the 2-arg body itself. It now is.

### 2.1 Where the wrong order came from — a true comment read one clause too far

`jca/provider_chain.rs:2612-2614`, on `check_named_provider_arg`, says:

> mirroring real JDK's ordering: the provider is resolved BEFORE the algorithm
> is looked up

That is **true and load-bearing** — and it is about the algorithm *lookup*, not
the algorithm *null check*. Both sit "before the algorithm" from the lookup's
point of view, and only one of them is after the null check. A reader
implementing a new `getInstance` from that sentence puts the provider call
first and is wrong on exactly the four rows above.

The tree already knew better. `phases_late/ssl_security.rs:340-374` — another
lane's file, its measurements dated the same day — carries the same table for
`javax.crypto.Mac` and states the rule outright as its point 3:

> **The null-algorithm check runs FIRST.** `getInstance(null, null)` is NPE,
> not IAE — measured on both two-argument overloads.

and its code puts the null-algorithm check *above*
`check_named_provider_arg`. So this is [1 of 10 callsites] again: the correct
convention existed, was measured, was written down, and the sibling JCA family
twenty files away did the opposite. The two now agree.

---

## 3. The sweep — by argument kind, and what it CLEARS

The point of the exercise is the axes that came back clean, so the next lane
does not re-audit them. All 27 registered triples, sorted by the kinds of
argument they take rather than by method.

### 3.1 Nullable references — 5 sites, **5 were defects**

`Random.nextBytes([B)`, `SecureRandom.nextBytes([B)`, `SecureRandom.setSeed([B)`,
`SecureRandom.<init>([B)`, `SecureRandom.getInstance(String…)` (all three
overloads). Every one swallowed null; every one is fixed above. **This axis had
a 100% defect rate** — which is the measure of how completely it had gone
unexamined.

### 3.2 Ranged integers — 3 sites, **0 defects**

Driven at `0`, `-1`/`-5`, `Integer.MIN_VALUE`, and the accepting boundary.

| triple | HotSpot | ours | verdict |
|---|---|---|---|
| `Random.nextInt(I)` | IAE `bound must be positive` at 0 / -5 / MIN; no throw at 1, MAX_VALUE | same, same wording | **clear** |
| `SecureRandom.nextInt(I)` | IAE `bound must be positive` at 0 / -5 / MIN | same wording, and the bound is checked **before** the receiver, as HotSpot does | **clear** |
| `SecureRandom.generateSeed(I)` | IAE at -1 and MIN_VALUE; empty array at 0 | wording fixed in §1(f); the 0 and MIN rows were already right | **clear after (f)** |

This is the axis F3 identified as the one that *had* been audited, and it holds
up under a wider probe than the one that produced it. `Integer.MIN_VALUE` is
the interesting column — a `bound <= 0` test passes it, an `abs(bound)` or
`bound < 0` test would not, and all three sites use `<= 0`.

### 3.3 Arrays, empty and oversized — 4 sites, **0 defects**

* **Empty:** `Random.nextBytes(new byte[0])`, `SecureRandom.nextBytes(new byte[0])`,
  `SecureRandom.setSeed(new byte[0])`, `new SecureRandom(new byte[0])` — no
  throw on HotSpot, no throw here. `Random.nextBytes` at lengths 0/4/7/8 is
  already covered green by `--only=random` checks 27 and 38–40, which is
  independent confirmation rather than my reading.
* **Oversized:** `generateSeed(Integer.MAX_VALUE)` would ask for a 2 GB array.
  This is **clear by ordering, not by luck**: the body calls
  `ctx.new_array(Byte, n)` *before* `vec![0u8; n]`, and `new_array`
  (`vm/src/vm/vm_exec.rs:11019`) raises a catchable `OutOfMemoryError` for a
  request neither generation can satisfy when running under
  `safe_native_call`'s `catch_unwind`. The unbounded Rust `vec!` is therefore
  unreachable for a length the heap already refused. Had the two lines been in
  the other order this would be a process abort. Left alone; recorded so the
  ordering is not "cleaned up" later.

### 3.4 Unranged longs — 3 sites, **0 defects**

`Random.<init>(J)`, `Random.setSeed(J)`, `SecureRandom.setSeed(J)`. Measured:
HotSpot accepts **everything**, including `Long.MIN_VALUE` and `0`. There is no
rejection to miss here, and — the point worth stating — the file does not
invent one. `native_secure_random_set_seed`'s `if seed == 0 { return }` is not
a rejection but the JDK's own `if (seed != 0)` (`SecureRandom.java:761`),
present because `Random`'s constructor calls `setSeed` virtually.

### 3.5 String / enum selectors — 1 site, **1 defect (e′), plus one clear column**

`SecureRandom.getInstance(String)`. Beyond null and `""`:

* `getInstance("NO-SUCH-PRNG")` → NSAE `NO-SUCH-PRNG SecureRandom not available`
  on both. Clear (a prior wave fixed it).
* `getInstance("sha1prng")` → **no throw on HotSpot**: JCA lookup is
  case-insensitive. Ours normalises to alphanumeric + upper-case in
  `secure_random_static_provider`, so it accepts it too, and
  `secure_random_is_sha1prng` uses `eq_ignore_ascii_case`, so the seeded
  SHA1PRNG path engages for the lower-case spelling as well. **Clear** — and
  this one is easy to get wrong in a way no null probe would reveal.

### 3.6 No-argument members — 12 sites, not applicable

`<init>()V`, `nextInt()`, `nextLong()`, `nextDouble()`, `nextFloat()`,
`nextBoolean()`, `nextGaussian()` on both classes, plus `getInstanceStrong()`.
No argument, so no argument contract. Their correctness is a *value* question
(the LCG stream), which `--only=random` checks 1–40 already measure and pass.

---

## 4. Which of `--only=random`'s 60 checks change — PREDICTED

The distinction matters here because 19 of the 60 have never executed on any
CratonVM binary.

**FLIPS red → green: check 41, and only check 41.**
`nextBytes(null)` now raises `RuntimeError::NullPointerException`, which
`types/src/error.rs:1478` maps to `java/lang/NullPointerException` — the exact
string `nameOf(rt)` compares against
(`RJdkIntrinsics2.java:2176`). The check tests the **type only**; the message I
added is not read by it, and is there for the developer, not the assertion.

I verified the index by hand-counting `check()` calls from the top of
`random()`: 3 + 2 + 2 + 1 + 5 + 5 + 5 + 3 + 1 + 3 + 1 + 1 + 2 + 3 + 3 = 40 before
`nextBytes(null)`. It is check 41. F3's instrumented HotSpot run and my static
count agree independently.

**BECOMES REACHABLE, status unknown: checks 42–60 (19 checks).**
They are *unblocked*, not *fixed*. None of my edits touches any of them —
verified rather than assumed: none of the triples they call
(`nextInt(II)`, `nextLong(J)`, `nextLong(JJ)`, `nextDouble(D)`, `nextDouble(DD)`,
`nextFloat(F)`, `nextExponential()`, `ints`/`longs`/`doubles`) is registered
anywhere in `securerandom.rs`, so all of them run as real JDK bytecode over the
intercepted primitives. F3's per-row risk assessment stands; I did not re-derive
it, and I did not run them.

**UNCHANGED: checks 1–40.** The `Object(None)` arm I added is unreachable for a
non-null array, and the three other `Random` edits are on `SecureRandom`, which
this family never constructs.

**Not measured by this family at all: sites (b)–(f) and §2.** `--only=random`
drives `java.util.Random` only. Five of the seven fixes have **no check in this
family**, so a green `--only=random` is not evidence for them. `RJdkSecurity.java`
is the family that would cover `SecureRandom`; see NOM F12-2.

---

## 5. NOMINATIONS

### NOM F12-1 — `regression-suite/src/RJdkSecurity.java` — the `SecureRandom` argument-kind vector does not exist

Five of the seven divergences fixed here — (b), (c), (d), (e), (e′), (f) and §2
— have **no regression check anywhere in the tree**. I confirmed this: no
`.java` file in the repo mentions `generateSeed`, and none drives
`getInstance("")`. The `java.util.Random` half of this exact defect had a check
(41) and was therefore found; the `SecureRandom` half had none and was found
only by reading the file next to it.

Add to `RJdkSecurity.java`, mirroring `--only=random`'s GAP structure — the
oracle values are all in §1 and §2 above and are measured, so the vector can be
written without a HotSpot run:

* `nextBytes(null)`, `setSeed((byte[])null)`, `new SecureRandom((byte[])null)`
  → `java.lang.NullPointerException`;
* `generateSeed(-1)` → IAE with message `numBytes cannot be negative` — **assert
  the message**, since the type was already right and only the wording was wrong;
* `getInstance(null)` → NPE, `getInstance("")` → NSAE — **assert the type**, since
  both used to be IAE;
* the four argument-order rows of §2, which is the only way to catch a
  regression that re-orders the two checks.

Not written by this lane: `regression-suite/src/` is not mine.

### NOM F12-2 — `native-builtins/src/jca/provider_chain.rs:2612-2614` — the comment that produced §2

Comment-only, in a file that is not mine. `check_named_provider_arg`'s doc says
the provider is resolved "BEFORE the algorithm is looked up". True, and it
silently omits that the algorithm's **null check** runs before *both*. Suggested
addition, in the tree's own measured voice:

> Callers must do their own `Objects.requireNonNull(algorithm, "null algorithm
> name")` BEFORE calling this: on HotSpot 25 the null-algorithm check precedes
> all provider handling (`SecureRandom.java:439`, `:481`), so
> `getInstance(null, <anything>)` is NPE and never
> NoSuchProviderException/IAE. `ssl_security.rs`'s `Mac` overloads and
> `securerandom.rs`'s do this; a new caller that does not will diverge on four
> rows and on no others.

Two of the tree's JCA families now hoist the check independently, with no shared
helper enforcing it — which is the shape that drifts. A
`require_non_null_algorithm(args, idx)` helper in `provider_chain.rs` beside
`check_named_provider_arg` would make the ordering hard to get wrong; I did not
add one, because that file is not mine and a third copy of the check is worse
than two until they can be unified in one diff.

### NOM F12-3 — `native-builtins/src/phases_late.rs:5076-5107` — six interface registrations that no receiver in this tree can reach, one of which panics

*(F3's task text cited `:5010`; the function `register_p64_random_generator` is
actually at `:5072` and its `RandomGenerator` block at `:5076-5107`. Line
numbers below are re-verified against the working tree.)*

Characterised in full in §6. It is **not** the hazard F3 feared — it cannot win
for a `java/util/Random` receiver in either mode, so no `--only=random` check is
exposed to it. What the investigation found instead is worth its own row:

1. **The six `java/util/random/RandomGenerator` registrations appear to be
   unreachable.** The interface name occurs exactly once in the repository
   outside the JDK jars — its own registration line. No class declaration names
   it as an interface, so nothing dispatches through it today. Either delete
   them or write the receiver that justifies them; a registration that cannot
   fire is a registration whose wrongness is invisible, which is how the
   `nextInt(I)` body below came to be shipped.
2. **`nextInt(I)I` divides by the bound with no guard** — `:5080-5088`, the
   divide at `:5086`:
   `(p64_simple_random() as i64).unsigned_abs() % (bound as u64)`. `bound == 0`
   is an integer divide-by-zero — a Rust **panic**, where HotSpot raises
   `IllegalArgumentException: bound must be positive` (§3.2, measured). A
   negative bound does not panic; it sign-extends through `as u64` into a huge
   modulus and silently returns garbage. The `ThreadLocalRandom` sibling
   (`:5125-5137`) has the **identical** final expression at `:5135` preceded by
   `if bound <= 0 { return Ok(Some(Value::Int(0))); }` — so the two copies of
   one expression disagree, and neither matches HotSpot. Diffed, not assumed.
3. The generator is receiver-ignoring, so were it ever reached for a seeded
   receiver it would replace the LCG stream wholesale — F3's original concern,
   correct in its consequence and inapplicable to `Random`.

Not mine: `phases_late.rs` is owned by another lane. Filed with the reachability
question answered so whoever takes it can start from "delete or justify" rather
than from "is this live".

---

## 6. The hazard at `phases_late.rs:5010` — characterised

**Plainly: it cannot win for a `java/util/Random` receiver, in either mode.
Checks 42–60 are at ZERO risk from this source, not low risk.** F3 recorded it
as "recorded, not investigated"; it is now investigated, and the answer is
negative in both modes for two *different* reasons.

`register_p64_random_generator` (`phases_late.rs:5072`, its `RandomGenerator`
block `:5076-5107`) registers
`nextInt()I`, `nextInt(I)I`, `nextLong()J`, `nextDouble()D`, `nextFloat()F`,
`nextBoolean()Z` on the interface `java/util/random/RandomGenerator`, backed by
the receiver-ignoring `p64_simple_random()`.

**1. The registry itself has no hierarchy.** Every lookup funnels through
`native-api/src/registry.rs:7337 slot_index_for_key`, which is a digest probe
plus a full re-verification of all three strings. The only miss-fallback is
`resolve_id_with_descriptor_quirks` (:7399), which rewrites the **descriptor**
only. There is no superclass walk and no superinterface walk inside the
registry. So the question reduces entirely to *which class name the interpreter
hands it*.

**2. The interpreter hands it the class `find_method_recursive` returns**
(`vm/src/runtime/interpreter/invoke.rs:3630-3671`: "look up in the registry by
declaring class"), and `invokeinterface` takes the identical path as
`invokevirtual` — `opcodes.rs:2490-2504` — with the dispatch class taken from
`heap.class_id_of(receiver)`. The CP class is used only when there is no
receiver class id at all (null or zeroed header).

**3. `find_method_recursive` is two-phase, and Phase 2 is the only interface
walk.** `classloading/src/class.rs:1857-1890`, read directly:

```rust
        if let Some(method) = class.find_method(method_name, method_descriptor) {
            if !method.is_abstract() {
                // Concrete method … the most-specific real dispatch target.
                return Some((method, current_id));
            }
```

Phase 2's own comment says it is reached only on "No concrete superclass-chain
method".

**4. Real-JDK mode: Phase 1 wins.** `javap -p java.util.Random` on JDK 25 — run
on this host, not recalled — lists all six as concrete public methods:

```text
public class java.util.Random implements java.util.random.RandomGenerator, java.io.Serializable
  public int nextInt();      public int nextInt(int);
  public long nextLong();    public boolean nextBoolean();
  public float nextFloat();  public double nextDouble();
```

`Random` *does* implement `RandomGenerator`, which is what made the hazard
plausible — but every one of the six is concrete on `Random` itself, so Phase 1
returns `(method, java/util/Random)` and Phase 2 is dead code for this class.
The registry key is `java/util/Random` and `securerandom.rs`'s registration
wins. This is also exactly what checks 1–40 passing already proved empirically;
the mechanism is now known rather than inferred.

**5. Synthetic-JDK mode: Phase 2 has nothing to find.** `java/util/Random`'s
synthetic declaration is `class_manager.rs:12831`,
`"java/util/Random" => instance_fields(2)` — a field-layout entry with no
`implements` clause and no method declarations. The interface is not in its
hierarchy at all, so the BFS cannot reach `RandomGenerator` even when Phase 1
finds nothing.

**6. The registrations are, as far as this tree goes, dead.**
`java/util/random/RandomGenerator` occurs **once** in the entire repository
outside the JDK jars: `phases_late.rs:5076`, the registration itself. No class
declaration anywhere names it as an interface. The one shape that *would* reach
them is a receiver whose runtime class declares `RandomGenerator` and provides
no concrete override — a user lambda or a JDK
`SplittableRandom`/`ThreadLocalRandom`-shaped implementor falling back to the
interface's default methods. That is a **different receiver**, so it cannot
affect this family; but if it ever happens it silently swaps in a non-LCG
stream, and `nextInt(I)`'s body there divides by `bound as u64` with **no
`bound <= 0` guard** — a zero bound is a Rust divide-by-zero panic, not the IAE
§3.2 measured. Not mine to fix.

**Why this mattered enough to chase.** The failure mode F3 described is the
worst-shaped one available: fifteen checks failing as a block with
plausible-looking random numbers, no null and no exception, looking nothing like
the defect actually under repair. Ruling it out *before* the run means a red
42–56 after this fix is a genuine new finding rather than a suspect this lane
already had reason to distrust. That is the whole value of clearing it, and it
is why "LOW risk" was not good enough.

---

## 7. The lesson

**F3's lesson, applied, produces a defect F3's own audit did not reach — and
the difference is what "argument kind" means.**

F3 swept `securerandom.rs` by member and by contract, and found five null
swallows and two wrong exception types. Sweeping the same file by *argument
kind* found one more, and it is a different shape from all seven: **the method
rejects both of its arguments, with the right types and the right messages, and
still diverges — because it rejects them in the wrong order.** No per-method
audit reaches that, because per-method the code looks complete; and no
single-argument probe reaches it either, because the divergence only exists when
*two* arguments are bad at once and you must find out which one HotSpot blames
first. Argument-kind sweeping made me drive the cross product, and the cross
product is where it lived.

**The corollary is about where the wrong order came from.** Not from carelessness
— from a *true sentence*. `check_named_provider_arg`'s comment says the provider
is resolved before the algorithm is looked up, and that is correct; it is a
statement about the lookup, and the null check is not the lookup. A comment can
be entirely accurate and still be a trap when the thing it is silent about sits
one clause away. The tree had already measured the missing clause and written it
down in `ssl_security.rs` — so the information was present, in the right words,
in a sibling family, and this file diverged anyway. [1 of 10 callsites] is
usually told as "grep the shape"; the sharper version here is **grep the shape
across families that share a contract, because the family that got it right is
the cheapest oracle you will ever have** — cheaper than HotSpot, since it is
already in your language and already explains itself.
