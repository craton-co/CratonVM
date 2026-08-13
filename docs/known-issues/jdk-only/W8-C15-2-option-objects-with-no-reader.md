# W8-C15-2 — `HexFormat`, `UUID`, `Base64`: an option object with no reader, a canonical-form matcher where the spec is lenient, and a null contract that was never written

**Status: PARTIALLY FIXED (lane C15, 2026-08-13). The `HexFormat` repair and
the `UUID` parser are landed in files this lane owns
(`native-builtins/src/phases_late.rs`); the registrations that actually own
those slots in **real-JDK mode** live in `native-builtins/src/lib.rs`, which
this lane does not own — see NOMINATIONS N1–N3, which are the load-bearing
half. Unbuilt and unrun by this lane; every "after" is PREDICTED.**

Three defects from `RJdkIntrinsics2` (`--only=hex`, `--only=b64`,
`--only=uuid`). They look unrelated and are one shape twice over: a native that
answers a question the receiver's own state was supposed to answer, and a
native whose refusal paths were never written down.

---

## 1. `--only=hex` — every `with*` was dropped, not just `withUpperCase`

### Measured

`HexFormat.of().withUpperCase().formatHex({00,FF,0A,80})` — expected
`00FF0A80`, CratonVM produced lowercase.

The brief's hypothesis was "a native that formats from the raw bytes and never
reads the receiver's configuration fields — in which case every `with*` is
dropped." That is exactly what it is, and it is worse than the one symptom
suggests. HotSpot 25.0.3+9, `scratchpad/c15/P2Hex.java`, over `{00,FF,0A,80}`:

```text
of()                                              00ff0a80
of().withUpperCase()                              00FF0A80
of().withLowerCase()                              00ff0a80
ofDelimiter(":")                                  00:ff:0a:80
of().withDelimiter("-")                           00-ff-0a-80
of().withPrefix("0x")                             0x000xff0x0a0x80
of().withSuffix(";")                              00;ff;0a;80;
ofDelimiter(", ").withPrefix("0x")
      .withSuffix("!").withUpperCase()            0x00!, 0xFF!, 0x0A!, 0x80!
```

The live body was:

```rust
for i in 0..len { hex.push_str(&format!("{:02x}", b)); }
```

— no receiver argument used at all. So `withPrefix`, `withSuffix`,
`withDelimiter`, `ofDelimiter` and `withUpperCase` all produced the same eight
characters. The `ucase` flag was *stored* correctly by `withUpperCase` (real
layout slot 3) and simply never read. **A configuration object with no reader
is not "one missing option"; it is every option.**

Four more divergences fell out of the same probe, two of them VM-fatal-adjacent:

| call | HotSpot | CratonVM (before) |
|---|---|---|
| `of().formatHex(b, 3, 1)` | `IndexOutOfBoundsException: Range [3, 1) out of bounds for length 4` | `to - from` on `usize` → **arithmetic overflow panic** |
| `of().parseHex("ff…")` with any non-ASCII char | `NumberFormatException: not a hexadecimal digit: …` | `&s[i*2..i*2+2]` byte-slice → **"byte index is not a char boundary" panic** |
| `HexFormat.isHexDigit(0x661)` (ARABIC-INDIC ONE) | `false` | `true` — the body did `v as u8 as char`, truncating `0x661` to `'a'` |
| `fromHexDigits(" ff ")` | `NumberFormatException: not a hexadecimal digit: " " = 32` | `255` — the body called `.trim()` |
| `fromHexDigits("g")` | `NumberFormatException` | `0` — `unwrap_or(0)` |
| `fromHexDigits("123456789")` | `IllegalArgumentException: string length greater than 8: 9` | silently wrapped |
| `of().formatHex(null)` | `NullPointerException` | `""` |

A Rust panic is not a Java throwable; it takes the VM. Both panics are
reachable from ordinary application input.

### There are TWO HexFormat implementations and they are not the same one

This is why the fix is split. `--dump-native-registry` (`scratchpad/p1/reg.json`,
`mode=compatible`) says every `java/util/HexFormat` slot is owned by
`native-builtins/src/lib.rs:21077-21315` — `register_hex_format_real_jdk_natives`.
The `phases_late.rs` family (`register_p64_hex_format`) does not appear in that
dump at all. Reading `vm_init.rs`:

| mode | registrar that runs | HexFormat owner |
|---|---|---|
| real-JDK (`compatible`, the default) | `register_essential_natives_with_shims` | **`lib.rs`'s** `register_hex_format_real_jdk_natives` |
| synthetic-jdk | `register_builtins` → `register_synthetic_overrides` → phase 64 | **`phases_late.rs`'s** `register_p64_hex_format` |

