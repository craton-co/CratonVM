# W7-71 — an RSA padding failure that no `catch` could catch, and a `Files.write` that hardcoded LF

**Status: FIXED in source 2026-08-12, NOT yet verified against a binary.** This
lane could not build or run Rust. Every expected value below is measured on
Temurin 25.0.3+9 on Windows 11; nothing here has been observed on a CratonVM
binary. The verification command is in §8.

> **VERIFIED AGAINST A BINARY 2026-09-01.** The status above was written by a
> lane that could not build or run Rust, and it stood for roughly three weeks.
> Run on a release binary of `dev`, against HotSpot 25.0.4+7 on the same host:
>
> ```text
> RCrypto
>   HotSpot          PASS RCrypto (57 checks)
>   CratonVM compat  PASS RCrypto (57 checks)      0 differing lines
>   CratonVM strict  PASS RCrypto (57 checks)      0 differing lines
> ```
>
> Byte-identical output in BOTH modes, so the source work this record describes
> does what it claimed on a real binary.
>
> **The predicted COUNT is superseded: this record expected `PASS RCrypto (47 checks)`.** Other
> lanes added to the shared vector across the three weeks. A count written as an
> expectation ages into a falsehood the moment a shared vector grows -- what
> survives verification is the ASSERTIONS, and those match. Do not re-derive a
> defect from a count that merely moved.


Predecessor: `W7-60-harness-extract-blindness.md`, which repaired the two
instruments that made both of these visible. Neither is a regression. `RCrypto`
went from 7 asserted checks to 27 and `RNioNoFollow` from **0 executed
assertions on Windows to 10**; these are what those instruments could see the
first time they could see anything.

---

## 0. The headline

| | measured |
| --- | --- |
| JCA exception rows the probe SAMPLED | **2** |
| rows the census found wrong, **excluding** the 2 | **7** |
| of those, raising an UNCHECKED exception where the JDK raises a checked one | **5** |
| of those, raising **NOTHING** — a wrong answer with no exception at all | **2** |
| AEAD tag failure correctly `AEADBadTagException`? | **yes, and it already was** |
| hardcoded-`\n` sites in the io/nio surface | **5** |
| of those, LIVE (wrong answer on Windows today) | **1** |
| were `%n` / `println` also wrong? | **NO — both already emit `\r\n`** |
| `RCrypto` | 27 → **47** checks, 10 → **18** `CK` lines |
| `RNioNoFollow` | 10 → **16** checks on Windows |
| harness self-check after the fixture changes | **70 vectors sound, 0 flagged** |

Two rows sampled, nine defects. The ratio is the point, and it is the third time
today: `String.format` raised the base `IllegalArgumentException` where **11 of a
sealed family of 12** needed a subclass, and `ProcessBuilder("").start()` raised
`IllegalArgumentException` where `start()` declares `IOException`. Each was one
dead predicate seen many times.

---

## 1. Item 1 — the reported red

```
CratonVM: CK RCrypto oaepRefusals=IllegalStateException,IllegalStateException
HotSpot : CK RCrypto oaepRefusals=BadPaddingException,BadPaddingException
CratonVM: CK RCrypto pkcs1Refusals=IllegalStateException
HotSpot : CK RCrypto pkcs1Refusals=BadPaddingException
```

`javax.crypto.BadPaddingException` is **checked**. `IllegalStateException` is
not. So a caller writing the JDK's own `catch (BadPaddingException e)` around an
RSA decrypt did not catch this, and the failure escaped as an unchecked throw
through code that believed it had handled it.

`crypto_impl::rsa_cipher_encrypt` / `rsa_cipher_decrypt` returned
`Result<Vec<u8>, String>`, and the single live caller —
`jca/cipher.rs::cipher_do_final_impl` — collapsed the entire set into

```rust
Err(msg) => Err(RuntimeError::IllegalStateException { message: msg }.into()),
```

One line, every RSA failure mode.

### 1.1 What the oracle said that the two sampled rows did not

Both functions now return `RsaCipherError`, whose three variants carry the class
SunJCE raises. The interesting part is not that padding failures became
`BadPaddingException`; it is the two things a length check written as
`ct.len() != k` cannot express:

| input | CratonVM before | HotSpot 25 |
| --- | --- | --- |
| OAEP/PKCS1 decrypt, wrong private key | `IllegalStateException` | `BadPaddingException: Padding error in decryption` |
| OAEP/PKCS1 decrypt, one flipped byte | `IllegalStateException` | `BadPaddingException: Padding error in decryption` |
| PKCS1 decrypt, **200-byte** ciphertext (RSA-2048) | `IllegalStateException` | `BadPaddingException: Padding error in decryption` |
| PKCS1 decrypt, **300-byte** ciphertext | `IllegalStateException` | `IllegalBlockSizeException: Data must not be longer than 256 bytes` |
| PKCS1 encrypt, 246-byte plaintext | `IllegalStateException` | `IllegalBlockSizeException: Data must not be longer than 245 bytes` |
| OAEP-SHA-256 encrypt, 191-byte plaintext | `IllegalStateException` | `IllegalBlockSizeException: Data must not be longer than 190 bytes` |
| `NoPadding`, value ≥ the modulus | `IllegalStateException` | `BadPaddingException: Message is larger than modulus` |

**The short/long asymmetry is HotSpot's own.** `RSACipher.doFinal` (JDK 25
source, read from `lib/src.zip`) refuses only `bufOfs > buffer.length`; a
ciphertext *shorter* than the modulus is simply a smaller integer that goes
through the modexp and fails to unpad. The old check refused both directions
with one exception, so it had the class wrong on both and the **side** wrong on
one. `rsa_cipher_decrypt` is now one-sided and also carries `RSACore.parseMsg`'s
`c >= n` refusal.

`buffer.length` on the encrypt side is `RSAPadding.getMaxDataSize()`, which is
why `RsaCipherPadding::max_data_size` exists and why it lives on the padding
enum rather than inside the pad functions — see §1.3.

### 1.2 The census — true population against the 2 sampled

Every row measured with `probes/JcaExceptionTypeProbe.java` (91 rows, oracle
transcript committed beside it) and adjudicated against the CratonVM source.

| exception | CratonVM before | verdict |
| --- | --- | --- |
| `BadPaddingException` | RSA: **unchecked ISE**. AES/ECB padding: correct. | **FIXED** (RSA) |
| `IllegalBlockSizeException` | RSA lengths: **unchecked ISE**. AES ragged: correct. AES/KW *unwrap*: **wrong sibling** (see below). | **FIXED** ×2 |
| `AEADBadTagException` | **correct already**, AES-GCM and ChaCha20-Poly1305 both | no change — §2 |
| `InvalidKeyException` | 18 throw sites, correct where checked | no change |
| `NoSuchPaddingException` | 1 site, reached from `classify_transformation` | no change |
| `ShortBufferException` | `Mac` correct. **`Cipher.doFinal([BII[B)I` raised NOTHING** — wrote past the end of the caller's array with no bounds check | **FIXED** |
| `InvalidAlgorithmParameterException` | 7 sites correct. AES-GCM with a non-12-byte IV was **unchecked ISE** | **FIXED** |
| `SignatureException` | **8 sites raised unchecked ISE**; `update` before init raised **NOTHING**; `sign(byte[],int,int)` into a short buffer raised **NOTHING** and returned a truncated signature as success | **FIXED** ×10 |
| `KeyStoreException` | 21 sites, correct | no change |
| `UnrecoverableKeyException` | 3 sites, correct — but its place in the verifier's hierarchy table was wrong, §3 | table FIXED |
| `InvalidKeySpecException` | 3 sites correct; **absent from the hierarchy table under its real name**, §3 | table FIXED |
| `DigestException` | correct, via its own `throw_digest_exception` | no change |
| `IllegalStateException` on `Mac` | **correct, and must stay** — HotSpot raises it AND `Mac.doFinal` declares it | asserted, §2.2 |

Two rows sampled. **Nine defects across five classes**, of which two are worse
than a wrong class because they are a wrong *answer*:

* **`Signature.sign(byte[], int, int)` into a too-small buffer.** The code was
  `let written = sig_bytes.len().min(max_len);` — it wrote the first 4 bytes of a
  256-byte RSA signature into a 4-byte window and **returned 4**. The caller was
  told it succeeded. HotSpot: `SignatureException: partial signatures not
  returned`.
* **`Cipher.doFinal(byte[], int, int, byte[])`.** The write loop had no bounds
  check at all. HotSpot: `ShortBufferException: Output buffer must be (at least)
  32 bytes long`.

