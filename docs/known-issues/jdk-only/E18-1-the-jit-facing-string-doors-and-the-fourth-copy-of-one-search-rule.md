# E18-1 — the JIT-facing `String` doors, and one JVMS search rule written four times

**Status: FIXED in `native-builtins/src/lang_string.rs` (lane E18, 2026-08-13),
which is the only file this lane owns and the only file it edited. UNBUILT and
UNRUN — this lane may not run `cargo` or the CratonVM binary, so every CratonVM
"after" below is marked PREDICTED. Every HotSpot column is measured, with
transcripts.**

Oracle: Microsoft OpenJDK 25.0.3+9 (`java -version` confirmed in-session).
Probes: `scratchpad/e18/E18Probe.java`, `CharSearch.java`, `Idx.java`,
`Idx2.java`, `Idx3.java`.

**One nomination is REQUIRED FOR THE TREE TO COMPILE** — see §7 N1. It is
six lines in `vm/src/jit/helpers.rs`, and the coupling is deliberate: see §1.3.

---

## 1. TASK 1 — `jit_string_to_lower_case` bypassed the null-`Locale` check

### 1.1 What it was

E8 gave `String.to{Upper,Lower}Case(Locale)` its JDK contract by making
`string_case_native` call `locale_arg_checked`, which distinguishes an ABSENT
`Locale` slot (the no-argument overload, "use the default") from a
PRESENT-and-null one (`NullPointerException`). `jit_string_to_lower_case` did
not go through `string_case_native`. It called `string_case_impl` — one layer
BELOW the check:

```rust
match string_case_impl(ctx, this, locale, true, true) { … }
```

So after E8, `s.toLowerCase((Locale) null)` threw while the method was
interpreted and answered the default-locale string once its caller tiered up.
**The same source line, two answers, switching at an iteration count nobody
controls.** That is the `aastore` store-check shape this session already paid
for once.

The oracle does not behave that way, and the probe says so rather than assuming
it — 200 000 warm iterations of `"AbC".toLowerCase((Locale) null)` on OpenJDK
25.0.3+9:

```text
200k warm toLowerCase(null): npe/ok  =>  200000/0
```

### 1.2 Why `locale_arg`'s doc was wrong, not just its callers

`locale_arg`'s doc claimed the JIT helper is a caller that "genuinely cannot
tell them apart" because it is handed an `Option` and never saw an argument
list. It can. The JIT binds exactly ONE descriptor to this helper —
`jit/src/lib.rs`'s `STRING_LATIN1_LOWER_DIRECT_FN` ladder requires

```text
java/lang/StringLatin1 . toLowerCase (Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;
```

whose third parameter is **mandatory**. "Absent" is not a state that call site
can be in, so a `None` arriving there is an explicit `null` and nothing else.
The arity rule is not being bypassed at this door; it is being *supplied*. The
doc has been rewritten to say so, because the false claim is what made the
bypass look principled.

### 1.3 The fix, and why it is compile-coupled

`jit_string_to_lower_case` now calls `string_case_native` — the exact function
the registered native calls — with the `Locale` reconstituted as a present
slot:

```rust
string_case_native(ctx, &[Value::Object(Some(this)), Value::Object(locale)], true, true)
```

One body, not two. Its return type changes from `Option<ObjectRef>` to
`MethodCallResult`, because **there is no other channel**: `NativeContext` has
no throw method, `JIT_SIGNALS`/`set_jit_pending_npe` are private to
`vm/src/jit/helpers.rs`, and the old signature could only report the JDK's NPE
as `None`, which that file turns into a `null` String — a different wrong
answer, not a fix.

The one-line predecessor also **silently discarded every `Err`**: any exception
raised inside the case mapping became a null return and then an NPE at the
caller's line, blaming the caller.

That signature change makes N1 mandatory for the crate to build, and that is on
purpose. A tier divergence that can be half-landed will be half-landed
(`[fix+fix≠]`, `[dup-fix]`); a compile error is the loudest available guard and
costs the owning lane six lines. **If you are landing this and N1 is not in the
tree, do not "fix" the build by restoring the old signature — that restores the
divergence.**

### 1.4 The rest of the JIT-facing surface

`jit_string_*` matches exactly one function in this file. The sweep therefore
went the other way, from `jit/src/lib.rs`'s intrinsic recognition back to the
natives it shadows:

| JIT door | shadows | null contract | bounds contract | verdict |
|---|---|---|---|---|
| `jit_string_latin1_to_lower_direct` → `jit_string_to_lower_case` | `native_string_latin1_to_lower_case` | **DIVERGED** | n/a | fixed, §1.3 + N1 |
| `JitIntrinsic::StringLength` / `StringIsEmpty` | `native_string_length` / `_is_empty` | null receiver → deopt | n/a | agree |
| `JitIntrinsic::StringCharAt` | `native_string_char_at` | null receiver → deopt | OOB → deopt → native's `SIOOBE` | agree |
| `JitIntrinsic::StringHashCode` | `native_string_hash_code` | null receiver → deopt | n/a | agree |
| `JitIntrinsic::StringCompareTo` | `native_string_compare_to` | null arg → deopt → native's NPE | n/a | agree |
| `JitIntrinsic::StringIndexOfStr` | `native_string_index_of_str` | null arg → deopt → native's NPE | n/a | agree |
| `JitIntrinsic::StringEquals` | `native_string_equals` | null arg → deopt → `false` | **no `instanceof String` test on EITHER side** | §3 |
| `JitIntrinsic::StringIndexOfChar` | `native_string_index_of` | n/a | **both mask to `ch & 0xFFFF`; the JDK does not** | §2, N2 |

The deopt-on-null design is why most of these agree: the JIT's inline bodies
route a null receiver / null argument / null backing array to the deopt stub, so
the contract is answered once, by the native. The two that do NOT are the two
where the JIT carries a *rule* rather than a *fast path* — and in both, the JIT
and the native are wrong **together**, so no test that diffs the tiers against
each other can see them. `[HS=oracle]`.

---

## 2. One JVMS rule, four implementations, four different sets of wrong answers

`String.indexOf(int ch)` does not narrow `ch` to a code unit:

```text
isLatin1() ? StringLatin1.indexOf(value, ch, …)   // if (!canEncode(ch)) return -1;
           : StringUTF16 .indexOf(value, ch, …)   // !isValidCodePoint(ch) -> -1
                                                  //  ch <  0x10000       -> scan for (char) ch
                                                  //  ch >= 0x10000       -> scan for the PAIR
```

In this file that rule was written **three** times and answered differently
each time; `jit/src/lib.rs` has a fourth:

| site | what it did | wrong for |
|---|---|---|
| `native_string_index_of` (`indexOf(I)`) | `(ch & 0xFFFF) as u16` | every supplementary `ch` |
| `native_string_index_of_from` (`indexOf(II)`) | `(ch & 0xFFFF) as u16` | same — and its own DOC said "matched via the surrogate pair", one line above the mask |
| `native_string_last_index_of_from` (`lastIndexOf(II)`) | `(ch & 0xFFFF) as u16` | same |
| `native_string_last_index_of_char` (`lastIndexOf(I)`) | `char::from_u32(ch)` + `encode_utf16` | gets the pair RIGHT; drops every LONE SURROGATE, because `char::from_u32` rejects `0xD800..=0xDFFF` |
| `jit/src/lib.rs` `StringIndexOfChar` | masks, and its comment calls that "bit-identical to native `String.indexOf(int)`" | every supplementary `ch` — see N2 |

The JIT comment is the `[1 of 10 callsites]` tell in reverse: it is *accurate*
about the native it copies and wrong about the JDK, and it was written by
reading the sibling instead of the oracle.

### 2.1 The measured rows

`scratchpad/e18/CharSearch.java` and `Idx3.java`, OpenJDK 25.0.3+9. `mixed` is
`"xзy𐐷z"` — a Cyrillic з (U+0437, which is `0x10437 & 0xFFFF`)
at index 1 and the real surrogate pair for U+10437 at index 3.

