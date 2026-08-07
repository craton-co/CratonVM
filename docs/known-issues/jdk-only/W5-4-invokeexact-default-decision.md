# `CRATONVM_MH_STRICT_INVOKEEXACT` — should the default flip?

**Status:** DECISION recorded 2026-08-07 (lane W5-4, JDK-only wave 5). **No default
was changed by this lane.** The flip is a two-file change listed below; it needs
the A/B in *How to close it* first.

**Recommendation: FLIP**, with one residual named and accepted, and with the
expectation set correctly: **flipping does not make `RJdkHandles` pass.** It
advances the failure from `adaptation()` line 153 to `varHandles()`, which has
never executed. See §*What flipping actually buys*.

This lane re-derived W3-1's safety argument from the source as it now stands
rather than from W3-1's own summary. The argument holds with **one** exception,
stated precisely in §*The one refutation*.

---

## 1. The check, re-derived

`vm/src/vm/vm_exec.rs::unbox_poly_return_checked` throws
`java/lang/invoke/WrongMethodTypeException` iff **all** of:

| # | condition | source |
|---|---|---|
| 1 | `method_name == "invokeExact"` | `vm_exec.rs:1558` |
| 2 | `mh_strict_invokeexact()` | `vm_exec.rs:1558` (the flag) |
| 3 | call-site return char `R` is one of `JIBSCZFD` | `primitive_wrapper_for_ret_char` returns `None` otherwise |
| 4 | the produced value is `Value::Object(Some(obj))` | `vm_exec.rs:1562` |
| 5 | `obj`'s class resolves to a name | `vm_exec.rs:1574` — unresolvable bails out |
| 6 | that name is one of the eight in `PRIMITIVE_WRAPPER_CLASSES` | `vm_exec.rs:1577` |
| 7 | that name `!=` `primitive_wrapper_for_ret_char(R)` | `vm_exec.rs:1577` |

Call it the **fire set** `F`.

`coerce_value_against_ret_char` (`vm_exec.rs:1353`) fabricates a zero
(`Long(0)`/`Float(0.0)`/`Double(0.0)`/`Int(0)`) iff conditions 3, 4 hold and
`cls_name != expected_wrapper`, where `expected_wrapper` is spelled inline at
`vm_exec.rs:1385-1395`. Call that the **fabricate set** `Z`.

Two facts make `F ⊂ Z` — a **strict** subset, not merely a subset:

* The two wrapper tables are byte-identical, char for char
  (`J`→Long, `I`→Integer, `B`→Byte, `S`→Short, `C`→Character, `Z`→Boolean,
  `F`→Float, `D`→Double). W3-1's two unit tests at `vm_exec.rs:26128` and
  `:26156` are what keep them from drifting.
* `Z` is *wider* on two axes `F` deliberately declines: an **unresolvable**
  class (`coerce` uses `.unwrap_or_default()` → `""` ≠ expected → zero; the
  check bails at condition 5) and a **non-wrapper** object (condition 6).

So with the flag on, the only behavioural delta anywhere in the VM is:
**on a strict subset of the inputs that today get a fabricated zero, an
exception is raised instead.** Nothing else changes — `invoke`,
`invokeBasic`, `invokeWithArguments`, every `VarHandle` accessor, every
reference/array/void return, `null`, and every argument-side shape take
byte-identical paths (condition 1 and 3 exclude them structurally).

### 1a. Where the wrapper class actually comes from

`native-builtins/src/lang_class.rs::box_value` (line 4065) is
**descriptor-driven and correct per wrapper**: `"Z"` allocates
`java/lang/Boolean`, `"C"` allocates `java/lang/Character`, and so on. It never
collapses the int-shaped primitives onto `Integer`. This was the single most
dangerous thing to get wrong — had `box_value` boxed a `boolean` as `Integer`,
condition 7 would fire on **every** `Z`-returning `invokeExact` in the tree.
It does not.

`auto_box_return` (`lang_invoke.rs:8582`) calls it with
`return_type_desc(mh_read_desc(this))` — the handle's **raw `MH_DESC`**. So for
a value that `auto_box_return` boxed, condition 7 reduces exactly to:

> `MH_DESC`'s return char `!=` the call site's return char, both primitive.

