# E14-1 — Base64: the fabricated receiver, and the seven methods that read it

**2026-08-13, lane E14.** Picks up the four items lane E5 recorded but could
not land (`E5-1` §6/N2, §6/N3).

This lane **owns `native-builtins/src/lib.rs`**; everything else is a
NOMINATION (§7).

**This lane may not build or run the VM.** Every CratonVM "after" is
**PREDICTED**. Every HotSpot row was executed on this host against
`openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` from `scratchpad/e14/`
(`B64Ident.java`, `B64Line.java`, `B64Unreg.java`, `B64Fab.java`,
`B64Zero.java`, `B64Matrix.java`), most of them through the **private
constructors** `Encoder(boolean,byte[],int,boolean)` and
`Decoder(boolean,boolean)`, which is what makes "what does our fabricated
object look like to real bytecode" a measurement rather than a code reading.

The file was and remains **LF-only (0 CRLF)**, counted after every edit.
`native-builtins/src/lib.rs` **parses** (`rustfmt --emit stdout` against a
scratchpad copy with stubbed submodules, exit 0) and the edited regions are
**rustfmt-clean** (0 remaining hunks); the rest of the file was already not
fmt-clean and was left alone, so no other lane's lines moved.

---

## 0. Verdict

| E5's item | what it actually was | landed |
|---|---|---|
| (1) fabricated `Encoder` leaves `newline` NULL | **worse than recorded** — it is not "whenever `linemax > 0` and the input wraps", it is **every encode call**, because `encodedOutLength` reads `newline.length` before `encode0` runs (§1) | yes |
| (2) custom `linemax` not honoured | confirmed, 82 vs 83, and the off-by-one is a *coincidence* of two different accountings (§2) | yes |
| (3) the six factories are not singletons | **the fix is not a cache.** The singletons ARE the JDK's own statics, and a cache cannot satisfy a row a real-bytecode caller can already see (§3) | yes |
| (4) 7 of 18 methods unregistered | **register none of them** — but three of the seven were being fed a receiver with the wrong layout, which is the real defect behind the item (§4) | yes |
| — | a THIRD defect neither item names: `linemax = 0` is not `-1`, and `getEncoder().wrap(os)` threw NPE because of it (§5) | yes |

## 1. `newline` is read on EVERY encode, not only when the output wraps

E5 recorded that real `encode0` reads `newline` "whenever `linemax > 0`". The
read that actually bites is one level earlier, in `encodedOutLength` →
`outLength`, which sizes the destination array:

```java
if (linemax > 0)                                  // line separators
    len += (len - 1) / linemax * newline.length;
```

So it is dereferenced before a single byte is encoded, for any input length.
Measured by constructing CratonVM's exact fabricated shape through the private
constructor (`B64Fab.java`):

```
### CratonVM's fabricated MIME encoder TODAY: newline=null, linemax=76
encodeToString(30B / 40 chars)     NullPointerException / Cannot read the array length because "this.newline" is null
encodeToString(57B / 76 chars)     NullPointerException / …
encodeToString(60B / 80 chars)     NullPointerException / …
encode(byte[])      60B            NullPointerException / …
encode(byte[],byte[]) 60B          NullPointerException / …
encode(ByteBuffer)  60B            NullPointerException / …
wrap(OutputStream)  60B            NullPointerException / Cannot read the array length because "b" is null

### after the fix: newline=CRLF, linemax=76
encodeToString(60B)                82
encode(byte[],byte[]) 60B          82
equals getMimeEncoder output       true
```

30 bytes is 40 characters and needs no wrapping at all, and it still throws.
"Only long inputs are affected" was the wrong model.

### 1.1 The GC hazard, stated precisely

`try_alloc_concurrent_synthetic` returns a bare `ObjectRef` — a raw pointer
into the GC heap, not a root. Writing `newline` means allocating a
`byte[]{13,10}` **between** that allocation and the `set_field` calls. That
allocation can trigger a young collection, and this VM's young collector
**moves**: it evacuates survivors and rewrites only the root sets it knows —
thread stacks, statics, `native_pin_roots`, and the per-thread handle-slot
table (`vm/src/memory/roots.rs`). A raw `ObjectRef` in a Rust local is in none
of them.

