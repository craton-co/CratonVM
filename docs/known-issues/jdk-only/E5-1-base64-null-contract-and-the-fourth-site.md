# E5-1 — the fourth Base64 null site, and what a whole-surface audit found behind it

**2026-08-13, lane E5.** Closes the measured `RJdkIntrinsics2 --only=b64`
failure

```
decode((String) null) must throw NullPointerException, got none
```

This lane **owns `native-builtins/src/lib.rs`**; everything else here is a
NOMINATION (§6).

**This lane may not build or run the VM.** Every CratonVM "after" below is
**PREDICTED**. Every HotSpot row was executed on this host against
`openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` from `scratchpad/e5/`
(`B64Surface.java`, `B64Canon.java`, `B64Ident.java`, `B64Unreg.java`,
`LineMax.java`).

---

## 0. Verdict

| claim | verdict |
|---|---|
| the failing site is `Base64$Decoder.decode(Ljava/lang/String;)[B` | **CONFIRMED** (§1) |
| it is a FOURTH site the three landed patches missed | **CONFIRMED** — and it failed differently from the other three (§1) |
| the malformed-input contract is already faithful | **CONFIRMED, mechanically** — 36/36 rows identical to HotSpot (§3) |
| there is a reachable Rust panic in the decoder | **NO** (§3.3) |
| "malformed input throws" is a single rule | **WRONG** — MIME ignores what BASIC rejects; 8 of 12 rows differ (§3.2) |
| `Ok(None)` VOID-shape returns | **5 found, 5 fixed** (§2) |
| the audit found a SECOND live defect beyond the brief | **YES** — `linemax`'s `-1` sentinel (§4) |

## 1. The fourth site, and why grep missed it

The brief's `grep -n '"decode".*Ljava/lang/String'` returned nothing because the
registration spans four lines:

```rust
registry.register(
    dec,
    "decode",
    "(Ljava/lang/String;)[B",
    native_b64_decode_string,
);
```

`scratchpad/p1/reg.json` settles ownership without guessing — all **11**
Base64 triples are registered by `native-builtins/src/lib.rs`, each with
`overwrote: null` and `owns_slot: true`, so there is no second registrar and no
shadowing. (The `registered_by` line numbers in that dump are ~18 lower than the
current file; it predates the three landed patches.)

**The defect was not the same shape as the other three.** The three already
patched returned `Ok(None)` for a null argument. This one did something worse:

```rust
let src_str = match args.get(1) {
    Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
    _ => String::new(),                      // <-- null defaulted to ""
};
```

A null argument was **defaulted to the empty string**, decoded successfully, and
returned a zero-length `byte[]`. That is why the fixture said `got none` rather
than reporting a wrong exception: the call **returned normally**. It is also the
one wrong answer a caller cannot detect, because it is byte-for-byte what a
legitimate `decode("")` returns — measured on HotSpot:

```
decode("").length                  = 0
decode(new byte[0]).length         = 0
```

This is the `[default=wrong write]` family: a defaulting reader turned a null
into a quiet, plausible, wrong answer.

## 2. The null contract across the whole surface, and the VOID shape

HotSpot 25 throws `NullPointerException` for a null argument on **every** one of
the 13 encoder/decoder combinations (`B64Surface.java`):

```
Encoder.encode((byte[])null)              -> java.lang.NullPointerException / Cannot read the array length because "src" is null
Encoder.encodeToString(null)              -> java.lang.NullPointerException / Cannot read the array length because "src" is null
Decoder.decode((byte[])null)              -> java.lang.NullPointerException / Cannot read the array length because "src" is null
Decoder.decode((String)null)              -> java.lang.NullPointerException / Cannot invoke "String.getBytes(java.nio.charset.Charset)" because "src" is null
UrlEncoder.encode((byte[])null)           -> java.lang.NullPointerException / Cannot read the array length because "src" is null
UrlEncoder.encodeToString(null)           -> java.lang.NullPointerException / Cannot read the array length because "src" is null
MimeEncoder.encode((byte[])null)          -> java.lang.NullPointerException / Cannot read the array length because "src" is null
MimeEncoder.encodeToString(null)          -> java.lang.NullPointerException / Cannot read the array length because "src" is null
Enc.withoutPadding().encodeTS(null)       -> java.lang.NullPointerException / Cannot read the array length because "src" is null
UrlDecoder.decode((byte[])null)           -> java.lang.NullPointerException / Cannot read the array length because "src" is null
UrlDecoder.decode((String)null)           -> java.lang.NullPointerException / Cannot invoke "String.getBytes(java.nio.charset.Charset)" because "src" is null
MimeDecoder.decode((byte[])null)          -> java.lang.NullPointerException / Cannot read the array length because "src" is null
MimeDecoder.decode((String)null)          -> java.lang.NullPointerException / Cannot invoke "String.getBytes(java.nio.charset.Charset)" because "src" is null
```