Neither is findable by asking "which exception does it raise", because the
answer is "none". The census question has to be the three-way one the task
posed — *raise it, raise something else, or raise nothing* — and it is the third
branch that produced the two worst rows here.

### 1.3 The constant-time contract, kept

`rsa_pkcs1_type2_unpad` and `rsa_oaep_unpad` were rewritten earlier in this
campaign (`nb-crypto-impl VULN(1)`) to collapse every structural failure into
one opaque string decided by a single branch over a bitwise mask — the
Bleichenbacher / Manger repair. Widening the exception surface is exactly the
kind of change that reopens that.

It does not, and the reason is structural rather than careful: **the length
decision is made before either function is entered.** A length that does not fit
is not secret — the caller chose it — and it is a different JCA class, so
`max_data_size` is consulted in `rsa_cipher_encrypt` and the `> k` test in
`rsa_cipher_decrypt`, above the constant-time region. Everything the two
unpadders can return maps to exactly one variant, `RsaCipherError::Padding`, so
the class a caller catches is identical for all of them.

`RSA_PADDING_ERROR` changed value, from `"RSA: decryption error"` to SunJCE's own
`"Padding error in decryption"`. Still exactly one constant, so it still
distinguishes nothing; it removes a second-order tell.

### 1.4 The AES key-wrap row, which is a wrong *sibling* rather than a wrong kind

`aes_key_wrap_do_final` answered an unwrap integrity failure with
`BadPaddingException`. Measured, SunJCE answers
`IllegalBlockSizeException: Integrity check failed` — for `AES/KW`, `AES/KWP` and
`AESWrapPad` alike. Both are checked, so `catch (GeneralSecurityException)` was
unaffected; the two are **siblings, not a hierarchy**, so a
`catch (IllegalBlockSizeException)` written against the JDK did not run.

Worth stating on its own because of how it was found: **this file already had a
second RFC 3394 arm** — the `"KW"` case of the block dispatch — and that one
already answered `IllegalBlockSizeException`. One algorithm, two arms, two
answers, depending on which surface the caller reached for. That is the same
shape as `W7-63`'s `Signature`-versus-`KeyFactory` disagreement about `ML-DSA`,
and the same fix applies: make each arm truthful about the *algorithm*, not
about the other arm.

---

## 2. AEAD — measured, and it was already right

`AEADBadTagException extends BadPaddingException`, so `catch
(BadPaddingException)` catches both and **a probe that only checks the base class
cannot tell them apart.** This is the trap, and it is not hypothetical here: this
campaign shipped a `Cipher` that served AES-256-ECB for ChaCha20 with no AEAD tag
at all, so tampered ciphertext decrypted cleanly
(`an-engine-that-validates-a-name-then-dispatches-on-something-else`).

**Both AEAD paths already raise `AEADBadTagException` specifically**, and neither
was touched:

| | |
| --- | --- |
| AES-GCM, flipped ciphertext / tag / wrong key / wrong IV / mismatched AAD | `AEADBadTagException: Tag mismatch` |
| AES-GCM, ciphertext shorter than the tag | `AEADBadTagException: Input too short - need tag` |
| ChaCha20-Poly1305, same two | same two |

That is stated as a measurement, not as a reassurance, so `RCrypto` now asks the
question in a way that can answer `no`:

```java
static String aead(Body body) {
    try { body.run(); return "NONE"; }
    catch (AEADBadTagException e) { return "AEADBadTagException"; }
    catch (BadPaddingException e) { return "BadPaddingException-BASE-ONLY"; }
    ...
}
```

Catching the **subclass first** is the whole mechanism. A VM that reported the
bare base class returns a distinct token instead of reading as a pass, which is
what a `catch (BadPaddingException)`-shaped probe would have done.

### 2.1 The other half of the AEAD guard is in the verifier table

See §3: `AEADBadTagException` is deliberately **not** flattened to
`GeneralSecurityException` in the static hierarchy table. Flattening it would
satisfy every "is this catchable as a `GeneralSecurityException`" assertion and
break the one link that makes a GCM tag failure catchable as
`BadPaddingException`.

### 2.2 The row where UNCHECKED is the correct answer

`Mac.doFinal` before `init` raises `IllegalStateException: MAC not initialized`
on HotSpot, and `Mac.doFinal` **declares** it. CratonVM already matched, and it
is now asserted with that exact expected value:

```java
check(macUninit.equals("UNCHECKED:IllegalStateException"), ...);
```