There is a second producer: if `mh_dispatch` already returned an
`Object(Some(_))`, `auto_box_return` preserves it verbatim
(`lang_invoke.rs:8590`), so the wrapper is whatever the leaf produced,
independent of `MH_DESC`.

---

## 2. CONFIRM / REFUTE

**W3-1's claim:** *"every value reaching that branch is already a wrong answer,
so nothing correct can become an exception."*

**Verdict: CONFIRMED for the computation, REFUTED for the observable outcome.**

The first half is airtight and I re-derived it above: on `F`, today's VM does
not compute anything — it *invents* a zero. The value is not derived from the
handle, the arguments, or the leaf's result.

The second half does not follow, and this is the gap:

> **A fabricated zero is the correct observable answer whenever the true answer
> is `0` / `0L` / `0.0` / `'\0'` / `false`.**

For `Z` this is a coin flip, not a corner case: today's `Int(0)` reads as
`false`, and a caller doing `if (!(boolean) h.invokeExact(x))` currently takes
the right branch every time the right answer is `false`. For `I`, zero is the
single most common int.

### 2a. …but the refutation is narrower than it looks

For a fabricated zero to be *accidentally right* **and** for HotSpot to have
disagreed with throwing, you need `MH_DESC`'s return to be **stale relative to
the handle's true `type()`**. Split `F` by that:

* **(a) The program really did write a mismatched `invokeExact`.** HotSpot
  throws `WrongMethodTypeException` here too — `invokeExact` converts nothing.
  Throwing is *correct*; the accidental zero was a spec violation regardless.
  `RJdkHandles.java:152` is this case.
* **(b) CratonVM's `MH_DESC` is stale.** `lang_invoke.rs:9963-9972` records the
  mechanism in the tree: *"CratonVM's `asType` is a passthrough that stamps the
  `type` field but leaves `MH_DESC` carrying the LEAF method's real (possibly
  primitive or void) return type."* So
  `add.asType(methodType(long.class,int.class,int.class))` followed by
  `(long) h.invokeExact(a,b)` is **legal** in HotSpot and returns the widened
  long; CratonVM boxes `Integer` off the stale `(II)I` and lands in `F`.

**(b) is the entire refutation, and only its zero-valued slice.** Note what
that slice is: a path that is *already broken today for every non-zero value*.
No suite can be depending on it except by the coincidence that its answer is
always zero/false. The flip converts *"silently wrong for all values except
zero"* into *"loudly wrong for all values"*.

Nothing distinguishes (a) from (b) from the value alone. The distinguishing
information is the handle's own `type()`, which `unbox_poly_return_checked`
does not receive. See §5.

---

## 3. What actually funnels through the four call sites

| site | guard | reachable with `invokeExact`? |
|---|---|---|
| `vm/src/runtime/interpreter/invoke.rs:3311` (`try_stackless_invoke`) | `class_name == "java/lang/invoke/MethodHandle"` **exactly**, method ∈ {`invoke`,`invokeExact`,`invokeBasic`} | yes — **this is the production path**, and the one `RJdkHandles:152` uses |
| `vm/src/vm/vm_exec.rs:22893` (`invoke_on_class_shared_inner`, prefer-exact / dynamic `DowncallHandle`) | inside `if is_mh \|\| is_vh`; method from the 33-name signature-polymorphic list | yes, for `MethodHandle`-family receivers |
| `vm/src/vm/vm_exec.rs:22911` (base-class lookup) | same | yes |
| `vm/src/vm/vm_exec.rs:22929` (exact-class lookup) | same | yes |

`is_var_handle_signature_polymorphic_receiver` receivers reach the three
`vm_exec` sites, but no `VarHandle` method is named `invokeExact`, so condition
1 excludes them all.

### 3a. Wave-3's named risks, re-checked structurally

Condition 3 requires a **primitive** call-site return. That alone removes four
of the six by construction — not by testing:

| risk | call-site return | in `F`? | test in THIS tree? |
|---|---|---|---|
| Groovy `IndyInterface` | `Object` (every indy call site) | **structurally impossible** | yes — `scripts/smoke/ri14_groovy.sh` (downloads groovy 4.0.21), plus 11 hand-built indy probes under `docs/internal/fixed-suite-bugs/repros/springrepos-indy-3c/` |
| JRuby | `Object` | **structurally impossible** | **none** — zero fixtures; the name appears only in Rust comments and docs |
| Jackson 3 `DirectMethodHandle$Constructor` | the constructed type (reference); also `MH_KIND_CONSTRUCTOR` skips `auto_box_return` entirely | **structurally impossible** | **none for Jackson 3** — `tools.jackson` is absent from the tree; only Jackson **2** exists, via `scripts/smoke/ri5_jackson.sh` |
| Spring/Hibernate proxying | reference returns | **structurally impossible** | runners exist (`apps/spring-suite-runner/run-suite.sh`, `apps/hib-suite-runner/run-hib.sh`) but the `apps/spring-framework` / `apps/hibernate-orm` checkouts are **absent from this worktree** |
| Panama `DowncallHandle` | `MemorySegment` → reference; **`float`/`int`/`long` downcalls → primitive** | **possible** | **no end-to-end Java fixture** — nothing in the tree calls `Linker.nativeLinker().downcallHandle(...)` from Java. Coverage is Rust-side only: `native-builtins/src/panama.rs` (~62 unit tests), `panama_libffi.rs` (3), `vm/tests/es_segalloc_arena_dispatch.rs` (`MemorySegment`, a reference return) |
| Netty `invokeExact(Thread)Z` | `Z` | **possible** | yes — `scripts/smoke/ri11_netty_echo.sh` (downloads netty 4.1.110.Final) |

Read the last two columns together. The four risks with the loudest names are
**structurally excluded** by condition 3 — they cannot reach `F` no matter what
the suite does. The two that *can* reach `F` split the other way:

* **Netty** is a real risk with a runnable test.
* **Panama primitive downcalls are a real risk with no test that can detect
  them.** The Java-visible Panama surface in this tree returns
  `MemorySegment`; the primitive-return downcall path is exercised only from
  Rust, which does not go through `unbox_poly_return_checked` at all. This is
  the one place the A/B below is blind, and it is the reason §8's falsifier is
  worth watching for after the flip lands rather than only during it.

Both survivors are direct `findVirtual`/`findStatic`/`Linker.downcallHandle`
handles whose `MH_DESC` *is* the true descriptor — i.e. case (a)-shaped, where
the check agrees with HotSpot.

Two additional in-repo vectors do reach `F`'s preconditions and **are** cheap to
run, and neither was on wave 3's list:

* `vm/tests/resources/cratonvm/MethodHandleTest.java:225` —
  `int result = (int) bound.invokeExact(7)`, driven from
  `vm/tests/interpreter_tests.rs` (~:1757, the WP1.6 strict-arity round-trip).
* `regression-suite/src/RJdkHidden.java:123,:143` — `(int) st.invokeExact(2)`
  on a hidden class.

---

## 4. What flipping actually buys — and what it does not

`CRATONVM_MH_STRICT_INVOKEEXACT=1` gets the run past `:153` and prints
`CK RJdkHandles adapt=14`; W4-1 then fixed the next wall (`publicLookup` access
modes) and expects `varHandles()` to be next.

### 4a. The 51 checks, and which of them have never run

Counted from source against the HotSpot log (`checks=51`):

| block | checks | status |
|---|---|---|
| `lookupAndInvoke()` L57-94 | 10 (L62,63,64,70,75,80,83,86,88,92) | **all pass today** — the run prints `CK RJdkHandles invoke ok` |
| `adaptation()` L96-159, up to L147 | 11 (L102,105,109,113,117,119,125,133,138,142,147) | **all pass today** — the run reaches L152 |
| `adaptation()` L149-158 | 1 (L157; L153 is the `unreachable` guard and must **not** execute) | **today's failure.** Needs the strict check |
| `accessChecks()` L181-224 | 6 (L190,201,211,218,220,222) | never executed; W4-1 fixed this block in source, unverified against a binary |
| `varHandles()` L226-289 | 23 (L231,233,234,235,236,237,238,240,242,243,244,245,248,251,252,255,261,262,269,279,283,284,285; L275 is an `unreachable` guard) | **never executed** |

10 + 11 + 1 + 6 + 23 = 51. (The block the next lane inherits is **23** checks,
not 21.)