The two are *twins*: both mint a HexFormat, both store the settings, and
**both** format from raw bytes without reading them. `lib.rs`'s twin is the
more complete of the two (it already had `withSuffix`, `withLowerCase`,
`isUpperCase`) and it is still the one that produced the measured lowercase,
because the measurement was taken in the default mode. `[2twins]`, `[dup nati]`.

### Fixed here

`native-builtins/src/phases_late.rs`, `register_p64_hex_format` rewritten
around a reader:

* `P64HexCfg` + `p64_hex_cfg(ctx, this)` — reads `delimiter@0 prefix@1
  suffix@2 ucase@3`, which is the **real JDK 25 field order** (verified by
  reflection, `scratchpad/c15/Fields.java`), so the same slot numbers serve
  both modes.
* `formatHex([B)` and `formatHex([BII)` share `p64_format_hex_impl`, which
  emits `prefix + 2 digits + suffix` per element with the delimiter BETWEEN
  elements only — the shape the transcript above pins.
* `formatHex([BII)` range-checks in `i32` before widening and reproduces
  `Range [from, to) out of bounds for length n`.
* `withUpperCase` / `withLowerCase` / `withDelimiter` / `withPrefix` /
  `withSuffix` all mint a copy with one field changed; `withX(null)` is
  `NullPointerException: <name>`, as HotSpot's `Objects.requireNonNull` gives.
* `toHexDigits(B/I/J)` honour `ucase`.
* `parseHex` is rewritten over `Vec<char>` (no byte slicing, so no panic) and
  honours delimiter/prefix/suffix, with HotSpot's four error texts:
  `string length not even: 3`, `not a hexadecimal digit: "g" = 103`,
  `extra or missing delimiters or values consisting of prefix, two hexadecimal
  digits, and suffix`, and the null NPE. Registered for the `CharSequence`
  descriptor as well as `String`.
* `fromHexDigits` / `fromHexDigitsToLong` reject sign, whitespace, non-hex and
  over-length instead of `trim().unwrap_or(0)`.
* `isHexDigit` compares the code point, not its low byte.
* `toString()` returns HotSpot's
  `uppercase: true, delimiter: "", prefix: "", suffix: ""`.

PREDICTED after: `--only=hex` green **in synthetic-jdk mode only** until N1
lands. In real-JDK mode nothing changes without N1.

### NOMINATION N1 — `native-builtins/src/lib.rs`

Make the repaired implementation the one that owns the slots in real-JDK mode
too, by delegating at the end of the existing registrar. This is one line plus
a comment and deletes nothing, so it is reversible by deleting the line.

file: `native-builtins/src/lib.rs`, in `register_hex_format_real_jdk_natives`

old (the final `toHexDigits` registration and the function's tail — unique in
the file):
```rust
    registry.register(hf, "toHexDigits", "(J)Ljava/lang/String;", |ctx, args| {
        let v = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let s = format!("{:016x}", v as u64);
        let obj = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(obj))))
    });
    registry.set_category(__prev_cat);
}
```

new:
```rust
    registry.register(hf, "toHexDigits", "(J)Ljava/lang/String;", |ctx, args| {
        let v = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let s = format!("{:016x}", v as u64);
        let obj = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(obj))))
    });
    // W8-C15-2: every registration above is a TWIN of `phases_late.rs`'s
    // `register_p64_hex_format`, and this copy is the one that owns the slots
    // in real-JDK mode (`--dump-native-registry`, mode=compatible, rows
    // lib.rs:21077-21315). Both copies formatted from the raw bytes without
    // reading the receiver, so EVERY `with*` setting was inert -- measured
    // `withUpperCase().formatHex(...)` == lowercase. The repaired body reads
    // the receiver's `delimiter/prefix/suffix/ucase` and reproduces HotSpot's
    // delimiter/prefix/suffix placement, range checks and error texts.
    // Registration is last-write-wins, so this call must stay LAST.
    crate::phases_late::register_p64_hex_format(registry);
    registry.set_category(__prev_cat);
}
```

`register_p64_hex_format` is `pub(crate)`, saves and restores its own category,
and is already reached in synthetic-jdk mode — so this makes the two modes
agree rather than introducing a third behaviour.

**Follow-up, not part of N1:** with the delegation in place the ~270 lines
above it are dead. Deleting them is the right end state and is a separate,
verifiable step; leaving them costs nothing but reading time.

### Residual (hex)