Two things follow, and the second is the sharper one:

* the encoder can **move**, so the four `set_field`s would write into the
  vacated from-space slot — whatever object now occupies it;
* at that moment the encoder is young, half-built, and **referenced from
  nowhere in Java**, so it is not merely relocatable, it is *collectable*.

That is the family `types/src/handle.rs` opens by calling this codebase's #1
recurring defect ("37+ independently-discovered, independently-fixed sites").
The old body was safe **only because it never allocated** — it simply left the
field null. This is why the fix is not a one-liner: it converts a safe function
into an unsafe one and has to bring its own rooting.

The existing helper is `NativeHandleScope` (`native-api/src/registry.rs:452`,
over `types/src/handle.rs`), already used in this same file at three sites and
~18 times in `lang_string.rs`; no new machinery was written. The encoder is
rooted before the array allocation and **re-read through the slot afterwards**,
so there is no pre-allocation local left to use by mistake, and `Drop` closes
the scope on the `?` path too.

## 2. Why the custom-`linemax` miss is off by exactly one

`Base64.getMimeEncoder(int, byte[])` is unregistered, so it runs real bytecode
and returns a real `Encoder` carrying `linemax=20, newline=[10]`; a subsequent
`encodeToString` is then intercepted by our native, which re-derived 76/CRLF
from a three-valued variant tag. On the 60-byte fixture (80 base64 characters):

```
HotSpot   getMimeEncoder(20,{'\n'})   80 + 3 separators x 1 byte = 83
CratonVM  hardcoded 76/CRLF           80 + 1 separator  x 2 bytes = 82
```

The two are one apart for unrelated reasons — three one-byte separators against
one two-byte separator. Nothing about the shapes is close: HotSpot writes
**four 20-character lines**, we wrote **one 76-character line and a tail**. The
length near-miss is a coincidence, and it is the whole danger: a
"roughly the right length" assertion passes, and so does a round-trip, because
MIME decoding ignores every non-alphabet byte and therefore ignores the line
structure entirely. Only asserting the TEXT catches it, which is what the new
test does.

`b64_encode` was rewritten as `b64_encode_wrapped(input, is_url, no_padding,
linemax, newline)`, a line-for-line port of `encode0`. Three details a
plausible rewrite gets wrong, each measured (`B64Line.java`):

1. **The separator test is `dlen == linemax`, an EQUALITY.** `slen` is rounded
   down to whole triples, so a `linemax` that is not a multiple of 4 can never
   be hit and the encoder emits **no separators at all**: `linemax=20 -> 83`
   but `linemax=22 -> 80` and `linemax=6 -> 80`. A `>=` wraps all three.
2. **A separator is written only when input remains** (`&& sp < end`): 57 bytes
   at `linemax=76` is 76 characters, not 78; 15 bytes at `linemax=20` is 20,
   not 21.
3. **`linemax > 0` does not mean "wraps"**: `linemax=1000` on 60 bytes is 80.

Also pinned from the same probe, and relevant to §7/N1: `getMimeEncoder`'s
`lineLength` is rounded with `lineLength >> 2 << 2` (`19 -> 16`, `21/22/23 ->
20`), anything `<= 0` after rounding returns the **basic** encoder, a separator
containing a base64-alphabet character is `IllegalArgumentException / Illegal
base64 line separator character 0x41` (`'-'` passes, `'='` does not), an EMPTY
separator is legal, and the array is **not** copied — mutating the caller's
array afterwards changes the encoder's output.

### 2.1 Verified mechanically, not by eye

Because this lane cannot run cargo, the new `b64_encode_wrapped` was
transcribed line-for-line into `scratchpad/e14/port.py` and run against
HotSpot over a matrix in an identical output format
(`B64Matrix.java`, encoders built through the private constructor):

**17 payload lengths x 2 alphabets x 2 padding x 12 linemaxes x 4 separators
= 3264 rows.**

```
$ diff hotspot.txt rustport.txt
$ echo $?
0
```