### 4b. Verdict on `varHandles()`: it will NOT pass

`varType()` and `coordinateTypes()` have **no implementation anywhere in this
tree.** A repo-wide search for either identifier returns exactly two hits —
`RJdkHandles.java:244` and `:245`, the assertions themselves. There is no
native, no shim, no Rust-side handler.

Our `VarHandle` is a synthetic object with a private slot layout
(`VH_KIND`, `VH_IS_STATIC`, …; `native-builtins/src/phases_late/reflect_invoke.rs:690`).
With no native registered, `vi.varType()` falls through to the real JDK's
bytecode, which resolves through the `VarForm` we never populate. That is the
"native-backed state is invisible to real-JDK bytecode" shape, and it lands on
a `null`, not on a wrong answer.

So the honest expectation for the flip is: **`RJdkHandles` stays red, and its
failure moves from `adaptation():153` to `varHandles()`, at the latest by
`:244`.** The strict-corpus failure count does not move from 4.

One `varHandles()` risk that looked like a repeat of `:153` is **not** one:
L274's `String bogus = (String) vi.get(h)` must throw. `vh_get_plain_impl`
returns `vh_auto_box(ctx, val)` (`reflect_invoke.rs:721`), so the value on the
stack is a boxed `Integer`, the `checkcast java/lang/String` raises
`ClassCastException`, and L276's catch covers `ClassCastException`. That check
should pass. `varType`/`coordinateTypes` are the wall.

### 4c. So why flip now rather than after the `varType` lane?

Because the `varType` lane is orthogonal and **cannot see its own failures
until this flag is on.** Today `varHandles()` is unreachable: 23 checks that
have never executed once, in either mode, in this campaign. Flipping makes them
visible. Holding the flip until `varType` lands means that lane works blind and
then needs this same A/B afterwards anyway.

That is still an argument to flip **now** rather than after: the `varType`
lane is orthogonal, and it cannot see its own failures until this flag is on.

---

## 5. The flag-free fix (recommended follow-up, not implemented here)

The refutation in §2a is removable, and removing it removes the need for the
flag entirely. The predicate becomes:

> throw iff (conditions 1, 3-7) **and** the handle's *effective* return type —
> read from its `type` field's `rtype` mirror, i.e. what
> `lang_invoke.rs::mh_type_descriptor` prefers over `MH_DESC` — is **known**
> and differs from the call-site return char `R`.
> If `type` is absent or unreadable, do **not** throw.

* Case (a) → `type().returnType() != R` → throws, matching HotSpot exactly.
  `RJdkHandles:152` still throws: the run's own `CK` line proves
  `add.type()` is `(int,int)int`, so `rtype` is `int` ≠ `J`.
* Case (b) → `type().returnType() == R` → never throws. The mismatch is known
  to be our own stale `MH_DESC`, and the value can be numerically coerced
  instead of zeroed.

This cannot fire on a correct value under any input, so it needs no flag and
the flag can then be **deleted**.

It was **not implemented in this lane**, deliberately: it needs the receiver
plumbed into `unbox_poly_return_checked` at all four sites plus a new
mirror→primitive-char reverse lookup over `shared.classes.primitive_mirrors`,
in the return funnel of every signature-polymorphic `invokeExact`, and this
lane can neither build nor run. It also makes a check whose purpose is
catching a stale model depend on a second field of that same model. It wants
its own lane with a binary in hand.

---

## 6. The flip

Two files. `docs/flag-tokens.md` and `types/tests/flag-surface.txt` do **not**
change (neither the token nor the variable name moves), which is the
consistency set that reddened `flag_docs_generated` earlier this campaign.

**(i) `types/src/flag_groups.rs`** — give the entry an off-word, matching the
`jboss-logger-level-filter` / `tomcat-mapper-natives` precedent:

```
off_word: None   ->   off_word: Some("0")
```

**(ii) `vm/src/vm/vm_exec.rs::mh_strict_invokeexact`** — invert the read to the
house default-on idiom (`native-builtins/src/logmanager.rs:2565`):

```rust
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_MH_STRICT_INVOKEEXACT").as_deref(),
            Ok("0")
        )
```