* `HexFormat.of() == HexFormat.of()` is `true` on HotSpot (a cached singleton)
  and `false` here — both twins allocate. Fixing it needs a VM-scoped cache,
  and `[vmscope]` is explicit that a process-global one is wrong. Not attempted.
* `toHexDigits(char)`, `toHexDigits(short)`, `toHexDigits(long,int)`,
  `formatHex(Appendable,…)`, `parseHex(char[],int,int)` and `wrap`-style forms
  are unregistered in both twins; in real-JDK mode they run real bytecode over
  a receiver whose four fields are now all populated, which is the first time
  that has been true.

---

## 2. `--only=uuid` — over-strict and over-lax are the same missing spec

### Measured

`UUID.fromString("1-2-3-4-5")` — HotSpot returns
`00000001-0002-0003-0004-000000000005`; CratonVM threw
`IllegalArgumentException: Invalid UUID string: 1-2-3-4-5`.

The brief asked for the reverse direction too, and it is there. The parser was:

```rust
let hex: String = s.chars().filter(|c| *c != '-').collect();
if hex.len() != 32 { IAE }
let msb = u64::from_str_radix(&hex[0..16], 16).unwrap_or(0) as i64;
```

Strip every dash, count to 32. That is strict about length and **completely
blind to structure**, so it is lax in three directions at once. HotSpot
25.0.3+9 (`scratchpad/c15/P4Uuid.java`, `P4b.java`):