The one deliberate divergence is excluded from the matrix and documented in
code: `linemax` in `1..=3` makes real JDK's block size `linemax / 4 * 3` zero
and its `while (sp < sl)` loop never advances — measured, HotSpot **hangs**
(all three still running after 1.2 s). No public factory can produce it. We
treat it as "no wrapping" rather than reproducing a hang, and a test pins that.

### 2.2 `withoutPadding()` had the same bug

It called `b64_alloc_encoder(ctx, variant, true)` — rebuilding from the tag, so
a custom 20/LF encoder came back 76/CRLF. Real JDK **copies the fields**
(`new Encoder(isURL, newline, linemax, false)`), sharing the same `newline`
array. Measured: `getMimeEncoder(20,{'\n'}).withoutPadding()` keeps
`linemax=20, newline=[10]`, 60B -> 83, 61B -> 86 (88 padded). Now a field copy,
which also needs no allocation for the separator.

## 3. The six factories: the fix is not a cache

Identity, not equality (`B64Ident.java`):

```
getEncoder()        == getEncoder()        -> true
getUrlEncoder()     == getUrlEncoder()     -> true
getMimeEncoder()    == getMimeEncoder()    -> true
getDecoder()        == getDecoder()        -> true
getUrlDecoder()     == getUrlDecoder()     -> true
getMimeDecoder()    == getMimeDecoder()    -> true
```

**Is caching safe?** Yes — every instance field of both classes is `final`
(measured, all six), so a shared instance has no mutable state. But that is the
wrong question, because a cache of a *fabricated* object cannot satisfy this
row, also measured:

```
getMimeEncoder(0,  sep) == getEncoder()  -> true
getMimeEncoder(-1, sep) == getEncoder()  -> true
```

`getMimeEncoder(int, byte[])` is deliberately unregistered (§4), so it runs
real bytecode and returns the real `Encoder.RFC4648` for any `lineLength <= 0`.
No native-side memoization makes a fabricated object equal to that. The
singletons ARE the JDK's own `static final` fields — `Encoder.RFC4648 /
RFC4648_URLSAFE / RFC2045` and the three matching ones on `Decoder` — so the
six factories now **return the field** when a real class library is present.

That also disposes of the two things a cache would have needed and could have
got wrong: a static field is already a GC root, so no
`register_var_handle_root` / `read_var_handle_root` re-read discipline against
a moving collector; and it is already VM-scoped, so no process-global `static`
to key by `vm_identity()`.

Fallback is unconditional on any imperfection — class will not initialize, no
such field, field holds anything but a non-null reference — and then the
factory fabricates exactly as before. That is what keeps synthetic-JDK mode
working. `Base64$Encoder.<clinit>` and `Base64$Decoder.<clinit>` build these
statics and call nothing on `java.util.Base64` (checked with `javap -c`), so
there is no recursion back into the factory.

**Risk note, stated plainly.** This is the change most in need of a run. Its
mitigation is that `<clinit>` is not newly exercised: `try_alloc_concurrent_
synthetic` already called `ensure_class_initialized("java/util/Base64$Encoder")`
on **every** `getEncoder()` before this change, so the class initializer is
already on today's green path. What is new is reading its output.

What still diverges and this does NOT fix: `withoutPadding()` on a padded
encoder returns a FRESH object every call (measured **false** for
`enc.withoutPadding() == enc.withoutPadding()`), which we already match, and
`getMimeEncoder(20, sep)` is likewise never cached (measured false).

## 4. The seven unregistered methods — register NONE, fix the receiver instead

The brief's question for each was "should this instead run REAL BYTECODE". For
all seven the answer is yes, and the interesting finding is *why they were not
already right*: all seven read their answer off the RECEIVER's fields, and
three of them were being handed a receiver with the **wrong layout**.

### 4.1 The `Decoder` synthetic was one Int slot where the JDK has two booleans

```
javap -p java.util.Base64$Decoder
  private final boolean isURL;     // slot 0
  private final boolean isMIME;    // slot 1