The message is a HotSpot *helpful-NPE* synthesised from the callee's
local-variable table, not a specified contract; `message: None` is left unset,
and the fixture asserts the TYPE (`nameOf(t)`).

**The VOID shape.** Separately from the null argument, all five instance natives
returned `Ok(None)` for a non-object receiver. Every descriptor in this family
returns a value (`[B`, `Ljava/lang/String;`, `Ljava/util/Base64$Encoder;`), so
`Ok(None)` hands the interpreter a value of the wrong kind. Five sites, all
fixed:

| native | descriptor returns | was |
|---|---|---|
| `native_b64_encode` | `[B` | `Ok(None)` |
| `native_b64_encode_to_string` | `Ljava/lang/String;` | `Ok(None)` |
| `native_b64_without_padding` | `Ljava/util/Base64$Encoder;` | `Ok(None)` |
| `native_b64_decode_bytes` | `[B` | `Ok(None)` |
| `native_b64_decode_string` | `[B` | `Ok(None)` |

Both rules now live in **one** place — `b64_receiver` / `b64_arg` — rather than
in five copied comment blocks, so the next overload added to this family cannot
get a *different* answer to the same question. That consolidation is the reason
this defect existed: three of four arms were patched and the fourth, which read
its argument through a different expression, was not.

## 3. The malformed-input contract — VERIFIED, not re-done

The brief said an earlier lane found `b64_decode` a faithful JDK port and asked
me to verify rather than re-do. Verified, and mechanically rather than by eye.

Because this lane may not run cargo, `b64_decode`/`b64_decode_char`/
`b64_illegal_char_msg`/`b64_encode` were transcribed line-for-line into
`scratchpad/e5/port.py`, and the same 12 malformed inputs were run through both
it and HotSpot (`B64Canon.java`) in an identical output format:

```
$ diff hotspot.txt rustport.txt
*** IDENTICAL: Rust decoder matches HotSpot 25 on all 36 malformed rows (12 inputs x BASIC/URL/MIME) ***
```

**No change was made to the decoder.** §3.1–§3.3 record what that diff pins.

### 3.1 One distinct message per shape, including two off-by-one details

```
BASIC  decode("QQ==X") -> IllegalArgumentException / Input byte array has incorrect ending byte at 4
MIME   decode("QQ==X") -> IllegalArgumentException / Input byte array has incorrect ending byte at 5
```

The index differs by one between modes because real JDK's
`if (isMIME && base64[src[sp++]] < 0)` only evaluates — and so only advances —
`sp` when `isMIME`. The port reproduces the short-circuit, not just the message.

```
BASIC  decode("A")  -> Input byte[] should at least have 2 bytes for base64 bytes
MIME   decode("A")  -> Last unit does not have enough valid bits
```

A 1-byte input is fatal in `decodedOutLength` for BASIC/URL but reaches
`decode0` for MIME, which reports the dangling unit instead.

### 3.2 "Malformed input throws" is the WRONG rule — the brief was right

The MIME decoder ignores illegal characters where BASIC rejects them. **8 of the
12 inputs differ between the two**, and 4 of those *succeed* under MIME:

| input | BASIC | MIME |
|---|---|---|
| `-_-_` | `Illegal base64 character 2d` | `[]` len=0 |
| `not valid base64` | `Illegal base64 character 20` | `[9e 8b 6f 6a 58 9d 6d ab 1e eb]` len=10 |
| `AB€D` | `Illegal base64 character 3f` | `[00 10]` len=2 |
| `ÿÿÿÿ` | `Illegal base64 character -1` | `[]` len=0 |
| `A` | `should at least have 2 bytes` | `Last unit does not have enough valid bits` |
| `=` | `should at least have 2 bytes` | `wrong 4-byte ending unit` |
| `QQ==X` | `incorrect ending byte at 4` | `incorrect ending byte at 5` |
| `QQ\n==` | `Illegal base64 character a` | `[41]` len=1 |

Note `-_-_`: it is valid under URL (`[fb ff bf]`), rejected under BASIC, and
silently **empty** under MIME — one input, three different correct answers.

Also pinned: `Illegal base64 character` renders the **signed** byte, because
real JDK passes a `byte[]` element to `Integer.toString(int, 16)` — `0xff` is
`-1`, not `ff`. And `decode(String)` is `decode(src.getBytes(ISO_8859_1))`, so
`AB€D` reports `3f` (`'?'`), not a UTF-8 continuation byte.

### 3.3 No reachable Rust panic