| call | HotSpot 25 | CratonVM (before) | after (PREDICTED) |
|---|---|---|---|
| `"abc".indexOf(0x10061)` | **-1** | `0` (finds `'a'`) | -1 |
| `"abc".indexOf(0x10061, 0)` | **-1** | `0` | -1 |
| `"abc".lastIndexOf(0x10061, 2)` | **-1** | `0` | -1 |
| `"abc".lastIndexOf(0x10061)` | -1 | -1 | unchanged |
| `"з".indexOf(0x10437)` | **-1** | `0` | -1 |
| `mixed.indexOf(0x10437)` | 3 | `1` (the masked half) | 3 |
| `mixed.indexOf(0xD801)` | 3 | 3 | unchanged |
| `mixed.indexOf(0xDC37)` | 4 | 4 | unchanged |
| `mixed.lastIndexOf(0xDC37)` | **4** | `-1` (`char::from_u32` said `None`) | 4 |
| `mixed.lastIndexOf(0x10437)` | 3 | 3 | unchanged |
| `mixed.lastIndexOf(0x10437, 4)` / `, 3` | 3 | `-1` (masked; з is at 1) | 3 |
| `mixed.lastIndexOf(0x10437, 2)` | -1 | `1` | -1 |
| `mixed.lastIndexOf(0x10437, -1)` | -1 | -1 | unchanged |
| `"￿q".indexOf(-1)` | **-1** | -1 | unchanged |
| `"￿q".indexOf(0xFFFF)` | 0 | 0 | unchanged |
| `"￿q".indexOf(0x1FFFF)` | **-1** | `0` | -1 |
| `"abc".indexOf(0x110000)` / `Integer.MIN_VALUE` | -1 | -1 | unchanged |
| `"aÿc".indexOf(0xFF)` | 1 | 1 | unchanged |
| `mixed.indexOf(0x10437, 4)` / `, 99)` | -1 | -1 | unchanged |
| `mixed.indexOf(0x10437, -5)` | 3 | `1` | 3 |
| `"abc".lastIndexOf('a', -1)` | -1 | -1 | unchanged |
| `"abc".lastIndexOf('a', 99)` | 0 | 0 | unchanged |
| `"".lastIndexOf('a')` / `, 0)` | -1 | -1 | unchanged |
| `"𐐷".indexOf(0x10437, 1)` | -1 | `-1` | unchanged |

The `-1` row is the one that pins the rule's SHAPE, and it is the reason a
"just narrow it" reading is wrong: `(char) -1` is `0xFFFF`, the receiver holds
`0xFFFF`, and HotSpot still answers `-1`. The gate is
`Character.isValidCodePoint`, evaluated **before** any narrowing. A fix that
wrote `if ch <= 0xFFFF { scan(ch as u16) }` without the `ch >= 0` half would
pass every other row in the table and fail that one.

### 2.2 The fix

`code_point_needle(ch) -> Option<CharNeedle>` is now the only place the rule is
written:

```text
ch < 0 || ch > 0x10FFFF   ->  None          (isValidCodePoint)
ch <= 0xFFFF              ->  one unit      (including an unpaired surrogate)
otherwise                 ->  the pair
```

and all four natives feed it into `index_of_units_from` /
`last_index_of_units_from`, two shared scanners. Those two also absorb the
`String`-needle overloads (`indexOf(String)`, `indexOf(String,int)`,
`lastIndexOf(String)`, and the package-private `indexOf([BBILjava/lang/String;I)I`
static helper), which had **three further** hand-rolled copies of the same loop
with three different bounds expressions. The static helper's used
`saturating_sub` where the public ones used a length guard, so a needle longer
than `srcCount` re-examined index 0 — reachable as a slice-range panic, which is
a Rust panic and therefore not a Java throwable.

`last_index_of_units` is now a one-line call into `last_index_of_units_from`
with `from = i64::MAX`, so "no `fromIndex`" and "a `fromIndex`" are the same
code. The `i64` is load-bearing and is the JDK's own reason: a NEGATIVE `from`
must find nothing even when the needle is present (the JDK's loop counter starts
below zero and never runs), and clamping it into a `usize` would find it.

### 2.3 The tests are written against `&[u16]`, deliberately

The new tests do not build a heap String. They cannot: `create_string(&str)`
cannot hold an unpaired surrogate, and the lone-surrogate rows are half the
contract. They assert measured JDK answers against the pure functions, in the
shape E8 used for `region_matches_short_circuits`. Deriving the expectations
from Rust's `char` instead is precisely the proxy-oracle trap E8 §4.2 records —
and `char::from_u32` being consulted at all is how the `lastIndexOf(0xDC37)` row
got its wrong answer in the first place.

---

## 3. `String.equals(Object)` had no `instanceof String` test — and reads by slot

`String.equals` is
`(anObject instanceof String aString) && …`. `native_string_equals` had no
equivalent, and everything below its early-outs reaches the ARGUMENT's `value`
field **by slot index** (`string_char_array` is `get_field(obj, 0)`).
CratonVM's own synthetic `StringBuilder` layout is `char[] value @0`
(`classloading/src/class_manager.rs`, `instance_fields(2)`), so a builder whose
backing array happens to be exactly as long as the receiver compared **equal**
to it.

That is reachable, not theoretical. Measured on OpenJDK 25.0.3+9:

```text
StringBuilder sb = new StringBuilder(3); sb.append("abc");
sb.length()/capacity   =>  3/3
"abc".equals(sb)       =>  false        (CratonVM: true, PREDICTED before)
"abc".contentEquals(sb)=>  true         (the method that IS supposed to say true)
```