```

CratonVM allocated a **1-field** synthetic and wrote the variant tag (0/1/2)
into slot 0. So `getMimeDecoder()` wrote **2 into `isURL`** — nonzero, hence
`true` to real bytecode — and left `isMIME` false. Measured by building
`Decoder(isURL=true, isMIME=false)` through the private constructor
(`B64Unreg.java`):

| input | real `getMimeDecoder()` | our fabricated shape |
|---|---|---|
| 76-column wrapped text | `len=60` | `IllegalArgumentException: Illegal base64 character d` |
| `-_-_` | `len=0` | `len=3` |
| flat 80-char text | `len=60` | `len=60` |
| `QQ\n==` | `len=1` | `IllegalArgumentException: Illegal base64 character a` |

Three of four rows wrong, one of them **silently**. This is the
two-readers-of-one-slot shape: the NATIVE route read slot 0 as a variant tag,
the BYTECODE route read it as `isURL`, and the two drifted on the value that
matters. Fixed to the real two-boolean layout, with the variant tag derived on
read and kept as this file's internal vocabulary (`b64_decode` is written
against it and was verified 36/36 by lane E5 — **not touched**).

No `classloading/src/class_manager.rs` change is needed for the width: the
synthetic declarations there declare **methods only**, no fields, so
`class_num_total_fields` is 0 in synthetic mode and
`try_alloc_concurrent_synthetic`'s `num_fields.max(real)` simply allocates 2.
In real-JDK mode `real` is now 2 and the request is 2, so the layout-alias
report this call used to trip (`classify(1, 2)`) also goes away.

### 4.2 Per-method decision

| method | decision | why |
|---|---|---|
| `Base64.getMimeEncoder(I[B)` | **not registered** | a native would have to re-implement `lineLength >> 2 << 2` rounding, the separator alphabet check, and the `<= 0 -> RFC4648` aliasing — three separately measured behaviours the bytecode already has |
| `Encoder.encode([B[B)I` | **not registered** | real `encode0` + the `Output byte array is too small for encoding all input bytes` contract |
| `Encoder.encode(ByteBuffer)` | **not registered** | plus ByteBuffer position/limit semantics (measured: source position advances to 60) |
| `Encoder.wrap(OutputStream)` | **not registered** | returns a stateful `Base64$EncOutputStream`; this is also the method §5 was breaking |
| `Decoder.decode([B[B)I` | **not registered** | real `decode0`, now fed a correctly-laid-out receiver (§4.1) |
| `Decoder.decode(ByteBuffer)` | **not registered** | ditto |
| `Decoder.wrap(InputStream)` | **not registered** | ditto; returns a stateful `Base64$DecInputStream` |

HotSpot's answers for the 60-byte fixture, re-measured here: 83 / 82 mime + 80
basic / 82 / 82 / 60 / 60 / 60.

**Synthetic-JDK mode stays broken on purpose.** There is no bytecode there, so
all seven are `NoSuchMethodError`. Registering them to fix that would ALSO
shadow the real bytecode in real-JDK mode, because a `registry.register` call
cannot see the runtime JDK mode — the same trap as gating a native on a feature
flag and thereby dropping it from a mode whose stub has no method bodies, run
in reverse. Nothing in the corpus is known to call these seven.

## 5. The third defect: `linemax = 0` is not `linemax = -1`

Neither E5 item names this. `b64_alloc_encoder` wrote **0** into `linemax` for
the basic and URL encoders; the real JDK writes **-1**. E5 correctly changed
our MIME test from `!= 0` to `> 0`, which reads both sentinels alike — and so
does every `linemax > 0` test inside `outLength` and `encode0`. But
`Base64$EncOutputStream` does not: it tests `linepos == linemax`, and `linepos`
starts at 0. Measured (`B64Zero.java`):

```
linemax=0   newline=null   encodeToString=80   wrap(OutputStream)=NPE / "b" is null
linemax=0   newline=CRLF   encodeToString=80   wrap(OutputStream)=82     <- WRONG
linemax=-1  newline=null   encodeToString=80   wrap(OutputStream)=80     <- real getEncoder()
real getEncoder().wrap                                                -> 80
```

So `Base64.getEncoder().wrap(os)` — one of the seven, hence real bytecode —
threw `NullPointerException` on its first write. And the second row is why
"just give the fabricated encoder a CRLF newline" is not the fix on its own:
with `linemax` still 0 that converts the NPE into a **silently wrong 82**, a
basic stream with a CRLF injected into it. Writing -1 is what makes `wrap`
correct.

## 6. What landed in `native-builtins/src/lib.rs`

1. **Section header** rewritten to state the real JDK layout of BOTH classes and
   the measured damage the old decoder layout did.
2. **`b64_encode_wrapped`** — the `encode0` port taking `linemax`/`newline`;
   `b64_encode(input, variant, no_padding)` kept as a thin wrapper so the three
   out-of-file callers (`http_url_connection.rs`, `net_phase_e.rs`,
   `regex_matcher.rs`'s test) are untouched, with a test pinning that the
   wrapper still means 76/CRLF.
3. **`b64_alloc_encoder`** — writes all four fields under a `NativeHandleScope`;
   `-1` not `0`; explicit null `newline` for the non-MIME encoders.
4. **`b64_alloc_decoder`** — the two-boolean JDK layout.
5. **`b64_encoder_shape` / `b64_decoder_variant`** replace the two variant-tag
   readers; the shape reader throws HotSpot's NPE for `linemax > 0` with a null
   `newline` rather than defaulting to CRLF.
6. **`b64_jdk_singleton` + `b64_encoder_factory` / `b64_decoder_factory`** — the
   six factories return the JDK's own statics, fabricating only as fallback.
7. **`native_b64_without_padding`** — copies the four fields instead of
   rebuilding from a tag.
8. **`register_base64_natives`** — a doc block recording the per-method
   decision for all seven unregistered methods (§4.2).
9. **Nine tests.** Four pure-function
   (`line_wrapping_matches_hotspot_for_every_measured_linemax` — the 13-row
   HotSpot table plus the exact 83-character text;
   `no_trailing_separator_at_any_linemax`;
   `a_sub_quantum_linemax_terminates_instead_of_spinning`;
   `the_variant_tag_wrapper_still_means_76_and_crlf`) and five behavioural,
   driven through the natives:
   `fabricated_encoders_carry_the_jdk_newline_and_linemax_fields`,
   `a_custom_linemax_and_separator_reach_the_encoder`,
   `without_padding_preserves_a_custom_linemax_and_shares_the_separator`,
   `a_wrapping_encoder_with_a_null_newline_throws_npe`,
   `decoder_uses_the_two_boolean_jdk_layout`,
   `the_factories_return_the_jdk_singleton_when_there_is_one`,
   `the_factories_fabricate_when_there_is_no_jdk_singleton`.

   Every behavioural assertion reads a value the native **wrote** or an answer
   it **computed from a receiver's fields** — never "the triple is registered".
   Two deliberate discriminators: the mock's unwritten slots read back as
   `Value::Int(0)`, so the `Value::Object(None)` assertions fail if the explicit
   null write is dropped (they measure the WRITE, not the default); and the
   singleton test **declares** the static field on the mock rather than relying
   on a name-to-slot fallback, so it measures the lookup and not the mock.

## 7. NOMINATIONS

Both targets are **LF-only**; apply with LF endings.

### N1 — `regression-suite/src/RJdkIntrinsics2.java`: `--only=b64` reaches none of this

The fixture's 27 checks exercise `encodeToString`, `encode([B)`,
`decode([B)`/`decode(String)`, `withoutPadding()`, and the canonical 76/CRLF
MIME line policy. They do **not** touch identity, a custom `linemax`, any of the
seven unregistered methods, or the decoder's field layout — i.e. everything in
§1–§5. Seven rows would cover it. Replace

```java
        sectionEnd("b64", 27);
```

with

```java
        // E14: the surface the 27 rows above never reach — identity, a custom
        // linemax, and the methods that run real JDK bytecode against a
        // receiver this VM fabricates. Every expected value measured on
        // HotSpot 25 (scratchpad/e14).
        check(Base64.getEncoder() == Base64.getEncoder(),
                "the factories are SINGLETONS — identity, not equality");
        check(Base64.getMimeEncoder(0, new byte[] { '\n' }) == Base64.getEncoder(),
                "getMimeEncoder(lineLength<=0) must return the basic encoder ITSELF");
        check(Base64.getMimeEncoder(20, new byte[] { '\n' }).encodeToString(big).length() == 83,
                "a custom linemax must be honoured: 80 chars + 3 one-byte separators");
        byte[] dst = new byte[200];
        check(Base64.getEncoder().encode(big, dst) == 80,
                "encode(byte[],byte[]) must write 80 bytes for the basic encoder");
        check(Base64.getMimeEncoder().encode(big, dst) == 82,
                "encode(byte[],byte[]) must write 82 for the MIME encoder — it reads `newline`");
        check(Base64.getMimeDecoder().decode(
                        mime.getBytes(java.nio.charset.StandardCharsets.ISO_8859_1), dst) == 60,
                "decode(byte[],byte[]) on the MIME decoder must accept the wrapped text");
        java.io.ByteArrayOutputStream bo = new java.io.ByteArrayOutputStream();
        try (java.io.OutputStream os = Base64.getEncoder().wrap(bo)) {
            os.write(big);
        } catch (java.io.IOException e) {
            throw new RuntimeException(e);
        }
        check(bo.size() == 80, "getEncoder().wrap(OutputStream) must write 80 bytes");

        sectionEnd("b64", 34);
```

(`big` and `mime` are already in scope at that point; `StandardCharsets` needs
an import if absent.) PREDICTED after this lane: rows 1–5 pass; rows 6–7 are
genuinely new coverage of real-bytecode stream paths this lane could not run
and are as likely to *expose* a gap as to pass — which is the point of adding
them.

### N2 — `classloading/src/class_manager.rs`: the synthetic surface is 11 of 18

Unchanged from E5's N3 and **not** made worse by this lane (§4.1 explains why
the decoder width needs no declaration change). In synthetic-JDK mode the seven
methods of §4.2 are `NoSuchMethodError`. Registering them is **not** the fix —
see §4.2 for why a mode-blind `registry.register` would shadow correct
bytecode. If synthetic mode ever needs them, they need a runtime-mode-gated
registration, not a `cfg`.

## 8. Which `--only=b64` checks this flips — PREDICTED

**None. All 27 must stay green, and that is the honest claim.**

E5 already flipped the one row that was failing (26/27 -> 27/27, the
`decode((String) null)` row). This lane's four items are all in surface the
fixture does not reach — which is exactly why they survived a green fixture,
and why N1 exists.

What could nevertheless MOVE a row, ranked by risk, so a red run has a
shortlist:

1. **The six factories now return the JDK's statics (§3)** — the only change
   that touches all 27 rows, because every one of them starts at a factory. If
   `Base64$Encoder.<clinit>` produces a bad `RFC4648` under CratonVM, everything
   fails at once. Mitigations: the fallback covers a missing/null/non-reference
   field, and `<clinit>` already runs on today's green path (§3). **First thing
   to check on a red `--only=b64`.**
2. **`withoutPadding()` copies fields (§2.2)** — row 3
   (`"+/+/AAE"`). Basic encoder: copy is `newline=null, linemax=-1, isURL=0,
   doPadding=0` -> unpadded basic. Predicted unchanged.
3. **The encoder rewrite (§2)** — rows 2, 3, 4, 5, 6, 7 and the three MIME line
   rows (82 / break at 76 / exactly one break). Covered by the 3264-row diff and
   by `the_variant_tag_wrapper_still_means_76_and_crlf`.
4. **The decoder layout (§4.1)** — rows 8–22 and 24–27. `b64_alloc_decoder` and
   `b64_decoder_variant` were changed together and are the only two readers of
   those slots in the workspace (grepped: `B64_DECODER_FIELD_VARIANT` had
   exactly two uses, both in this file).
5. **`linemax` 0 -> -1 (§5)** — invisible to every `> 0` test, so invisible to
   all 27. It changes only `wrap(OutputStream)`, which the fixture does not call.

Still unmeasured after this lands, in this lane's own scope: everything in §1–§5
as *executed by the VM* (all of it is PREDICTED), the seven methods' real-JDK
answers, and whether `Base64$Encoder.<clinit>`'s statics are sound under
CratonVM.