| input | HotSpot | CratonVM (before) |
|---|---|---|
| `1-2-3-4-5` | `00000001-0002-0003-0004-000000000005` | IAE Invalid UUID string |
| `0112233-4455-6677-8899-00aabbccddeef` (35 ch) | `00112233-4455-6677-8899-0aabbccddeef` | IAE |
| `+1-2-3-4-5` | accepted (`parseLong` takes a sign) | IAE |
| `fffffffff-2-3-4-5` (9 f's) | `ffffffff-0002-…` (masked) | IAE |
| `00112233445566778899aabbccddeeff` (no dashes) | **IAE** | **ACCEPTED** |
| `0011-2233-4455-6677-8899-aabb-ccdd-eeff` | **IAE** | **ACCEPTED** |
| `zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz` | `NumberFormatException: Error at index 0 in: "zzzzzzzz"` | `00000000-0000-0000-0000-000000000000` |
| `1-2-3-4-` | `NumberFormatException: For input string: "" under radix 16` | IAE |
| 37 characters | `IAE: UUID string too large` | IAE (different message) |
| `fromString(null)` | `NullPointerException` | returned `null` |

and `&hex[0..16]` byte-slices a Rust `String`, so a 32-byte input containing
one multi-byte character straddling offset 16 **panics**.

### The actual spec

JDK 25's `UUID.fromString` is five `Long.parseLong` calls, and every row above
follows from it:

1. `length() > 36` → `IllegalArgumentException: UUID string too large`.
2. Find five successive `-`. If the FOURTH is absent or a FIFTH exists →
   `IllegalArgumentException: Invalid UUID string: <s>`. **Nothing checks where
   the dashes are** — which is why the 36-character
   `"001122334455-6677-8899-0aab-bccddeef"` parses, to
   `22334455-6677-8899-0aab-0000bccddeef` (measured).
3. Each group goes through `Long.parseLong(cs, from, to, 16)` and is then
   MASKED to its field width. Short groups zero-pad; long groups truncate; a
   leading `+`/`-` is legal; a bad digit or an overflow is a
   `NumberFormatException`, not an `IllegalArgumentException`.

That distinction is testable from Java and therefore contract, not cosmetics.
Note the overflow index: 17 `f`s reports `Error at index 15`, because HotSpot's
`result < multmin` guard fires one digit early. The transcription keeps it.

### Fixed here

`native-builtins/src/phases_late.rs` — `uuid_parse_long_hex(&[char])` and
`uuid_from_string_bits(&str) -> Result<(i64,i64), MethodCallFailed>`, plus a
`#[cfg(test)] mod uuid_from_string_spec_tests` whose every expectation is a
transcribed line of the probes above, covering the lenient forms HotSpot
ACCEPTS, the forms it REJECTS that the old parser accepted, and the two
`parseLong` message shapes. It carries its own mutation check
(`fifth_dash_is_the_rejecting_rule`).

It lives here because `lib.rs` is owned elsewhere. Without N2 the parser is
unreachable and `--only=uuid` does not move.

### NOMINATION N2 — `native-builtins/src/lib.rs`

file: `native-builtins/src/lib.rs`, function `native_uuid_from_string`
(around line 28081)

old:
```rust
fn native_uuid_from_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    // Parse UUID format: 8-4-4-4-12 hex
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: format!("Invalid UUID string: {s}"),
            }
            .into(),
        );
    }
    let msb = u64::from_str_radix(&hex[0..16], 16).unwrap_or(0) as i64;
    let lsb = u64::from_str_radix(&hex[16..32], 16).unwrap_or(0) as i64;
    let uuid = alloc_uuid(ctx, msb, lsb);
    Ok(Some(Value::Object(Some(uuid?))))
}
```

new:
```rust
fn native_uuid_from_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // W8-C15-2. The body that was here stripped every `-`, required exactly 32
    // remaining characters, and did `u64::from_str_radix(&hex[0..16]).unwrap_or(0)`.
    // That is strict where the spec is lenient ("1-2-3-4-5" is legal and
    // zero-pads), lax where the spec is strict (a dashless 32-char string and a
    // 7-dash string were both ACCEPTED), silent where the spec throws
    // (`unwrap_or(0)` turned "zzzz..." into the nil UUID), and it BYTE-sliced a
    // Rust String, which panics on a multi-byte character straddling offset 16.
    // `uuid_from_string_bits` is `UUID.fromString`'s real algorithm --
    // five `Long.parseLong` calls with masking -- transcribed from HotSpot
    // 25.0.3+9 with its two exception classes and their message texts.
    let s = match args.first() {
        Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"String.length()\" because \"name\" is null".to_string(),
                ),
            }
            .into())
        }
    };
    let (msb, lsb) = crate::phases_late::uuid_from_string_bits(&s)?;
    let uuid = alloc_uuid(ctx, msb, lsb);
    Ok(Some(Value::Object(Some(uuid?))))
}
```

`uuid_from_string_bits` is `pub(crate)` in `phases_late`.

PREDICTED after N2: `--only=uuid` green.

### Residual (uuid)

* `Long.parseLong`'s digit test is `Character.digit`, which accepts fullwidth
  and other Unicode decimal/letter forms (`Integer.parseInt("ｆｆ",16) == 255`,
  per the W8-C3-1 census). The transcription uses Rust's `char::to_digit`,
  which is ASCII-only, so a fullwidth-hex UUID group throws here and parses on
  HotSpot. Narrow, unmeasured against CratonVM, and it errs toward refusing.
* `UUID.nameUUIDFromBytes`, `variant()`, `compareTo`, `timestamp()`,
  `clockSequence()`, `node()` are unregistered — real bytecode in real-JDK
  mode, absent in synthetic. `version()` for `1-2-3-4-5` is `0` on HotSpot,
  which is a useful control that the masking is right.

---

## 3. `--only=b64` — the malformed contract was already right; the NULL contract was never written

### Measured

`Base64.getDecoder().decode((String) null)` must throw
`NullPointerException`; CratonVM threw nothing.

The brief asked to check the malformed-input contract and whether a Rust panic
is reachable. **Both are already correct**, and this is worth stating plainly
because it is the half a reader would expect to be broken: `b64_decode`
(`lib.rs:32524`) is a line-for-line port of JDK 25's `decodedOutLength` +
`decode0`, its four error texts are pinned by `Base64Probe` against HotSpot,
`Err(String)` is mapped to `IllegalArgumentException` at both call sites, and
there is no slicing or unchecked arithmetic in it. Verified against HotSpot
25.0.3+9 (`scratchpad/c15/P3B64.java`):

```text
decode("")       []                    decode("QQ")     [65]
decode("QR==")   [65]                  decode("-_-_")   IAE: Illegal base64 character 2d
decode("QQ=")    IAE: Input byte array has wrong 4-byte ending unit
decode("Q")      IAE: Input byte[] should at least have 2 bytes for base64 bytes
decode("QUJD!")  IAE: Illegal base64 character 21
```

What is missing is the null contract, in four bodies, and it is missing in two
different broken ways:

```text
                                  HotSpot                             CratonVM (before)
dec.decode((String) null)         NPE Cannot invoke "String.getBytes  decoded "" -> byte[0]
                                      (java.nio.charset.Charset)"
                                      because "src" is null
dec.decode((byte[]) null)         NPE Cannot read the array length     Ok(None)
                                      because "src" is null
enc.encode((byte[]) null)         NPE (same text)                      Ok(None)
enc.encodeToString(null)          NPE (same text)                      Ok(None)
```

`Ok(None)` from a native whose descriptor returns `[B` or `String` is its own
hazard: it is the VOID return, so nothing is pushed where the caller expects a
reference. That is a worse failure than the wrong value.

### NOMINATION N3 — `native-builtins/src/lib.rs`

**N3a** — `native_b64_decode_string` (around line 32843).

old:
```rust
    let src_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
```

new:
```rust
    let src_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        // W8-C15-2: `decode((String) null)` decoded the EMPTY string and
        // returned `byte[0]`. HotSpot 25.0.3+9, measured.
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"String.getBytes(java.nio.charset.Charset)\" \
                     because \"src\" is null"
                        .to_string(),
                ),
            }
            .into())
        }
    };
```

**N3b** — the byte-array argument arm, which is the SAME nine lines in three
functions: `native_b64_decode_bytes`, `native_b64_encode`,
`native_b64_encode_to_string`. In each, the SECOND `match` (the one binding
`src`, not the one binding `this`):

old:
```rust
    let src = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
```

new:
```rust
    let src = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        // W8-C15-2: HotSpot throws
        //   NullPointerException: Cannot read the array length because "src" is null
        // for all three of decode([B), encode([B) and encodeToString([B).
        // `Ok(None)` is the VOID return, so this also pushed nothing where the
        // caller expects a reference.
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some(
                    "Cannot read the array length because \"src\" is null".to_string(),
                ),
            }
            .into())
        }
    };
```

Apply to all three functions; leave the `this` arms alone. The old text above
occurs **exactly three times** in `lib.rs`, verified mechanically, and they are
exactly those three functions — the `this` arms bind through `args.first()`, so
they do not match:

```text
line 32780  fn native_b64_encode
line 32797  fn native_b64_encode_to_string
line 32828  fn native_b64_decode_bytes
```

PREDICTED after N3: `--only=b64` green.

### Residual (b64)

* `decode(ByteBuffer)`, `encode(ByteBuffer)`, `decode(byte[],byte[])`,
  `encode(byte[],byte[])`, `wrap(InputStream)`, `wrap(OutputStream)` and
  `getMimeEncoder(int,byte[])` are unregistered; their null contracts are in
  the HotSpot transcript above (`P3B64`) but unmeasured against CratonVM.
* `getMimeEncoder(int, byte[])` with a null separator is
  `NullPointerException: null` — a *bare* NPE, unlike the others.

---

## 4. NOMINATION N4 — the family fix, applied to itself

Every one of the three defects above was found by asking a family the same
question at every member. Two of the three would have been missed by asking it
once:

* `hex` looked like one dropped flag and was every dropped flag, plus two
  reachable Rust panics and a truncating `as u8` in a fourth method.
* `uuid` looked like an over-strict parser and was an over-strict AND
  over-lax one — the same missing specification read from two sides. A fix that
  only loosened would have kept the dashless-32-character acceptance, and
  nothing in the vector would have said so.
* `b64` looked like it needed a decoder audit and needed four null arms; its
  decoder was already a faithful port. Auditing the interesting half and
  skipping the boring half would have found nothing.

## 5. What this lane could and could not verify

Could not: build or run CratonVM. Every "after" above is PREDICTED.

Could, and did:

* **HotSpot is the oracle throughout.** Four probes, all committed to
  `scratchpad/c15/`: `P1Bounds` / `P2Hex` + `P2b` / `P3B64` / `P4Uuid` + `P4b`,
  plus `Fields.java` for the `HexFormat` instance layout. Every expected value
  in the fixes and in these records is a transcribed line of their output on
  Microsoft OpenJDK 25.0.3+9, not a memory.
* **Which body owns each slot**, from `--dump-native-registry`
  (`scratchpad/p1/reg.json`, `scratchpad/b13/reg.json`, both `mode=compatible`)
  — this is what found the `HexFormat` twin and stopped the fix from landing in
  the file that does not answer in the default mode.
* **Syntax.** `rustfmt --edition 2021 --emit stdout` (the real rustc parser, not
  `cargo`) over all three edited files: `rc=0` for
  `util_concurrent_ext.rs`, `phases_early.rs` and `phases_late.rs` with its
  submodule tree. That is a parse check, not a type check.
* **The `lib.rs` anchors.** Each NOMINATION's `old` text was matched
  mechanically against the current `lib.rs`: N1, N2 and N3a occur exactly once;
  N3b exactly three times, in exactly the three named functions.

Concretely, for whoever runs the eleven families next: run `--only=hex`,
`--only=uuid` and `--only=b64` **before and after** N1–N3 separately from the
synthetic-jdk arm, because `hex` will move in synthetic mode from this lane's
edits alone and will not move in real-JDK mode until N1 lands. A single green
run of the default mode after all three land cannot tell those two apart.