Audited every indexing and arithmetic site in `b64_decode`/`b64_encode`:

- `input[sp]` inside the `wrong_tail` check is guarded by the `sp == sl` arm;
- `input[i + 2]` is guarded by the `i + 2 < input.len()` loop condition;
- `bits |= b << shiftto` — `shiftto ∈ {18,12,6,0}` at that point (reset to 18
  whenever it goes negative) and `b < 64`, so the `u32` cannot overflow;
- `signed.unsigned_abs()` is on an `i32` widened from `i8` — no overflow;
- `input.len() - i` cannot underflow, `i` never exceeds `len`.

The two remaining defaulting readers are `b64_read_byte_array` (skips a
non-`Int` element, which would silently SHORTEN the array) and
`ctx.read_string(...).unwrap_or_default()`. Both are unreachable from Java —
the descriptors are `[B` and `Ljava/lang/String;`, so the verifier guarantees
the types — and neither is on the null path any more. Left as-is; noted so a
future reader does not mistake them for the same bug.

## 4. A SECOND defect the audit turned up: `linemax`'s `-1` sentinel

Not in the brief. `b64_encoder_variant` decided MIME with `linemax != 0`, but
the real JDK's non-MIME encoders do not use 0 — read off HotSpot with
reflection (`LineMax.java`):

```
getEncoder()                  linemax=-1   isURL=false  newline=null
getUrlEncoder()               linemax=-1   isURL=true   newline=null
getMimeEncoder()              linemax=76   isURL=false  newline=[13, 10]
getMimeEncoder(20,{'\n'})     linemax=20   isURL=false  newline=[10]
getEncoder().withoutPadding() linemax=-1   isURL=false  newline=null
```

`-1 != 0`, so **any `Encoder` that real bytecode constructed was classified
MIME** and encoded with phantom CRLF line breaks. Changed to `linemax > 0`,
which reads both sentinels as "no line breaks" and is **identical for every
encoder this file allocates** (whose linemax is 0 or 76) — so it cannot regress
the fabricated path.

Reachability is narrow but real: `Base64.getMimeEncoder(int, byte[])` is the one
factory that is *not* registered, so it runs real bytecode and returns a real
`Encoder`; a subsequent `encodeToString` is intercepted by our native. See §6/N2
— the `> 0` fix stops the misclassification but does **not** make us honour a
custom linemax, which remains a PREDICTED divergence.

## 5. What landed in `native-builtins/src/lib.rs`

The file was and remains **LF-only (0 CRLF)** — counted after every edit,
because a Windows-side paste that lands CRLF here is a standing hazard.

1. **`b64_receiver` / `b64_arg`** added — the single statement of the receiver
   rule and the null-argument rule, with the HotSpot transcript inline.
2. **Five natives** rewritten onto them, deleting five `Ok(None)` VOID returns
   and three copied comment blocks.
3. **`native_b64_decode_string`** — the fourth site; `String::new()` default
   replaced by `b64_arg(args)?`.
4. **`b64_encoder_variant`** — `linemax != 0` → `linemax > 0` (§4).
5. **`native_b64_without_padding`** — measured JDK semantics recorded inline
   (returns `this` when already unpadded; URL/MIME variant survives the copy).
6. **Two tests** in `mod base64_encoder_tests`:
   `null_argument_throws_npe_on_every_value_returning_overload` drives all four
   overloads × three variants × `withoutPadding()` and distinguishes the three
   wrong shapes (returned a value / `Ok(None)` / wrong exception) in its panic
   messages; `empty_input_still_decodes_to_an_empty_array` is the **negative
   control** that stops the fix degenerating into "throw on anything falsy" —
   it is exactly the answer the broken arm used to give.

Plus one nomination applied from lane E1 (§6/N1).

## 6. NOMINATIONS

Both targets are **LF-only**; apply with LF endings.

### N1 — APPLIED (from lane E1, `E1-1` §7)

`native-builtins/src/lib.rs:20352`. The comment *"Both concrete classes are
covered below"* was false after C12-1 removed `java/util/SimpleTimeZone`.
**Verified before applying, not taken on trust:** `"java/util/SimpleTimeZone"`
occurs **0 times** in `native-builtins/src/`, and
`register_tzdb_offset_natives_for` has exactly two callers —
`sun/util/calendar/ZoneInfo` and the abstract `java/util/TimeZone`. Replacement
text applied verbatim from `E1-1` §7.

E1's N2 (`regression-suite/src/RSimpleDateFormatZone.java`) is **not** this
lane's file and remains open.

### N2 — `native-builtins/src/lib.rs` is MINE, but these two need a build to land

Recorded rather than landed, because this lane cannot compile or run:

**(a) The fabricated `Encoder` has a null `newline`.** `b64_alloc_encoder`
allocates a 4-field synthetic and sets fields 1/2/3, leaving field 0
(`newline`) unset. The layout is deliberately aligned with the real JDK
(`newline, linemax, isURL, doPadding`), and real `encode0` reads it —
`143: getfield #13 // Field newline:[B` — whenever `linemax > 0`. So the four
unregistered encoder entry points reach a null field on a MIME encoder. HotSpot:

```
mime.encode(byte[],byte[]) -> 82        dst[76]=13 dst[77]=10   (CRLF from `newline`)
mime.encode(ByteBuffer)    -> 82
mime.wrap(OutputStream)    -> 82
basic.encode(byte[],byte[])-> 80
```

The fix is to write `byte[]{13,10}` into field 0 for the MIME variant. It is
**not** a one-liner: it introduces an allocation between
`try_alloc_concurrent_synthetic` and `set_field`, leaving one object unrooted
across a possible GC. It needs a `NativeHandleScope`, and it needs a run.

**(b) A custom `linemax` is not honoured.** Even with §4's fix,
`Base64.getMimeEncoder(20, new byte[]{'\n'})` is classified MIME and encoded
with hardcoded 76-char lines and CRLF. HotSpot gives **83** characters for the
60-byte fixture; CratonVM is PREDICTED to give **82**. A real fix means
threading `linemax` and the `newline` bytes through `b64_encode` instead of
hardcoding 76/CRLF.

**(c) The six factories are not singletons.** HotSpot returns the same object
every time (`getEncoder() == getEncoder()` is `true` for all six); ours
allocates a fresh one per call. No check depends on it and a process-global
cache would be wrong — it must be VM-scoped — so this is recorded, not fixed.

### N3 — `classloading/src/class_manager.rs`, synthetic-JDK mode only

The synthetic declarations match the 11 registered triples exactly (1:1, no
orphan declaration without a native — checked). But the real JDK's public
surface is **18** methods, so in synthetic-JDK mode these 7 are
`NoSuchMethodError`, with HotSpot answers measured for each:

| missing triple | HotSpot |
|---|---|
| `java/util/Base64.getMimeEncoder(I[B)Ljava/util/Base64$Encoder;` | 83 (20-char lines, `\n`) |
| `Base64$Encoder.encode([B[B)I` | 82 mime / 80 basic |
| `Base64$Encoder.encode(Ljava/nio/ByteBuffer;)Ljava/nio/ByteBuffer;` | 82 |
| `Base64$Encoder.wrap(Ljava/io/OutputStream;)Ljava/io/OutputStream;` | 82 |
| `Base64$Decoder.decode([B[B)I` | 60 |
| `Base64$Decoder.decode(Ljava/nio/ByteBuffer;)Ljava/nio/ByteBuffer;` | 60 |
| `Base64$Decoder.wrap(Ljava/io/InputStream;)Ljava/io/InputStream;` | 60 |

Low priority — nothing in the corpus is known to call them — but this is the
denominator, and `--only=b64` exercises none of it.

## 7. Which `--only=b64` checks flip — PREDICTED

`b64()` ends `sectionEnd("b64", 27)`. In
`regression-suite/src/RJdkIntrinsics2.java:734-815` there are **23 static
`check(...)` call sites**, one of which sits inside the 5-iteration `bad[]`
loop at `:783-792` — 23 − 1 + 5 = **27** at runtime, which is why a grep count
and the expected total disagree.

**Exactly one flips: check #23**, the null row at `:793-800`:

```java
Base64.getDecoder().decode((String) null);
...
check("java.lang.NullPointerException".equals(nameOf(t)),
        "decode((String) null) must throw NullPointerException, got " + nameOf(t));
```

- **before:** 26/27, failing `decode((String) null) must throw NullPointerException, got none`
- **after (PREDICTED):** 27/27

The other 26 must **not** move, and the two structural reasons they cannot:

- rows 2–22 and 24–27 all run through `b64_encode`/`b64_decode`, which this
  change **does not touch** (§3 diff);
- the closest row to the fix is #11, `decode("").length == 0`. `""` is a
  non-null `String` object, so `b64_arg` returns `Ok` and the empty input
  decodes as before — pinned by
  `empty_input_still_decodes_to_an_empty_array`.

`b64` is registered in the section table at `:1667` and dispatched at
`:1680-1681`, so `--only=b64` reaches it.

**Not covered by this fixture**, and therefore still unmeasured after this
lands: the null contract on the other 12 combinations (§2 — only
`getDecoder().decode((String) null)` is checked), everything in §6/N2 and
§6/N3, and the MIME-vs-BASIC divergence table in §3.2 beyond its two rows.
