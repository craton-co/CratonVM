# Lane 6 — the JCA argument contracts

**Date:** 2026-09-12
**Probe:** `apps/probes/L6JcaSweep.java`
**Predecessor:** [`lane-6-http-carrier-residuals-20260912.md`](lane-6-http-carrier-residuals-20260912.md), §9
**Result:** `L6JcaSweep` **17 rows → 1**, control arm at the same revision.

---

## 1. Why this probe, and why now

The predecessor's §9 closed with one line about this family: *"`L6JcaSweep`'s 34
diff lines. No §9 item names them and this wave did not open the probe."*
Neither did the two L6 waves before it. It was the largest measured-and-never-
attacked block on the board, and no other lane's branch touches `jca/`.

**34 was diff LINES, not rows.** A differing row prints two lines, one per VM,
so the sweep had **seventeen** rows. The same arithmetic corrects the
predecessor's own table: `L6HttpLoopbackSweep`'s "6" is 3 rows and
`L6TlsParamSweep`'s "22" is 11. Counting lines overstates a family by exactly
2× and makes a small residue look like a campaign.

## 2. The measurement

Two binaries, one worktree, base `6c8ad94d4`:

| | base | trial |
|---|---|---|
| `L6JcaSweep` | **17** | **1** |
| `L6HttpLoopbackSweep` | 3 | 3 |
| `L6TlsParamSweep` | 11 | 11 |
| `L6HttpLogicSweep` | 0 | 0 |
| `L6SocketSweep` | 0 | 0 |
| `L6UriSweep` / `L6UrlSweep` / `L6InetSweep` / `L6X500Sweep` | 0 | 0 |

The control arm is the point of the second column: every other sweep answers
identically on both binaries, so the sixteen closed rows are this change and
nothing else moved.

| row | what it asked | was | is |
|---|---|---|---|
| 68 | `Cipher.getInstance(null)` | `NPE: null object argument` | `NoSuchAlgorithmException: Null or empty transformation` |
| 78 | `Signature.getInstance(null)` | `NoSuchAlgorithmException` | `NPE: null algorithm name` |
| 80 | `KeyFactory.getInstance(null)` | `NoSuchAlgorithmException` | `NPE: null algorithm name` |
| 99 | AES cipher, Blowfish key | *no throw* | `InvalidKeyException: Wrong algorithm: AES or Rijndael required` |
| 108 | `Cipher.init(mode, null)` | `NPE: null object argument` | `InvalidKeyException: No installed provider supports this key: (null)` |
| 124 | `Mac.init(null)` | *no throw* | same `InvalidKeyException` |
| 149 / 150 | `Signature.initSign/initVerify(null)` | *no throw* | `InvalidKeyException: Key must not be null` |
| 153 | PSS spec on `SHA256withRSA` | *no throw* | `InvalidAlgorithmParameterException: No parameters accepted` |
| 155 | `initVerify` with a non-RSA key | our own sentence, and false | `InvalidKeyException: No installed provider supports this key: L6JcaSweep$1` |
| 156 / 157 | `generate{Public,Private}` with a garbage encoding | our own sentence | `java.security.InvalidKeyException: Unable to decode key` |
| 158 | `generatePublic(null)` | the decode-failure message | `InvalidKeySpecException: keySpec must not be null` |
| 160 | `KeyPairGenerator("RSA").initialize(0)` | `IllegalArgumentException` | `InvalidParameterException` with the JDK's text |
| 186 | `Provider.getService(null, alg)` | `null` | the JVM's own helpful NPE |
| 193 | `Security.addProvider` on a duplicate | `1` | `-1` |
| 161 | EC `initialize(bogus curve)` | *no throw* | **still open — §7** |

## 3. The first command was the registry dump, and it earned its keep twice

Before editing a single registration, `--dump-native-registry` named the live
owner of every triple in the table above. Two of them are not where the name
says:

* **`javax/crypto/Mac.init` is registered in
  `native-builtins/src/phases_late/ssl_security.rs`**, not anywhere under
  `jca/`. Every other Mac method is in `jca/`; `init` is not.
* **The live `java/security/Security.addProvider` is
  `jca/provider_chain.rs:7939`.** There is a second registration in
  `phases_early.rs:18138` that a grep finds first and that never runs in a
  shipping build.

Every row came back `overwrote=None`, so there was no ordering question to
resolve either. One command, two builds saved — the same lesson the predecessor
recorded, applied before the mistake instead of after it.

## 4. A guard that had never run

`cipher_init_record_with_counter` has carried this since row 108 was first
measured:

```rust
let key_bytes = extract_key_bytes(ctx, key);
if key_bytes.is_empty() && !is_rsa_transformation(...) {
    return Err(throw_jca_exc(ctx, "java/security/InvalidKeyException",
        "No installed provider supports this key: (null)"));
}
```

Right exception, right message, cites the row. It has never executed for a null
key, because all six `init` overloads reach it through:

```rust
let key = obj_arg(args, 2)?;
```

which throws this VM's own `NullPointerException: null object argument` one line
earlier. The refusal is unreachable, and the source reads as though the row were
already closed — a comment saying MEASURED next to code that cannot run is worse
than no code at all, because it retires the question.

This is the same shape as the predecessor's §5, one crate over: *a guard in a
path the dispatch never reaches is not a guard.* The fix adds
`cipher_require_key` at the six registration sites and keeps the deeper check as
the backstop for a key that is present but yields no bytes.

## 5. The null contracts are inverted, not missing

The tempting fix here is one shared null guard. It would have closed three rows
by breaking two, because the JCA factories genuinely disagree:

```
Cipher.getInstance(null)     -> NoSuchAlgorithmException: Null or empty transformation
Signature.getInstance(null)  -> NullPointerException: null algorithm name
KeyFactory.getInstance(null) -> NullPointerException: null algorithm name
```

`Cipher.tokenizeTransformation` treats null and `""` alike and reports both the
same way; everything else runs `Objects.requireNonNull(algorithm, "null
algorithm name")` in `GetInstance` before any provider is consulted. This VM
answered `NoSuchAlgorithmException` for all three — right for `Cipher` by
accident, wrong for the other two — while `Cipher` separately answered the
generic `obj_arg` NPE. So `Cipher` is the one class that needs its own guard,
and it is the one class that did not have one.

## 6. Three defects that a "message wording" reading would have missed

**Row 99 is silent algorithm substitution.** An AES cipher accepted
`SecretKeySpec(bytes, "Blowfish")`, took the sixteen bytes and encrypted with
AES. The caller believes it chose Blowfish and gets AES ciphertext with no
indication. Same shape as the `AesFixed` key-length defect one field over, and
worse, because the wrong answer is the algorithm rather than its strength. The
key's algorithm is read from the `algorithm` FIELD, not through
`getAlgorithm()`: this runs between the argument checks and the state write, and
a re-entry into the VM there can move both `this` and `key` under the caller. A
key that does not declare the field is not policed, which is the conservative
direction.

**Row 160 answered a SUPERCLASS.** `InvalidParameterException` extends
`IllegalArgumentException`, so a caller catching the specific type caught
nothing at all. The JDK's message looks like a mistake and is not: RSA keygen
validates the public exponent against the modulus size, the default F4 is
seventeen bits and does not fit in a zero-bit modulus, and
`KeyPairGenerator.initialize(int)` cannot throw the checked
`InvalidAlgorithmParameterException` that carries the complaint — so it rewraps
that exception's `toString`, type prefix and all.

**Row 155's message was FALSE, not merely different.** It said `getEncoded()
returned null`. The probe's key returns three bytes. Two different keys reach
that refusal and the JDK separates them: an RSA key with no material gets
`Missing key encoding` (measured on Temurin 25.0.3+9, and load-bearing for
netty's provider search); a key that is not RSA at all never reaches an RSA
engine, so provider selection fails first and the message names the key's
CLASS. Collapsing both into the first sends the reader to inspect an encoding
that was never the problem.

## 7. What this wave did not do

**Row 161 — `KeyPairGenerator("EC").initialize(new ECGenParameterSpec("no-such-curve"))`.**
HotSpot refuses at `initialize` with `InvalidAlgorithmParameterException:
Unknown curve name: no-such-curve`; this VM stashes the name and accepts. To
refuse at the same point this VM needs a list of curve names to reject
*against*, and it does not have one: the curve is honoured later by the real
SunEC drive, which is the only authority on what is valid. Any list hardcoded
here would eventually refuse a curve SunEC accepts, and a false refusal on a
valid curve is a worse defect than the open row — it fails a working
application. Closing it properly means asking the real
`AlgorithmParameters.getInstance("EC")` at `initialize` time, which is a
re-entry into the VM on a path that currently has none.

**`insert_at`, the unmeasured sibling of row 193.** `Security.addProvider`
returned the provider's POSITION for an already-installed provider where the
JDK returns `-1`; a caller guarding on `>= 0` concluded it had registered.
`insertProviderAt` next to it has the identical shape and no probe row. It is
left alone: this lane has been bitten by edits with nothing to measure them
against, and the honest move is to record it here rather than change it blind.

## 8. Gates

Merged tree `0a805f3fc` (dev + this wave), release binary built from it at
20:18:48Z. The two edits made after that build are both inside `mod tests` — a
helper and an assertion, §8.2 — so the binary the arms scored is the binary this
change lands.

### The arms — all three green

| arm | result |
|---|---|
| `CRATONVM_ARGS=--jdk-only` | **136 passed, 0 failed** |
| `SUITE=all` | **136 passed, 0 failed** |
| `SUITE=core` | **95 passed, 0 failed** |

The `--jdk-only` arm was 135/1 on the previous run against `RArrayStoreLibrary`,
a harness-hygiene error in a vector that arrived on `dev` without a check count.
Its lane fixed it; nothing here did.

### The cargo set

| config | result | whose |
|---|---|---|
| `cratonvm-native-api --tests` | 0 | — |
| `cratonvm-native-io --tests` | 0 | — |
| `cratonvm-native-builtins --tests` | 101 | **not this wave's** |
| `… --features management --tests` | 101 | **not this wave's** |
| `… --features synthetic-jdk --tests` | 101 | **not this wave's** |
| `cratonvm-types` | 101 (4 targets) | **not this wave's** |

`native-builtins` is red in all three configurations on exactly one target, and
it is the same one each time:

```
unsafe_natives_ext::unsafe_unaligned_read_tests::
    the_byte_array_bulk_route_agrees_with_the_per_element_loop