`equals` is what every `Map` lookup and `List.contains` in the VM ultimately
calls, so the blast radius of a false positive is not confined to the call that
made it.

The fix is one integer compare, placed after the identity check:

```rust
if ctx.class_id_of_object(other) != ctx.class_id_of_object(this) {
    return Ok(Some(Value::Int(0)));
}
```

It is written against the **receiver's** class id rather than a resolved
`java/lang/String` id on purpose: `java/lang/String` is final and
bootstrap-defined, so every String in a VM shares it, no name lookup is needed
on this very hot path, and if the receiver's id were ever surprising, two
Strings would still agree with each other and no answer would change.

The JIT's `StringEquals` intrinsic has the same gap and is not this lane's file
— N3.

---

## 4. TASK 2 — the two residuals E8 left

### 4.1 Residual 1 was TASK 1. Done, §1.

### 4.2 Residual 2 — `String.getBytes(String charsetName)`: the record is half wrong, and the reason is `[dup nati]`

E8 recorded this as "`phases_early.rs` ignores the charset entirely and returns
the receiver's UTF-8 bytes … null charset name, wrong charset name and
`UnsupportedEncodingException` are all unhandled". That describes ONE of **two**
registrations of the same triple:

| site | body | charset | bad name | null name |
|---|---|---|---|---|
| `phases_early.rs:2035` (in `register_core_stdlib_extras`) | closure | ignored, always UTF-8 | returns bytes | returns bytes |
| `charset.rs:1271` → `native_string_get_bytes_named` | real | **honoured** | **`UnsupportedEncodingException`** | `IOException("UnsupportedEncodingException: ")` |

`NativeMethodRegistry::register` is **last-write-wins**, and
`register_core_stdlib_extras`'s own file already documents which mode reaches it,
for the `codePointAt` registration eleven lines below the `getBytes` one:

* **real-JDK / compatible mode** — `register_core_stdlib_extras` is not called
  at all (it is reached only through `register_enterprise_final_natives`), and
  `charset::register_real_charset_natives` is called directly from
  `vm/src/vm/vm_init.rs` (both real-JDK arms, `:2372` and `:3093`). So
  `native_string_get_bytes_named` owns the slot and E8's description does **not**
  apply.
* **synthetic-jdk mode** — `register_synthetic_overrides` calls
  `register_charset_natives` (`lib.rs:23973`) and
  `charset::register_real_charset_natives` (`:23977`) FIRST, then
  `register_enterprise_final_natives` (`:24021`) after them, so the
  `phases_early` closure overwrites the good body and IS the live answer. E8's
  description applies here, and only here.

Measured contract, OpenJDK 25.0.3+9, receiver `"héllo"`:

```text
getBytes("UTF-8")          [104, -61, -87, 108, 108, 111]
getBytes("ISO-8859-1")     [104, -23, 108, 108, 111]
getBytes("US-ASCII")       [104,  63, 108, 108, 111]      <- '?' substitution, not an error
getBytes("UTF-16BE")       [0,104, 0,-23, 0,108, 0,108, 0,111]
getBytes("utf8")           [104, -61, -87, 108, 108, 111] <- alias resolves
getBytes("no-such")        java.io.UnsupportedEncodingException: no-such
getBytes("")               java.io.UnsupportedEncodingException:        (empty message)
getBytes((String) null)    java.lang.NullPointerException               <- NOT UnsupportedEncoding
getBytes((Charset) null)   java.lang.NullPointerException
```

Nominated as N4 (synthetic-mode duplicate) and N5 (the null-name row, both
modes). Neither file is this lane's.

### 4.3 Residual 3 — `String.contentEquals`: confirmed, nothing to do

Grepped: there is no `contentEquals` registration anywhere in `native-builtins`,
`vm` or `jit`. Real bytecode runs, backed by this file's `getCoder`/`getValue`
`AbstractStringBuilder` natives. E8's recorded contract is confirmed —
`contentEquals((CharSequence) null)` and `contentEquals((StringBuffer) null)`
both throw `NullPointerException: Cannot invoke "java.lang.CharSequence.length()"
because "cs" is null`, unlike `equals`. **No edit, no nomination.**

One knock-on worth naming: `contentEquals(CharSequence cs)` short-circuits with
`if (cs instanceof String) return equals(cs);`, so §3's new type test is on its
path. It cannot change that path's answer — the `instanceof` has already
established both are Strings.

---

## 5. TASK 3 — the negative control, and whether a `strfmt` row can now move

**No. I do not believe any `strfmt` row can move, and here is the check rather
than the assertion.**