This is the anti-vacuity control for the whole section. Without it, a VM that
made *every* JCA refusal checked would pass every other row here. Unchecked is
not wrong by itself — it is wrong when the JDK's own method signature says
otherwise, which is the distinction the eight `Signature` sites got backwards
and this one gets right.

---

## 3. What the census walked into — the verifier's superclass table

`classloading::class_manager::jdk_superclass` is the verifier's fallback for
proving assignability when a `catch` type's class file is not loaded yet. It
carried the `java.security` half of the JCA hierarchy and **none of the
`javax.crypto` half.** So `jdk_name_is_subclass("javax/crypto/BadPaddingException",
"java/security/GeneralSecurityException")` answered **false**, and
`static_common_superclass_lookup` widened any two of them to `Object`.

Those are precisely the classes this record made the VM raise more of.

Two errors in the half that *was* present, both against the measured `H.` rows:

* `UnrecoverableKeyException` was listed as extending `GeneralSecurityException`.
  It extends `UnrecoverableEntryException`. **The transitive answer stayed
  right**, so "is it a `Throwable`" was never wrong and only the one question in
  between — does `catch (UnrecoverableEntryException)` match — came out wrong.
  A spot check on the useful question does not find this.
* `"java/security/InvalidKeySpecException"` **is not a class.** The real name is
  `java/security/spec/InvalidKeySpecException`, which was absent from the table
  entirely, and its superclass is `GeneralSecurityException`, not `KeyException`.
  The arm was keyed on a name that could never be looked up — the same species as
  a registration bound by a name nothing publishes.

Ratcheted by `jca_exception_hierarchy_matches_hotspot`, which asserts four
things and not one: the direct edges, the transitive question the fallback is
actually asked, the AEAD link on its own, and — anti-vacuity — that the walk can
still say **no**, including that no JCA exception is a `RuntimeException`. That
last assertion is the one this whole record is about.

---

## 4. Item 2 — `Files.write(Path, Iterable)` hardcoded `'\n'`

```
AssertionError: Iterable write: alpha\nbeta\n
```

`phases_late/nio_file.rs::write_iterable_impl` pushed a bare `'\n'` after every
element. The JDK's own implementation is `writer.append(line); writer.newLine();`
and `BufferedWriter.newLine()` is the platform separator, so HotSpot writes
`alpha\r\nbeta\r\n` on Windows. Both registered overloads — with and without a
`Charset` — forward into the same function, and it has a single registration
each; there is no competing registrar.

### 4.1 The population

| # | site | live? |
| --- | --- | --- |
| 1 | `phases_late/nio_file.rs` `write_iterable_impl` | **LIVE** — the reported red |
| 2 | `native-builtins/src/lib.rs` `bootstrap_property_fallback("line.separator")` | latent |
| 3 | `native-builtins/src/lib.rs` `host_line_separator` — feeds **every** `println` | latent |
| 4 | `native-io/src/lib.rs` `native_bw_new_line` | latent **and dead** |
| 5 | `native-builtins/src/properties_sidetable.rs` `build_store_text` | latent |

**#2 and #3 are `cfg!(windows)` blocks whose two arms are identical.** Verbatim:

```rust
"line.separator" => Some(if cfg!(windows) { "\n".to_string() } else { "\n".to_string() }),
```

A `cfg!` that decides nothing reads as a platform split and compiles as a
constant, and #3's doc comment said so out loud — *"`\n` on Windows, `\n`
elsewhere"* — which is a comment that describes the defect accurately and was
still read past. `file.separator` and `path.separator` sit immediately beside #2
and are both platform-correct, which is what makes the wrong one easy to miss.

**#4 is a LOSING registration.** `java/io/BufferedWriter.newLine()V` is
registered by both `native-io`'s `register_io_natives` and `phases_late`'s
`register_phase57_nio_file`, and phase 57 runs after `register_io_natives` in
every mode, so the correct copy wins and the wrong one was never observable. It
is corrected rather than deleted, because "the losing registrar disagrees with
the winner" is how a later ordering change turns a dead defect into a live one.

Three of the five now call one helper, `p57_line_separator`, rather than
open-coding the property read: the way they came to disagree is that each read
it — or failed to — on its own.

### 4.2 `%n` and `println` were checked and are **not** wrong

The brief noted that if these were also `\n` on Windows they would be the same
defect with a much wider blast radius. Measured: **they are not.**

