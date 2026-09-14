# F1-1 — The boxing caches were three of six, and no two of the six share a bound

**2026-08-13, lane F1.** Fixes `Character.valueOf` / `Byte.valueOf` /
`Short.valueOf` in `native-builtins/src/lang_math.rs`, which is the only source
file this lane edited (plus this record). **This lane did not build or run
CratonVM**; every "before" below is a measurement the orchestrator supplied or a
fact read out of the tree, and every CratonVM "after" is explicitly PREDICTED.
Every HotSpot value was MEASURED on this host against
`openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` (Microsoft build) from
`scratchpad/f1/BoxOracle.java`, `IcHigh.java` and `ReflBox.java` before any of it
was written down.

---

## 1. Verdict

| | |
|---|---|
| the reported failure | `RJdkIntrinsics2 --only=charcls`, check **#161 of 167**: `Character.valueOf('a') == Character.valueOf('a')` |
| root cause | `native_character_value_of` allocated a fresh object on **every** call — there was no `CharacterCache` at all |
| family members audited | **8** (`Integer` `Long` `Boolean` `Character` `Byte` `Short` `Float` `Double`) |
| members that were **wrong** | **3** — `Character`, `Byte`, `Short`. All three allocated unconditionally. |
| members that were **right** and were NOT touched | **5** — `Integer`, `Long`, `Boolean` (cached, correctly) and `Float`, `Double` (uncached, correctly) |
| distinct bounds in the family | **5** (`0..127`, *all 256*, `-128..127`, *exactly two*, *nothing*) — the differences are the specification |
| checks predicted to flip in `RJdkIntrinsics2 --only=charcls` | **1** (#161), with **#162–#167 predicted to PASS**, i.e. the family reaches `sectionEnd("charcls", 167)` |
| checks predicted to flip in `RJdkIntrinsics3 --only=boxid` | **4** — and that family is predicted to be **aborting at its check #9 of 69 today**, so 60 further checks become reachable (§5.2) |
| measured divergences recorded but deliberately NOT fixed | **2** (§6) |
| NOMINATIONS raised | **3** (§7) |

**The reported failure was not the only red row this defect causes, and
`charcls` was not the family that would have hurt most.** `RJdkIntrinsics3`'s
`boxid` family — 69 checks, and the **second** family it runs — asserts the
identity contract for all six cached types and is predicted to be dying nine
checks in. §5.2.

## 2. The oracle spoke first

`scratchpad/f1/BoxOracle.java` asserts nothing and only prints. It does not
sample the boundaries — it **walks** them: all 65,536 code units, all 256 byte
values, and `Short.MIN_VALUE..=Short.MAX_VALUE`.

```
Char.valueOf(0) id        = true      Char.valueOf(128) id     = false
Char.valueOf('a') id      = true      Char.valueOf(255) id     = false
Char.valueOf(126) id      = true      Char.valueOf(65535) id   = false
Char.valueOf(127) id      = true      Char.valueOf(128) eq     = true
Character first NON-identical code unit = 128
autobox char 'a' id       = true      autobox char 200 id      = false

Byte ALL -128..127 identical = true   Byte.valueOf(-128) id    = true
Short cached range           = -128..127
Short.valueOf(128) id  = false        Short.valueOf(-129) id   = false
Integer cached range (default) = -128..127     Integer.valueOf(-129) id = false
Long cached range              = -128..127
Boolean.valueOf(true)==TRUE = true    Boolean.valueOf(false)==FALSE = true

Float.valueOf(0f) id  = false         Float.valueOf(1f) id  = false
Double.valueOf(0d) id = false         Double.valueOf(1d) id = false
Float.valueOf(0f).equals = true
```

Note the last line. **Every one of the `false` rows above is still `true` under
`.equals`.** An equality-shaped assertion passes against the exact defect this
record is about, in both directions, which is why the fixture row says
"IDENTITY, not equality" and why the new Rust tests below assert `assert_ne!` on
`ObjectRef` and not on the payload.

`autobox char 'a' id = true` is the part that makes this not a conformance
curiosity: `javac` compiles `Character c = 'a'` to `invokestatic
Character.valueOf(C)`, so on the pre-fix VM `a == b` was **false** for two
autoboxed ASCII chars in ordinary application code.

## 3. The bounds, from `jdk25src`, and why applying one of them twice is a bug

| type | JDK 25 source | bound | CratonVM before |
|---|---|---|---|
| `Character` | `if (c <= 127) return CharacterCache.cache[(int)c]; return new Character(c);` | `0..=127` — **no negative half, no `+128` offset** (`char` is unsigned) | **none** |
| `Byte` | `return ByteCache.cache[(int)b + 128];` | **unconditional** — all 256, there is no fresh arm | **none** |
| `Short` | `if (sAsInt >= -128 && sAsInt <= 127) return ShortCache.cache[sAsInt + 128];` | `-128..=127` of 65,536 | **none** |
| `Integer` | `IntegerCache.cache[i + (-IntegerCache.low)]` | `-128..=high`, `high` settable (§6.1) | `-128..=127`, correct at the default |
| `Long` | `if (l >= -128 && l <= 127)` | `-128..=127` | correct |
| `Boolean` | `return b ? TRUE : FALSE;` | exactly two, and they must be the **static fields** | correct (and the reason why is already documented in `native_boolean_value_of`) |
| `Float` / `Double` | `return new Float(f);` | **nothing** | correct |

Five different rules over eight types. The trap here is symmetry: `Character`'s
`0..=127` looks like `Short`'s `-128..=127` written badly, `Byte`'s rule looks
like a missing range check, and `Float`/`Double` look like the two members
somebody forgot. Any of those three readings, applied, is a regression. The
comment block now sitting above `CHARACTER_CACHE` states all five bounds
together, including the two that are "no cache", so the next reader has to
disagree with a written-down measurement rather than with an absence.

## 4. The fix

All of it in `native-builtins/src/lang_math.rs`:

1. `CHARACTER_CACHE` (128 slots, indexed by the code unit — no offset),
   `BYTE_CACHE` (256, `b + 128`), `SHORT_CACHE` (256, `s + 128`).
2. `cached_wrapper_box` — the canonical-instance dance once instead of three
   more copies. It drops the lock across `alloc_wrapper` (which can run
   `<clinit>` and can GC) and re-checks under the lock afterwards, so two
   threads that miss together still agree on which instance is canonical.
   It additionally **declines to cache** an object whose `class_id_of_object`
   is the `ClassId(0)` fallback: these caches are process-global and never
   invalidated, so installing a bootstrap-window fallback would latch a
   wrong-classed instance as THE canonical box for the life of the VM. Declining
   degrades to the pre-fix behaviour for that value instead of inventing a new
   failure mode.
3. `native_character_value_of` / `native_byte_value_of` / `native_short_value_of`
   route through it, each keeping its own bound and its own uncached arm.
   `Character`'s and `Short`'s uncached arms are load-bearing — HotSpot returns
   fresh objects above the bound and this VM has to as well.
4. `gc_scan_value_of_cache_roots` and `gc_update_value_of_cache_refs` grew from
   three caches to six. The two hooks were three copy-pasted blocks each; they
   are now `scan_one_cache` / `update_one_cache` called six times. That is not
   tidying: **a cache added to the root scan and forgotten in the remap is a
   use-after-move**, invisible under a non-moving collector, and the canonical
   instances are by construction the longest-lived objects in the heap. Six
   parallel one-liners make the omission visible; six 12-line blocks do not.

Five Rust tests were added next to the existing wrapper tests. Four are the
family: `Character` canonical through 127 **and fresh from 128**, `Byte`
canonical for all 256, `Short` canonical exactly on `-128..=127`, and a
**negative control** asserting `Float`/`Double` are `assert_ne!` — the test that
fails if a later lane "completes" the family. The fifth boxes a char, asserts
`gc_scan_value_of_cache_roots` reports it, remaps it through a `PointerMap`, and
asserts the next `valueOf` returns the **new** address; it fails if either hook
is missed, which is the pairing §4.4 is about.

Each test claims its own `vm_identity` (`0x5101`..`0x5105`). The caches are
process-global and the mock default identity is `0`, shared by every other test
in the suite; entries dangle once a mock heap drops, so leaving them under `0`
would hand a dangling `ObjectRef` to an unrelated test.

## 5. What should move, PREDICTED

### 5.1 `RJdkIntrinsics2 --only=charcls`

`charcls` is 167 checks. The `Character.valueOf` row is **#161** — counted from
the file, not from the header (the fixture-integrity loop at the top contributes
33, not 1; 167 reconciles exactly, so the header's tripwire and this count
agree).

* **#161 flips to PASS.** It is the only check in the family that touches a
  boxing cache.
* **#162–#167 — the six checks that had never executed on any CratonVM binary —
  are predicted to PASS.** They are `isEmoji(U+1F600)`, `isEmoji('#')`,
  `isEmojiPresentation(U+1F600)`, `!isEmojiPresentation(U+2764)`,
  `isExtendedPictographic(U+2764)`, `isEmojiComponent('#')`.

The reasoning, since these are predictions and not measurements:

Four of the six are answered by tables in `lang_math.rs` that were generated by
walking the JDK itself, and the required code points are in them by inspection:
`JAVA_EMOJI_RUNS` contains `(0x1F5FA,0x1F64F)` and `(0x0023,0x0023)`;
`JAVA_EMOJI_PRESENTATION_RUNS` contains `(0x1F5FB,0x1F64F)` and — the row that
matters — goes `…(0x2757,0x2757), (0x2795,0x2797)…`, so `U+2764` is **absent**
and the negated check holds; `JAVA_EMOJI_COMPONENT_RUNS` contains
`(0x0023,0x0023)`.

`isExtendedPictographic` is the interesting one: **it is not registered as a
native at all.** So is `getType`, `isMirrored`, `getDirectionality`,
`reverseBytes`, `compare`, `isIdeographic`, `isDefined`, `isAlphabetic`,
`toTitleCase` and `toChars` — and every one of those is asserted by `charcls`
**earlier** than #161 and therefore already passed on the measured binary. The
unregistered-static fallback into real JDK bytecode demonstrably works in this
configuration, and `CharacterDataLatin1`/`CharacterData00`'s
`isExtendedPictographic` is `(getPropertiesEx(ch) & 0x0800) != 0` — the same
`getPropertiesEx` load that `isDefined` and `isMirrored` already went through.
Measured on HotSpot: `isExtendedPictographic(U+2764) = true` while
`isEmojiPresentation(U+2764) = false`, which is the pair of answers the fixture
pins.

**If a check does surface behind #161, #167's neighbourhood is not where to look
first — #166 `isExtendedPictographic` is, because it is the only one of the six
with no in-tree table behind it.** In a mode where `java/lang/Character` has no
bytecode, #166 is a `NoSuchMethodError`, not a wrong answer.

### 5.2 `RJdkIntrinsics3 --only=boxid` — the family this defect actually guts

`boxid` is 69 checks and is `FAMILIES[1]`, so it runs second and is very likely
reached. `ckB` throws on the first mismatch, exactly like `charcls`'s `check`.
Its first fourteen rows are the caches, in this order:

| # | row | expects | pre-fix, PREDICTED | post-fix, PREDICTED |
|---|---|---|---|---|
| 1–5 | `Integer.valueOf` 127 / -128 / 128 / -129 / MIN | t t f f | pass | pass |
| 6–8 | `Long.valueOf` 127 / 128 / MIN | t f | pass | pass |
| **9** | `Byte.valueOf(-128) identity` | **true** | **FAIL — aborts the family here** | **PASS** |
| **10** | `Byte.valueOf(127) identity` | **true** | never runs | **PASS** |
| **11** | `Short.valueOf(-128) identity` | **true** | never runs | **PASS** |
| 12 | `Short.valueOf(128) identity` | false | never runs | pass — and this is the row that fails if the fix over-caches `Short` |
| **13** | `Character.valueOf(127) identity` | **true** | never runs | **PASS** |
| 14 | `Character.valueOf(128) identity` | false | never runs | pass — the over-caching guard for `Character` |
| 15–16 | `Boolean.valueOf` is `TRUE`/`FALSE` | t t | never runs | pass |

So: **4 rows flip red-to-green (#9, #10, #11, #13), and #10–#69 — sixty checks,
including the whole `equals`-is-type-sensitive, `hashCode`, `toString` and
lone-surrogate surface — become reachable for the first time.** Those sixty are
unaudited by this lane and are where a surprise, if there is one, will come
from. The `Double`/`Float` `toString` rows (shortest-round-trip) and
`Character.valueOf((char) 0xd800).toString()` (a lone surrogate as a
one-character `String`) are the two clusters with the most obvious way to be
wrong on a Rust-backed VM.

Note that #12 and #14 pass **today, vacuously** — a VM with no cache at all gets
every "must NOT be identical" row right for free. They only start carrying
information once the caches exist, which is now.

The prediction that #9 is where `boxid` dies is deductive, not measured: pre-fix
`native_byte_value_of` called `alloc_wrapper` unconditionally, so
`Byte.valueOf((byte) -128) == Byte.valueOf((byte) -128)` cannot have been true.

### 5.3 Everything else

Nothing else is predicted to move. `Float`/`Double` identity is asserted
**nowhere** in `regression-suite/src` (§7 N2) — that is the one gap this defect
family still has, and it is the gap on the over-caching side.

## 6. Measured, recorded, deliberately NOT fixed

### 6.1 `Integer.valueOf` ignores `java.lang.Integer.IntegerCache.high`

`scratchpad/f1/IcHigh.java`, HotSpot 25.0.3+9:

```
                                  (no flag)          -Djava.lang.Integer.IntegerCache.high=1000
Integer cached range          = -128..127            -128..1000
Long cached range             = -128..127            -128..127     <- unchanged, correctly
System.getProperty(...)       = null                 null          <- the JDK reads it via VM.getSavedProperty
```

`native_integer_value_of` hard-codes `-128..=127`. Because the native
**intercepts** `Integer.valueOf`, the JDK's own `IntegerCache.<clinit>` never
gets to widen anything, so a program started with that flag sees identity break
above 127 on this VM and hold on HotSpot.

Not fixed here, on purpose. The property is off by default, no test in the tree
exercises it, and honouring it means replacing `Integer`'s fixed
`[Option<ObjectRef>; 256]` with a resizable table plus its GC-hook consequences
— i.e. rewriting the one integral member that was already correct, in a lane
whose brief was the member that was not. Raised as N3.

### 6.2 Reflection boxing bypasses every cache — but NOT uniformly on HotSpot

`scratchpad/f1/ReflBox.java`, HotSpot 25.0.3+9:

```
Field.get(char 'a')  identity = true       Field.get == Character.valueOf('a') = true
Field.get(char 200)  identity = false
Field.get(int 7)     identity = true
Field.get(byte 3)    identity = true
Field.get(short 9)   identity = true
Field.get(float 1f)  identity = false
Array.get(char[] 'a') identity = false     <- the row that forbids the obvious fix
```

`lang_class::box_value` calls `alloc_wrapper` directly for all eight
descriptors, so it bypasses `INTEGER_CACHE`, `BOOLEAN_CACHE`, `LONG_CACHE` and
now the three added here. Its own doc comment says
`"Value::Int(42) with type \"I\" -> Integer.valueOf(42) object"` — the comment
names the right callee and the body does not call it.

The reason this is a nomination and not a patch is the last row. `Array.get`
allocates fresh on HotSpot while `Field.get` returns the canonical box, so
routing *all* of `box_value` through the caches would fix `Field.get` and break
`Array.get`. That call-site split belongs to whoever owns `lang_class.rs`.
Raised as N1.

## 7. NOMINATIONS

### N1 — `native-builtins/src/lang_class.rs`: `box_value` bypasses the caches its own doc names

**Evidence:** §6.2. `Field.get` on a `char`/`int`/`byte`/`short` field returns the
canonical box on HotSpot and a fresh object on CratonVM; `Array.get` returns a
fresh object on **both**, so the fix is not "route everything through the
caches".

Suggested shape (the owning lane decides the call-site split, which is the part
this lane cannot measure without running the VM): give `box_value` a sibling
that delegates to the registered natives rather than to `alloc_wrapper` —

```rust
        "I" => {
            let obj = alloc_wrapper(ctx, "java/lang/Integer");
            ctx.set_field(obj, 0, value);
            Value::Object(Some(obj))
        }
```

becomes, in the cached sibling,

```rust
        "I" => crate::lang_math::native_integer_value_of(ctx, &[value])
            .ok()
            .flatten()
            .unwrap_or(Value::Object(None)),
```

and likewise for `J` `Z` `B` `S` `C` (`F` and `D` must keep calling
`alloc_wrapper` — see the `Float.valueOf` rows in §2). `native_*_value_of` are
already `pub(crate)`. Callers reached from `Field.get` / `Method.invoke` /
`MethodHandle` boxing take the cached sibling; `Array.get` keeps the current one.

### N2 — `regression-suite/src/RJdkIntrinsics3.java`: `boxid` covers six of eight; the two it omits are the two that forbid over-caching

**This nomination started out four times larger and was cut down by looking.**
The first draft proposed adding `Byte` / `Short` / `Character` identity rows to
`RJdkIntrinsics2`, on the assumption that `charcls` #161 was the family's only
assertion. It is not: `RJdkIntrinsics3.boxid` already pins `Integer` (both
ends, both sides), `Long`, `Byte` (-128 and 127), `Short` (-128 true, 128
false), `Character` (127 true, 128 false) and `Boolean` (against the static
fields). Adding those rows would have been duplicate work — the very trap
E39-1 exists to mark. §5.2 is the corrected picture.

What `boxid` genuinely does **not** have is an identity row for `Float` or
`Double`. It asserts their `hashCode`, `equals` and `toString` at length and
never asks whether `valueOf` is canonical. That is the one direction no test in
the tree can catch: a lane that reads the six caches in `lang_math.rs` and
"completes the family" to eight makes `Float`/`Double` wrong against HotSpot and
**every existing check still passes**. Measured in §2: `Float.valueOf(0f) ==
Float.valueOf(0f)` is `false` on HotSpot 25, while `.equals` is `true`.

Two rows, in `boxid`, next to the other cache rows (`boxid`'s denominator moves
69 -> 71):

```java
        // Float and Double cache NOTHING. This is the only pair of rows that
        // fails if a VM makes the boxing family "consistent" -- and both
        // values are still .equals-equal, so an equality-shaped row cannot
        // stand in for these.
        ckB("boxid:Float.valueOf(0.0f) identity",
                Float.valueOf(fZero) == Float.valueOf(fZero), false);
        ckB("boxid:Double.valueOf(1.0) identity",
                Double.valueOf(dOne) == Double.valueOf(dOne), false);
```

`fZero` and `dOne` are already declared and already used by `boxid`, so the
operands stay non-constant and `javac` cannot fold them.

### N3 — `java.lang.Integer.IntegerCache.high` (this lane's own file, deferred)

§6.1. Deferred rather than done because it means rewriting a correct member for
a non-default flag with no test behind it. Whoever picks it up: the JDK reads
the property through `VM.getSavedProperty`, not `System.getProperty` (which
answers `null` for it, as the transcript shows), and `Long` must **not** move.

## 8. Residuals

* The three new caches add at most 640 permanently-live wrapper objects per VM
  (128 + 256 + 256). They are reported as GC roots, so they are retained by
  design, exactly like the 512 `Integer`/`Long` slots already were.
* `Character.valueOf` is now handing the same object to every thread. That is
  the contract, and wrappers are immutable, but it does mean
  `synchronized (Character.valueOf('a'))` now contends VM-wide — as it does on
  HotSpot.
* Nothing in this record claims a CratonVM behaviour was observed. §5 is a
  prediction and is written so it can be falsified by one run.