E8's stated meaning stands: `strfmt` exercises `String.format`, `formatted`,
`lines`, `indent`, `chars`, `repeat`, `replace`, `replaceAll`, `replaceFirst`,
`matches`, `String.valueOf((Object) null)` and `transform`, and every one passes
a non-null argument. This lane touched none of those bodies. What it touched
that `strfmt` can reach *indirectly* is the search family and `equals`, so each
substitution was checked for behavioural identity on the inputs `strfmt`
actually presents:

| change | can a non-null, BMP, in-range call see it? |
|---|---|
| `code_point_needle` | No. For `0 <= ch <= 0xFFFF` the needle is the same single code unit the mask produced. Only supplementary / negative / `>0x10FFFF` / lone-surrogate `ch` move, and `strfmt` passes none. |
| `index_of_units_from` replacing four loops | No. Empty-needle answer is `clamp(from, 0, len)` — identical to the old `from.max(0).min(len)` in both public overloads and the old `→ 0` in the one-arg form (whose `from` is 0). The `from >= len` early return the two-arg form had is subsumed: `start > max_start` gives the same `-1`. |
| `last_index_of_units_from` replacing two loops | No. `min(from, len - width)` with `width == 1` is the old `min(from, len - 1)`; `from < 0` still finds nothing; the empty-needle `len` answer is preserved in the `last_index_of_units` wrapper. |
| the static `indexOf([BBI…)` helper's `saturating_sub` → length guard | No, except that a needle longer than `srcCount` becomes `-1` instead of a **slice-range panic**. Strictly fewer aborts. |
| `indexOf(String,int)` reading through the lossless scratch instead of `ctx.read_string` | No, for any string without unpaired surrogates — which is every `strfmt` string. |
| `indexOf(String,int)` null argument → NPE | No. `strfmt` passes no null needle. |
| `equals` class-id test | No. Every `strfmt` and `bounds` `equals` is String-vs-String (checked: all `.equals(` sites in `RJdkIntrinsics2.java` compare to a `String` literal or to `nameOf(t)`), and two Strings always share a class id. |
| `jit_string_to_lower_case` | No. `strfmt` calls no `toLowerCase`, and for a non-null `Locale` the body is byte-for-byte the interpreted native's, memo included. |

The one place I would look first if a `strfmt` row DID move is `equals`, because
it is the only change on this list that can alter a comparison between two
*valid* objects, and it would mean `class_id_of_object` does not return a stable
id for Strings in some mode — which would be a real finding about the VM, not
about this patch. `transform` and the shared case helper — E8's two named
suspects — were not touched by this lane at all.

---

## 6. What should flip

**`--only=bounds`** — still **exactly one row**, E8's:
`RJdkIntrinsics2.java:1406`, `String.regionMatches with a null other must throw
NullPointerException`, fail → **pass** (PREDICTED). Section total 35. This lane
adds nothing to that section: `RJdkIntrinsics2.java` has no `String.indexOf` /
`lastIndexOf` row at all, and every `.equals(` in the file is String-vs-String.

**`--only=strfmt`** — **no row should flip.** §5. If one does, this patch has a
bug; start at `equals`.

**No other section is expected to move.** The changes that alter an answer for a
*valid* input are, exhaustively:

* `indexOf`/`lastIndexOf(int[,int])` for a supplementary, negative,
  out-of-range or lone-surrogate `ch` (§2) — deliberate;