**(iii) regenerate**, do not hand-edit: `tools/flag-census/render-inventory.sh`.
`render-inventory.py:128` derives the row from `off_word is not None`, so
`docs/config/flag-inventory.md:1189` becomes `default-on | on`. Hand-editing it
and getting the derivation wrong is precisely how `flag_docs_generated` goes
red.

The doc comments at `vm_exec.rs:1485-1497` and W3-1's §*The flag* both say
"**Default off**" and must be corrected in the same commit.

---

## 7. How to close it (A/B)

`CRATONVM_MH_STRICT_INVOKEEXACT=1` on the **current** binary is arm B; no
rebuild is needed to run the A/B, because the flag already exists. Run the A/B
*first*, then apply §6.

```
# A (default today) vs B (strict) — interleave A-B-B-A on a shared host.
export JDK25="/path/to/jdk-25"
A() { target/release/cratonvm "$@"; }
B() { CRATONVM_MH_STRICT_INVOKEEXACT=1 target/release/cratonvm "$@"; }
```

The vectors that must be compared A vs B, ordered by what they can actually
detect. Vectors 1-2 are the target and the floor; 3 is the claim that settles
W3-1's original objection; 4-5 are the only two that can reach the fire set.

| # | vector | command | pass condition |
|---|---|---|---|
| 1 | `RJdkHandles`, both modes | `A --real-jdk -cp regression-suite/classes RJdkHandles` and the same with `--jdk-only`; then `B` likewise | A dies at `:153`; **B reaches `CK RJdkHandles adapt=14`, then dies in `varHandles()`** (see §4 — *not* `PASS`) |
| 2 | the whole strict corpus, both modes | `regression-suite/run.sh` with `JDK_ONLY=1`, then again under `B` | identical failure set A vs B, except `RJdkHandles`' line number |
| 3 | Groovy | `scripts/smoke/ri14_groovy.sh` under A, then B | `GROOVY_SMOKE_OK` in both; byte-identical output |
| 4 | **Netty** — the `Z` case | `scripts/smoke/ri11_netty_echo.sh` under A, then B | echo completes in both; byte-identical output |
| 5 | in-repo primitive-return `invokeExact` | `cargo test -p cratonvm-vm --test interpreter_tests` (covers `MethodHandleTest.java:225`), and `RJdkHidden` from vector 2 | identical A vs B. **The flag latches from the process environment**, so arm B means exporting `CRATONVM_MH_STRICT_INVOKEEXACT=1` for the whole `cargo test` process — an in-test `set_var` is invisible |
| 6 | Jackson 2, breadth | `scripts/smoke/ri5_jackson.sh` under A, then B | `JACKSON_SMOKE_OK` in both |

Interleave **A-B-B-A** on a shared host.

**Not runnable here, and not blocking:** the H2, Spring, Spring Boot, Hibernate,
Tomcat, Keycloak and Elasticsearch runners all exist under `apps/`, but their
source checkouts are absent from this worktree. Run them in the A/B only if a
host already has the checkouts; §3a says every one of them is structurally
excluded from `F` (reference returns), so their absence does not block the flip.

**Not coverable at all:** Panama primitive-return downcalls (§3a). There is no
Java fixture in the tree that can exercise them. Accept this as a known blind
spot of the A/B, not as something a vector will close.

**The shape to grep for in every B log:** a `WrongMethodTypeException` at a site
where the A log produced a zero, a `false`, or no diagnostic at all. One such
hit on vectors 3-6 refutes the flip and the flag stays off.

---

## 8. Falsifying observation

**One:** arm B fails `RJdkHandles` at **line 157**
(`invokeExact with the wrong descriptor must throw…`) instead of getting past
`adaptation()`. That would mean the `invokeExact` native returned a raw
`Value::Int` rather than a boxed `Integer`, so condition 4 never held,
`coerce_value_against_ret_char` widened `Int(3)` to `Long(3)`, and `bogus == 3`
succeeded — i.e. the value-shape check has nothing to fire on and the whole
design needs the handle-descriptor comparison of §5 instead.

`auto_box_return`'s `Ok(Some(val)) => Ok(Some(box_value(ctx, val, ret_desc)))`
arm (`lang_invoke.rs:8591`) says this will not happen, but it is the one
observation that would settle it, and it is free — it is already in vector 1.