* `PrintStream.println` / `PrintWriter.println` — all nine overloads funnel
  through `stream_writeln_inner` → `host_line_separator`, which is
  property-driven. Only its *fallback* was wrong (#3), so `println` was correct
  on every path where `line.separator` is seeded, which is every normal path.
* `%n` in `String.format` / `Formatter` / `printf` — three sites, all
  `if cfg!(windows) { "\r\n" } else { "\n" }`. Correct on Windows.
* `System.lineSeparator()` — two **method** registrations
  (`native-builtins/src/lib.rs:10551` and `:23349`); both are property-driven
  with a platform-correct fallback, so which one wins does not matter. A third
  site, `lang_system.rs:4122`, is not a registration at all — it seeds the
  `java/lang/System.lineSeparator` **static field** during `initPhase1`, and it
  is property-driven too. Correct on all three. (Re-checked 2026-08-12; §11.)
* `line.separator` seeding in `vm_init` — `"\r\n"` on Windows. Correct, and it is
  the authority the four fallbacks exist below.

The one residual, recorded and not fixed: the three `%n` sites decide at
**compile time**, so a `-Dline.separator=` override is not honoured there. The
JDK's `%n` follows `System.lineSeparator()`, which does honour it. Vanishing
blast radius, and touching the `String.format` hot path for it was not worth the
risk in this lane.

### 4.3 The asymmetry that makes this easy to fix backwards

`Files.write(Path, Iterable)` appends a separator after **every** element.
`Files.writeString` and `Files.write(Path, byte[])` append **nothing**. Read from
`lib/src.zip` before touching anything, because a repair that reaches for
"line-oriented output" as one category adds a separator where the JDK adds none —
and every assertion about the Iterable overload still passes while it does.

`RNioNoFollow` now asserts both directions explicitly. That is what the two new
`writeString` / `write(byte[])` arms are for; they are not coverage padding.

---

## 5. Proving the red

Both vectors already failed. They are extended where the census found gaps, and
**every expected value is measured on HotSpot**, never guessed.

### 5.1 `RCrypto`, 27 → 47 checks

The existing arms compared exception class NAMES. A name comparison cannot see
the property that decides whether a handler runs, so the new arms ask that
directly:

```java
static String kind(Body body) {
    try { body.run(); return "NONE"; }
    catch (Throwable t) {
        boolean unchecked = (t instanceof RuntimeException) || (t instanceof Error);
        return (unchecked ? "UNCHECKED:" : "checked:") + t.getClass().getSimpleName();
    }
}
```

`Throwable` and not `Exception` on purpose: an `Error` is unchecked too, and a VM
that raises a name which does not **resolve** produces `NoClassDefFoundError` —
a wrong-exception defect turned into a worse one. Catching only `Exception` would
let that sail past this instrument exactly as it sails past the caller's.

Also added: the hierarchy asked of the VM rather than assumed. If
`GeneralSecurityException` were made to extend `RuntimeException`, the whole
family would silently become unchecked and every class-name comparison in the
file would still pass.

Both traps the brief named are respected. Nothing here round-trips to decide an
outcome — every negative arm corrupts one specific input, so the arm names which
input the engine failed to consult. And nothing compares two refusals: every row
prints the class verbatim rather than a boolean "did the two agree", because two
engines that both throw compare one exception name with itself and answer
`true`.

Measured, HotSpot 25.0.3+9:

```
CK RCrypto rsaExc=checked:BadPaddingException,BadPaddingException,IllegalBlockSizeException,IllegalBlockSizeException,IllegalBlockSizeException
CK RCrypto gcmAeadClass=AEADBadTagException,AEADBadTagException
CK RCrypto aesExc=checked:BadPaddingException,checked:IllegalBlockSizeException
CK RCrypto kwExc=checked:IllegalBlockSizeException
CK RCrypto shortBufferExc=checked:ShortBufferException
CK RCrypto sigExc=checked:SignatureException,checked:SignatureException,checked:SignatureException,checked:SignatureException,checked:SignatureException
CK RCrypto macExc=UNCHECKED:IllegalStateException
PASS RCrypto (47 checks)
```

### 5.2 `RNioNoFollow`, 10 → 16 checks

`readAllLines` round-trip (the separator is a **terminator**, not part of the
content — a VM that appended it to the last element's *text* passes the byte
comparison and fails this); `Files.writeString` and `Files.write(byte[])` append
nothing; `BufferedWriter.newLine`; `%n`; `PrintStream.println` over a
`ByteArrayOutputStream`. One rule stated once and checked five ways, always
against `System.lineSeparator()` and never against a literal.

`CK RNioNoFollow lineSep=0d0a` on Windows, `PASS RNioNoFollow (16 checks)`.

### 5.3 The instrument

`probes/JcaExceptionTypeProbe.java` + `JcaExceptionTypeProbe.expected.txt`
(HotSpot 25, Windows, 91 rows, byte-identical across two runs). Sections `H`
(hierarchy), `C` (`Cipher`), `S` (`Signature`), `M` (`Mac`), `D`
(`MessageDigest`), `K` (keys and stores).

`regression-suite/harness-selfcheck.sh` after both fixture changes: **70 vectors
sound, 0 flagged.**

---

## 6. Compatible-mode exceptions taken

Compatible mode (`--real-jdk`) is contractually frozen except for genuine
HotSpot-parity bug fixes. **Every change in this record is a parity fix**, and
each touches `--jdk-only` and Compatible mode alike, because none of them is
mode-gated.

The safety argument is one sentence and it covers all of §1:

> **Raising a CHECKED exception where an unchecked one was raised cannot break a
> caller that compiles today.** `Cipher.doFinal` already declares
> `BadPaddingException` and `IllegalBlockSizeException`; `Signature.sign`,
> `verify` and `update` already declare `SignatureException`;
> `Cipher.doFinal(byte[],int,int,byte[])` already declares
> `ShortBufferException`. Any `catch` for them is therefore **already written**
> and was simply dead. This can only make a dead handler start working.

The two changes that are not covered by that sentence, stated separately:

1. **`Signature.update` before `init` now refuses.** It previously accumulated
   the bytes and returned. A caller relying on that was relying on `sign()`
   producing a signature over data it never meant to sign, which is not a
   contract anyone can have depended on deliberately. HotSpot refuses.
2. **`Signature.sign(byte[],int,int)` and `Cipher.doFinal(...,byte[])` refuse a
   short buffer instead of truncating.** A caller who was getting a truncated
   signature reported as success now gets a checked exception. This is a
   behaviour change in the direction of the specification and away from silent
   data corruption.

The verifier-table change (§3) is fallback-only and adds rows; nothing that
resolved before stops resolving.

No `CRATONVM_*` flag is introduced, so none of the four flag-surface files is
touched. No test or fixture is weakened: the only test assertion edited is
`mldsa_sign_without_key_fails_closed`, which moves from asserting the unchecked
`IllegalStateException` to the file's existing `assert_signature_exception`
helper — a *tighter* assertion, and one that still refuses a success.

---

## 7. What this record does NOT close

* **`%n` decides at compile time** and so ignores `-Dline.separator=` (§4.2).
  Three sites, `lang_string.rs` ×2 and `lib.rs` ×1.
* **AES-GCM accepts only a 12-byte IV.** SunJCE derives J0 by GHASH for any
  length. This is now a checked `InvalidAlgorithmParameterException` instead of
  an unchecked `IllegalStateException` — closed *and* catchable rather than
  closed and not — but the algorithm gap is real and open.
* **`RSA/ECB/NoPadding` is refused at `getInstance`** with
  `NoSuchPaddingException` because `RsaCipherPadding` has no `NoPadding` variant.
  Pre-existing, fails closed with a checked exception, deliberately untouched —
  but note `probes/JcaExceptionTypeProbe.java`'s `C.rsa.raw.*` rows will diverge
  for that reason and it is not this record's defect.
* **`KeyAgreement.generateSecret()` uninitialised** answers `IllegalStateException`
  here and `NullPointerException` on HotSpot (SunJCE's DH SPI dereferences before
  the state check). Both unchecked, and `generateSecret` **declares**
  `IllegalStateException`, so this is arguably the better answer. Recorded, not
  changed.
* **`Cipher.update(byte[],int,int,byte[])`** is not registered natively, so its
  short-buffer behaviour was not adjudicated. `doFinal`'s was.
* **No CratonVM binary was run in this lane.** Every claim about CratonVM's
  behaviour above is a claim about its SOURCE.

---

## 8. How to verify

```
java -cp <out> JcaExceptionTypeProbe > cratonvm.txt
diff probes/JcaExceptionTypeProbe.expected.txt cratonvm.txt
```

and both vectors through the suite:

```
JDK=<hotspot> regression-suite/run.sh   # ONLY="RCrypto RNioNoFollow"
```

The rows that must move from the pre-change binary:

```
RCrypto  oaepRefusals   IllegalStateException,IllegalStateException -> BadPaddingException,BadPaddingException
RCrypto  pkcs1Refusals  IllegalStateException                       -> BadPaddingException
RCrypto  (new)          rsaExc / gcmAeadClass / aesExc / kwExc / shortBufferExc / sigExc / macExc
RCrypto  PASS           (27 checks) -> (47 checks)
RNioNoFollow lineSep    absent on the failing arm -> 0d0a on Windows
RNioNoFollow PASS       (10 checks) -> (16 checks)
```

## 9. The single falsifying observation

If `CK RCrypto macExc` reads `checked:` anything on a CratonVM arm, the repair
overshot: something made the whole JCA refusal surface checked rather than making
each method truthful about **its own declared signature**, and every other row in
that section would still be green. That one row is worth more than the rest of
the section put together, for the same reason `W7-63`'s
`C.md.fallbackEqualsSha256` was: every other row can be satisfied by a VM that
does the right thing for the cases it knows about, and only this one asks what it
does with a case where the right answer is the one the fix was moving away from.

## 10. The one-line lesson

**A census of an exception surface has to ask three questions, not one.** "Which
exception does it raise" finds the five wrong classes and misses the two worst
rows entirely, because for those the answer is *none* — a truncated signature
reported as success, and a write past the end of the caller's array. The third
branch of *raise it, raise something else, or raise nothing* is where the silent
wrong answers live, and it is the branch a diff of two exception names cannot
have.

---

## 11. Source re-verification, 2026-08-12 (A15)

Read, not run. **Status is unchanged: FIXED in source, still NOT verified
against a binary.** This section only establishes that the source today says
what §1–§4 claim it says, so a later reader does not have to re-derive it before
running §8.

| claim | where | today |
| --- | --- | --- |
| §1 RSA errors carry the JCA class | `native-builtins/src/crypto_impl.rs:2517` | `RsaCipherError` present, three variants mapping to `IllegalBlockSizeException` / `BadPaddingException` / `InvalidKeyException` |
| §3 verifier table gained the `javax.crypto` half | `classloading/src/class_manager.rs:10739`, `:10744` | `javax/crypto/BadPaddingException` present; `AEADBadTagException` → `BadPaddingException`, **not** flattened to `GeneralSecurityException` |
| §3 `UnrecoverableKeyException` superclass | `class_manager.rs:10717` | → `java/security/UnrecoverableEntryException`. Corrected |
| §3 `InvalidKeySpecException` under its real name | `class_manager.rs:10709` | `java/security/spec/InvalidKeySpecException` present |
| §3 ratchet exists | `class_manager.rs:17191` | `jca_exception_hierarchy_matches_hotspot` present, including the AEAD link and the negative arms |
| §4 #1 `Files.write(Path, Iterable)` | `phases_late/nio_file.rs:7454` | `p57_line_separator` exists; both call sites (`:6346`, `:7379`) are inside `register_phase57_nio_file` |
| §4 #2 bootstrap property fallback | `native-builtins/src/lib.rs:127` | `"\r\n"` on Windows. The identical-arms `cfg!` is gone |
| §4 #3 `host_line_separator` | `native-builtins/src/lib.rs:26652` | property-driven, `"\r\n"` fallback on Windows |
| §4 #4 `native_bw_new_line` (losing registrar) | `native-io/src/lib.rs:3109` | corrected, and the comment states it is the losing registration |
| §4 #5 `build_store_text` | `properties_sidetable.rs:3450` | property-driven with a platform fallback |
| §7 residual: `%n` decides at compile time | `lang_string.rs:6092`, `:6500`, `lib.rs:41775` | **still three sites, still compile-time.** Residual stands |

One phrasing correction landed in §4.2: `System.lineSeparator` has two *method*
registrations plus a static-field seed, not "two registrations". The substance
was right — every one of the three is property-driven — but the count was
counting two different kinds of thing, which is the shape that makes a "which
one wins" argument unfalsifiable.

**Nothing in §1–§6 is retired by this pass**, because none of it can be:
retirement here needs the §8 binary run, and this lane could not build either.
The record's own header already says so and it stays.