* `indexOf(String,int)` for a null needle (§4 of E8's family, missed there) —
  deliberate, now NPE;
* `String.equals(Object)` for a non-String argument (§3) — deliberate, now
  `false`;
* `toLowerCase((Locale) null)` in compiled code (§1) — deliberate, now NPE,
  **and only once N1 lands**.

The highest-regression-risk item is `equals`, for the reason it is worth fixing:
any in-tree caller that was relying on a String matching a same-length
`StringBuilder` now stops matching. That is HotSpot's answer, but it is what a
regression looks like from the outside.

---

## 7. NOMINATIONS

### N1 — REQUIRED TO COMPILE. `vm/src/jit/helpers.rs`, `jit_string_latin1_to_lower_direct`

`jit_string_to_lower_case` now returns `MethodCallResult` (§1.3). This call site
must consume it. `handle_jit_dispatch_error` and
`STRING_LATIN1_LOWER_DIRECT_INFO` both already exist in this file; the shape is
copied verbatim from `jit_hashmap_get_direct`'s error arm at `:11472`. The
`ctx` scope is what lets `thread` be re-borrowed for the error path, same as
`:11455`.

Replace exactly:

```rust
    // A null / non-heap Locale means "default locale", exactly as the
    // interpreted native treats a missing argument.
    let locale_obj = if locale == 0 {
        None
    } else {
        vm.mem.heap.is_object_address(locale as usize)
    };
    let Some((thread, _guard)) = jit_thread_mut() else {
        return 0;
    };
    let mut ctx = crate::vm::NativeContextImpl {
        shared: vm,
        thread: &mut *thread,
    };
    let Some(result) = cratonvm_native_builtins::lang_string::jit_string_to_lower_case(
        &mut ctx, source, locale_obj,
    ) else {
        return 0;
    };
    thread.native_pending_return = Some(result);
    result.as_ptr() as i64
```

with:

```rust
    // A `locale` of 0 is an explicit `null` Locale ARGUMENT, not "no argument":
    // the only descriptor bound to this helper,
    // `StringLatin1.toLowerCase(Ljava/lang/String;[BLjava/util/Locale;)`, has a
    // MANDATORY third parameter, so "absent" is not a state this site can be
    // in. `lang_string::jit_string_to_lower_case` now goes through the same
    // `string_case_native` the interpreted native does and throws for it —
    // E18-1 §1. It used to sit one layer below that check and answer the
    // default-locale string, so the same source line threw while interpreted
    // and stopped throwing once its caller tiered up.
    let locale_obj = if locale == 0 {
        None
    } else {
        // A non-zero address the heap does not recognise gets the same
        // treatment `source` gets six lines above: bail, do not guess.
        let Some(obj) = vm.mem.heap.is_object_address(locale as usize) else {
            return 0;
        };
        Some(obj)
    };
    let Some((thread, _guard)) = jit_thread_mut() else {
        return 0;
    };
    // Scoped so `thread` can be re-borrowed for the error path below, exactly
    // as `jit_hashmap_get_direct` does.
    let outcome = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        cratonvm_native_builtins::lang_string::jit_string_to_lower_case(
            &mut ctx, source, locale_obj,
        )
    };
    let result = match outcome {
        Ok(Some(Value::Object(Some(object)))) => object,
        Ok(Some(Value::Object(None))) | Ok(None) => return 0,
        Ok(Some(_)) => return 0,
        // The JDK's NullPointerException for a null Locale arrives here. The
        // old code could only report it as `None`, i.e. as a null String.
        Err(error) => {
            return handle_jit_dispatch_error(vm, thread, error, &STRING_LATIN1_LOWER_DIRECT_INFO)
        }
    };
    thread.native_pending_return = Some(result);
    result.as_ptr() as i64
```

and, in the same function's doc comment, replace exactly:

```rust
/// Both entries delegate to `lang_string::jit_string_to_lower_case`, the same
/// implementation the interpreted native uses. They used to carry a private copy
/// of the ASCII/Unicode fold that ignored `locale` entirely, so a
/// `toLowerCase(TURKISH)` call silently changed its answer when its caller
/// tiered up — the interpreter said `tıtle`, the compiled code `title`.
```

with:

```rust
/// Both entries delegate to `lang_string::jit_string_to_lower_case`, the same
/// implementation the interpreted native uses. They used to carry a private copy
/// of the ASCII/Unicode fold that ignored `locale` entirely, so a
/// `toLowerCase(TURKISH)` call silently changed its answer when its caller
/// tiered up — the interpreter said `tıtle`, the compiled code `title`.
///
/// That fix was one layer too shallow: the delegate called `string_case_impl`,
/// BELOW the null-`Locale` check, so `toLowerCase((Locale) null)` threw
/// interpreted and answered the default-locale string compiled. Measured on
/// OpenJDK 25.0.3+9, the oracle throws on the 200 000th warm iteration too.
/// See `docs/known-issues/jdk-only/E18-1-the-jit-facing-string-doors-and-the-fourth-copy-of-one-search-rule.md`.
```

**Verified by this lane**: `handle_jit_dispatch_error` at `:9428` takes
`(&SharedVm, &mut JvmThread, MethodCallFailed, &JitInvokeInfo) -> i64`;
`STRING_LATIN1_LOWER_DIRECT_INFO` is declared at `:11244`; `Value` is already in
scope (used at `:11462`). **Not verified**: that no other caller of
`jit_string_to_lower_case` exists — grepped, this is the only one, but the
orchestrator should re-grep at merge time in case another lane added one.

### N2 — `jit/src/lib.rs`: the `StringIndexOfChar` intrinsic masks, and its comment says that is the JDK

The block at `jit/src/lib.rs:8714` (`("indexOf", "(I)I") => …
StringIndexOfChar`) is documented by the comment above it as

> `indexOf(I)` — scan for `(ch & 0xFFFF)` from index 0, bit-identical to native
> `String.indexOf(int)` (which likewise masks to a single code unit —
> supplementary code points match their masked low half, no surrogate
> special-casing).

Both halves are now false: the native no longer masks (§2), and the JDK never
did. `"abc".indexOf(0x10061)` is `-1` on HotSpot and `0` from this intrinsic,
so with this lane's fix landed the intrinsic becomes a **tier divergence** of
exactly the shape §1 removes.

Two ways to close it, and this lane cannot measure which is cheaper:

* **Cheapest and safest** — stop recognising `("indexOf", "(I)I")` and let the
  site take ordinary dispatch into the (now correct) native. One line deleted.
* **Keep the fast path** — emit the inline scan only when the compiler can
  prove `ch` is a BMP constant, and fall back otherwise. Requires a constant
  test this lane has not read.

The comment must change either way; leaving it is worse than either fix,
because it is the reason the rule was copied rather than measured.

### N3 — `jit/src/lib.rs` + a fixture: `StringEquals` has no argument type test

`("equals", "(Ljava/lang/Object;)Z") => JitIntrinsic::StringEquals` inlines a
String-layout decode of the ARGUMENT, guarded only against null (`jit/src/lib.rs:8692`
comment: "only null receiver / null argument / null backing array route to the
deopt stub"). §3 shows the native had the same gap and what it costs. The inline
body needs the same test the native now has — a class-id compare against the
receiver's — or the site should deopt when the argument's class id differs.

There is also **no fixture row** for it. Worth adding to `RJdkIntrinsics2.java`'s
`strnull` block if E8's N2 lands, or to `bounds` otherwise:

```java
        // A same-LENGTH non-String CharSequence: `new StringBuilder(3)` really
        // does have capacity 3 on OpenJDK 25.0.3+9, so an equals() that reads
        // the argument's value array by slot index matches it.
        StringBuilder sb3 = new StringBuilder(OPAQUE_I[6]);
        sb3.append("abc");
        check(!"abc".equals((Object) sb3),
                "String.equals(StringBuilder) must be FALSE — equals is guarded by"
                        + " `instanceof String`, and contentEquals is the method that says true");
        check("abc".contentEquals(sb3),
                "String.contentEquals(StringBuilder) must be TRUE — the sibling that DOES compare"
                        + " content across CharSequence types");
```

(`OPAQUE_I[6] == 3`, read off the declaration at `RJdkIntrinsics2.java:140-142`
and confirmed. Two rows, so the section total goes up by two.)

### N4 — `native-builtins/src/phases_early.rs:2035`: delete the duplicate `getBytes(String)`

In synthetic-jdk mode this closure overwrites `charset.rs`'s charset-aware
`native_string_get_bytes_named` (§4.2). Delete exactly:

```rust
    // --- String.getBytes(String charsetName) ---
    r.register(s, "getBytes", "(Ljava/lang/String;)[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = ctx.read_string(this).unwrap_or_default();
        let bytes = val.as_bytes();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    });
```

and replace it with a note in the same style as the `codePointAt` retirement
eleven lines below, so the next reader does not re-add it:

```rust
    // --- String.getBytes(String charsetName) — RETIRED, deliberately not
    //     registered here ---
    //
    // What used to be here encoded the receiver as UTF-8 whatever the
    // charsetName said, and returned bytes for an unknown name instead of the
    // checked UnsupportedEncodingException. `charset.rs`'s
    // `native_string_get_bytes_named` already honours the name AND throws.
    //
    // Same last-write-wins story as `codePointAt` below: in real-JDK mode this
    // function is never called, so the good body already owned the slot; in
    // synthetic-jdk mode `register_enterprise_final_natives` (lib.rs:24021)
    // runs AFTER `charset::register_real_charset_natives` (lib.rs:23977), so
    // this closure overwrote it and WAS the live answer. Removing it is a
    // behaviour change in synthetic-jdk mode only, to the body that honours
    // the charset. Measured, OpenJDK 25.0.3+9, "héllo":
    //   getBytes("ISO-8859-1")  [104, -23, 108, 108, 111]
    //   getBytes("US-ASCII")    [104,  63, 108, 108, 111]   ('?' substitution)
    //   getBytes("no-such")     java.io.UnsupportedEncodingException: no-such
```

The sibling `getBytes(Ljava/nio/charset/Charset;)[B` closure immediately below
it has the identical defect against `charset.rs:1268`'s
`native_string_get_bytes_charset` and should go the same way in the same edit —
`[no-op w/ excuse]`: diff a family fix against every member.

### N5 — `native-builtins/src/charset.rs`, `native_string_get_bytes_named`: a null name is an NPE, not an `UnsupportedEncodingException`

The remaining wrong row in real-JDK mode. Currently a null `args[1]` reads as
`String::new()`, so it takes the unknown-name exit and produces
`IOException("UnsupportedEncodingException: ")` — a *checked* exception a caller
may well be catching, standing in for an unchecked one it is not. Measured:
`"héllo".getBytes((String) null)` throws `java.lang.NullPointerException`.

Replace exactly:

```rust
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
```

with:

```rust
    // A null charsetName is `NullPointerException`, NOT the checked
    // `UnsupportedEncodingException` — measured on OpenJDK 25.0.3+9. The
    // defaulting `_ => String::new()` sent it to the unknown-name exit below,
    // turning an unchecked contract violation into a checked exception the
    // caller may be catching. An EMPTY name really is
    // `UnsupportedEncodingException` (with an empty message), so the two
    // cannot share an arm.
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
```

**Verified by this lane**: `charset.rs:22` is
`use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};`,
so the unqualified `RuntimeError::NullPointerException` is in scope — the same
form used at `:1147` (`RuntimeError::IOException`) inside this very function and
at `:1168`.

### N6 — `regression-suite/src/RJdkIntrinsics2.java`: the search family has no row

Nothing in the fixture calls `String.indexOf(int)` or `lastIndexOf(int)` at all,
which is why four disagreeing implementations of one JVMS rule survived. If E8's
N2 `strnull` block lands, these belong beside it; otherwise they are a `bounds`
addition. All measured on OpenJDK 25.0.3+9 (§2.1):

```java
        // E18: `indexOf(int)` does NOT narrow to a code unit. A supplementary
        // ch is matched as a surrogate PAIR, an invalid one matches nothing,
        // and the gate is Character.isValidCodePoint — checked BEFORE any
        // narrowing, which is why indexOf(-1) is -1 on a receiver holding
        // U+FFFF even though (char) -1 == 0xFFFF.
        String mixed = "xзy𐐷z";   // 0x10437 & 0xFFFF == 0x0437
        check("abc".indexOf(0x10061) < 0,
                "\"abc\".indexOf(0x10061) must be -1 — a MASKING implementation finds 'a' at 0");
        check("abc".lastIndexOf(0x10061) < 0,
                "\"abc\".lastIndexOf(0x10061) must be -1 — same rule, backwards");
        check(mixed.indexOf(0x10437) == 3,
                "indexOf of a supplementary code point must find the PAIR at 3, not its masked"
                        + " low half at 1");
        check(mixed.lastIndexOf(0xDC37) == 4,
                "a LONE low surrogate is an ordinary code-unit scan — an implementation built on"
                        + " a code-point type answers -1 here");
        check("￿q".indexOf(-1) < 0,
                "indexOf(-1) must be -1 even when the receiver holds U+FFFF: the gate is"
                        + " isValidCodePoint, not a narrowing cast to (char)");
        check(mixed.lastIndexOf(0x10437, 2) < 0 && mixed.lastIndexOf(0x10437, 3) == 3,
                "lastIndexOf(supplementary, from) starts at min(from, length - 2), not length - 1");
```

Six rows; adjust `sectionEnd` accordingly. The `indexOf(-1)` row is the one that
rejects a plausible wrong fix, so keep it if any are dropped.

---

## 8. What this lane did NOT do

* **Did not touch `case_map.rs`, `lang_math.rs`, `vm/src/jit/helpers.rs` or
  `jit/src/x64/bytecode_walk.rs`.** Read all four; edits to the latter two are
  N1/N2/N3.
* **Did not re-do E8's work.** `equalsIgnoreCase`, `compareToIgnoreCase`,
  `regionMatches`, `getChars`, `startsWith`, `split`, `join`, `transform` and
  the six A7 constants were re-read and spot-measured (`E18Probe.java` §E),
  and all agree with E8's record. `code_unit_eq_ignore_case` really is what
  `equalsIgnoreCase` now calls — checked the call graph, not the comment.
* **Did not build or run anything.** No `cargo`, no CratonVM binary. The file
  was parse-checked with `rustfmt --check`; it is clean of NEW formatting
  diffs (36 hunks before this lane, 36 after — all pre-existing, none in code
  this lane wrote).
* **Did not add a fixture row.** `regression-suite/` is not this lane's;
  N3 and N6 carry the text.