panicked at native-builtins/src/unsafe_natives_ext.rs:7347:55:
    attempt to multiply with overflow
```

The fixture computes `i as u8 * 2 * 7 + 1`, in `u8`, before widening to `u16`.
That overflows at `i == 19` (267 > 255) and a debug build panics, so the whole
`--lib` suite aborts. The defect is in the test's own setup, not in the bulk
read path it exercises; the test arrived with `9b7df29bb` (perf/nio). This wave
does not touch that file. Everything else passes: **4273 passed, 1 failed**.

The four `cratonvm-types` targets are likewise foreign, and each names a file
this wave does not modify:

* `a_record_does_not_claim_a_fix_it_never_ran` —
  `docs/known-issues/tomcat/tls-handshake-enforcement-gaps-20260912.md`, which
  arrived with `d35f6dac2`.
* `every_relocatable_doc_citation_points_at_the_page` —
  `classloading/src/class.rs:520` cites a springboot page that has moved.
* `architecture_per_crate_loc_table_matches_reality` — the `jit` row, 5.11%
  drift. `native-builtins` is on the same table at 1.60% and passes, which is
  the row this wave could have moved.
* `flag_inventory_surface_counts_are_current` — 1,419 claimed against 1,420
  actual. This wave adds no `CRATONVM_` token.

### The script gates

`jdk-only-census.sh` 0. `jdk-only-refusal-survivors.sh` and `bridge-ratchet.sh`
are red for exactly the reasons the predecessor record's §10 sets out and in
exactly the same shape — five `-` rows on `java/util/logging`, and a missing
committed baseline for the `25/linux/jdk-only` key. Neither has moved.

### The two tests this change had to correct

Both are cases where the existing test recorded the implementation rather than
the contract, so "make it pass again" would have meant reverting a fix the
oracle confirms.

**`jca::provider_chain::tests::add_provider_idempotent`** asserted
`add("SUN", 25.0) == 1`. Its own comment says the point is that re-adding must
not duplicate, and the `snapshot().len()` assertion beside it already checks
that. The position was incidental, and it was wrong: `Security.addProvider`
answers `-1` for an already-installed provider, which is what a real JDK
answers for `L6JcaSweep` row 193. The assertion is now `-1`.

**`make_inited_sig`**, shared by three signature tests, called
`sig_init_sign(ctx, &[sig, null])` as a shortcut to the state those tests want:
INITED, holding no key. Rows 149 and 150 make that call throw. Routing the
helper through a call that now throws would have tested the new guard twice and
the fail-closed contract not at all, so the helper sets the state directly when
no key is given.

### The probes

Two binaries, one worktree, base `6c8ad94d4`, in ROWS:

| probe | base | trial |
|---|---|---|
| `L6JcaSweep` | **17** | **1** |
| `L6HttpLoopbackSweep` | 3 | 3 |
| `L6TlsParamSweep` | 11 | 11 |
| `L6HttpLogicSweep` | 0 | 0 |
| `L6SocketSweep` | 0 | 0 |
| `L6UriSweep` / `L6UrlSweep` / `L6InetSweep` / `L6X500Sweep` | 0 | 0 |

Sixteen rows closed. Every other sweep answers identically on both binaries,
which is what the second column is for.
