# E3-1 — the cipher-name helper had 1 caller out of 8, and the sixth of the seven nominated sites did not need it

**Status: FIXED-UNVERIFIED.** **Prov: HotSpot column MEAS (this host); CratonVM
column PRED.** **2026-08-13, lane E3.** Closes NOMINATION 2 of
`C12-2-https-session-capture-and-the-cipher-name-it-records.md`.

**This lane may not build or run the VM.** Every CratonVM "after" below is
**PREDICTED**. The JSSE numbers are measured on this host
(HotSpot 25.0.3+9-LTS, `scratchpad/e3/E3CipherProbe.java`,
`scratchpad/e3/E3PassThrough.java`); the rustls facts are read from
`rustls-0.23.38`, the version `Cargo.lock` resolves; the landed Rust was
type-checked and executed with plain `rustc` on a self-contained extract
(`scratchpad/e3/extract.rs`).

---

> **VERIFIED AGAINST A BINARY 2026-09-03.** This record's status was
> **FIXED-UNVERIFIED**, *"CratonVM column PRED"*, *"This lane may not build or
> run the VM."* §5 gave two verification commands in order of cost. **Both were
> run, and both pass.**
>
> ```text
> cargo test -p cratonvm-native-builtins t27_loopback_self_test
>   1 passed; 0 failed
> cargo test -p cratonvm-native-builtins the_only_rustls_suite_spelling_left_is_the_adapters_own
>   1 passed; 0 failed
> ```
>
> **The first one is a behavioural check, not a compile.** It performs a real
> in-process TLS 1.3 handshake and asserts the spelling this record is about:
>
> ```rust
> assert!(msg.contains("cipher=TLS_"),  "cipher must carry JSSE's spelling: {}", msg);
> assert!(!msg.contains("cipher=TLS13_"));
> ```
>
> §5 says of it: *"It fails today without the fix, which is what makes it worth
> more than the witness."* The assertion is present, it runs, and it is green.
>
> **What this does NOT verify.** §5 opens *"No in-tree fixture observes this
> across a real network handshake"*, and that is still true — the loopback test
> is in-process. The `javap`/JSSE denominator in §0 (one caller out of eight,
> and the sixth of seven nominated sites) is a source and oracle census; it was
> not re-counted here. This note verifies the two commands the record itself
> nominated, and no more. The claim "it fails today without the fix" was NOT
> re-proven by mutation.

## 0. Verdict

C12-2 NOM 2 nominated seven sites. **Six were adapted; one was not, and should
not be.** The line numbers in the nomination were all seven correct — verified
against the working tree before any edit, which is not the usual outcome for a
brief in this project.

| site (pre-edit) | function | where the string goes | needed it? |
|---|---|---|---|
| `:3254` | `rustls_client_connect` | `TlsClientStreamEntry::negotiated_cipher` → `rustls_session_info` → `SSLSession` slot 1 | **YES** |
| `:3418` | `rustls_server_accept` | `TlsServerStreamEntry::negotiated_cipher` → same | **YES** |
| `:3601` | `rustls_server_handshake_over_stream` | same | **YES** |
| `:3676` | `rustls_client_handshake_over_stream` | same | **YES** |
| `:9419` | `build_synthetic_ssl_session` | `SSLSession.getCipherSuite()` / `getHandshakeSession()`, directly | **YES** |
| `:5898` | `run_loopback_self_test` | `cratonvm.tls.T27SelfTest.run()`, a VM-private diagnostic | **cosmetic — applied anyway, and it is now the only test that can catch this** |
| `:9011` | `engine_take_pending_trust_check` | never leaves Rust; sole reader is `contains("ECDSA")` | **NO — provable no-op** |

C12-2's own "Checked before nominating" paragraph named the `:9067` `auth_type`
consumer as unaffected. That paragraph is the proof that `:9011` is unnecessary,
written one section above the nomination that included it. The brief that sent
this lane repeated the warning about `:9067` and asked whether all seven
genuinely need the spelling; they do not, and this is the one that does not.

## 1. The oracle, measured

`SSLSession.getCipherSuite()` is contracted to return the IANA registry name.
`scratchpad/e3/E3CipherProbe.java`, HotSpot 25.0.3+9-LTS on this host:

```
vm = OpenJDK 64-Bit Server VM 25.0.3+9-LTS

--- A. SSLContext.getDefault() supported/enabled ---
supported count = 31
default(enabled) count = 31

--- B. rustls spelling vs JSSE spelling: which does JSSE know? ---
rustls {:?} name                 known   asserted JSSE name               known
TLS13_AES_128_GCM_SHA256         false   TLS_AES_128_GCM_SHA256           true
TLS13_AES_256_GCM_SHA384         false   TLS_AES_256_GCM_SHA384           true
TLS13_CHACHA20_POLY1305_SHA256   false   TLS_CHACHA20_POLY1305_SHA256     true
TLS13_AES_128_CCM_SHA256         false   TLS_AES_128_CCM_SHA256           false
TLS13_AES_128_CCM_8_SHA256       false   TLS_AES_128_CCM_8_SHA256         false

suites in JSSE's supported list starting with TLS13_ = 0

--- C. every TLS 1.3 suite JSSE actually lists ---
  TLS_AES_256_GCM_SHA384
  TLS_AES_128_GCM_SHA256
  TLS_CHACHA20_POLY1305_SHA256

--- E. live handshake: SSLSession.getCipherSuite() ---
  getProtocol()    = TLSv1.3
  getCipherSuite() = TLS_AES_256_GCM_SHA384
  starts with TLS13_ ? false
  starts with TLS_   ? true
```

**Zero of HotSpot's 31 supported suites are spelled `TLS13_`**, and a live
TLS 1.3 handshake answers `TLS_AES_256_GCM_SHA384`. TLS 1.3 is this VM's
default, so the divergence was on essentially every connection.

The two CCM rows are an honest gap, not a contradiction: HotSpot implements no
CCM suite at all, so it cannot witness the name either way. The rewrite produces
the IANA registry spelling for them (`0x1304`/`0x1305`), and rustls's default
provider does not offer them, so no connection reaches that row today.

### The pass-through half, which had never been measured

The helper rewrites `TLS13_` and returns everything else unchanged. That is only
correct if rustls's other variant names are byte-identical to JSSE's. C12-2 and
D3-3 both asserted this from rustls's source ("everything else already
registry-named") without asking JSSE. `scratchpad/e3/E3PassThrough.java` asks
JSSE, over all 23 `CipherSuite` variants rustls 0.23.38 declares:

```
JSSE supported-suite count = 31   (HotSpot 25.0.3+9-LTS)

rewritten (TLS13_ -> TLS_)          = 5
passed through, JSSE knows the name = 15
passed through, JSSE does NOT know  = 3

--- would any name be MANGLED by the rewrite (a false positive)? ---
  mangled = 0 (0 means the rewrite touches only TLS13_)
```

All 14 negotiable TLS 1.2 suites — every `TLS_ECDHE_*` variant, GCM, CBC and
ChaCha20 — are byte-identical under both spellings. The 3 JSSE does not know are
`TLS_NULL_WITH_NULL_NULL` and the two `TLS_PSK_*`, none of which this VM
negotiates and all of which already carry the registry name. **The claim now
rests on a measurement rather than on a reading of the producer's source.**

rustls's variant list is `rustls-0.23.38/src/enums.rs:113`-`145`; exactly five
variants carry the `TLS13_` prefix, which is why a prefix rewrite is exact
today.

## 2. `:9011` — why the change is a no-op, and why the site now says so

`engine_take_pending_trust_check` stores the name in
`PendingTrustCheck::negotiated_cipher_suite_name`. That field has four mentions
in the whole tree (declaration, this write, a `None` literal, and one read) and
exactly one reader:

```rust
    let auth_type = match pending.negotiated_cipher_suite_name.as_deref() {
        Some(s) if s.contains("ECDSA") => "ECDSA",
        _ => "RSA",
    };
```

The only thing that reaches Java is the `"ECDSA"`/`"RSA"` literal, as the
`authType` argument to `checkServerTrusted`/`checkClientTrusted`. The rewrite
can only alter a `TLS13_` name, and no TLS 1.3 suite contains `ECDSA` under
either spelling — asserted mechanically over all five variants in
`scratchpad/e3/extract.rs`, not argued:

```
ECDSA invariance: OK for all five TLS13_ variants
```

Applying the helper there would change nothing and would additionally assert
"this value is a JSSE cipher name" about a value whose job is "a rustls suite
name used to guess an auth type". **Left raw** — but unlike D3-3, which
concluded the record was the right place for that answer, this lane put a ten-line
comment at the site. A reader who greps the shape lands on the code, not on this
file, and the next lane to sweep this family must not have to re-derive it. It
also makes the witness test's single exception legible where it applies.

## 3. What landed

`native-builtins/src/http_url_connection.rs`

* `jsse_cipher_suite_name` is now `pub(crate)` (pre-approved by C12-2 NOM 2;
  this lane owns the file). Its doc comment names the reason.

`native-builtins/src/t27_tls.rs`

* **One private adapter**, `negotiated_suite_name(rustls::SupportedCipherSuite)
  -> String`, above `TlsClientStreamEntry`, carrying the measurement above. The
  file now has ONE place that knows the translation instead of seven that each
  re-spell by hand, so **the number of sites that can drift is 2 (one per file)
  rather than 8.**
* Six call sites rewritten to `.map(negotiated_suite_name)`:
  `rustls_client_connect`, `rustls_server_accept`,
  `rustls_server_handshake_over_stream`, `rustls_client_handshake_over_stream`,
  `run_loopback_self_test`, `build_synthetic_ssl_session`.
* `engine_take_pending_trust_check` unchanged, plus the comment of §2.
* **`t27_loopback_self_test` now asserts the cipher substring.** It asserted
  `starts_with("OK ")`, `contains("proto=TLSv1.3")` and `contains("alpn=h2")`
  and **never the cipher** — which is precisely why the rustls spelling survived
  in that string. It runs a real in-process TLS 1.3 handshake, so it is the only
  behavioural exercise of the adapter that needs no network:
  `contains("cipher=TLS_")` and `!contains("cipher=TLS13_")`.
* **A source-witness test**,
  `the_only_rustls_suite_spelling_left_is_the_adapters_own`, reading the working
  tree (`\r`-normalised — this repo is edited from Windows and Linux). It
  asserts the raw `{:?}`-on-a-suite idiom appears in exactly two functions:
  `negotiated_suite_name` and `engine_take_pending_trust_check`. A ninth
  producer, or a revert of one of the six, fails it with a message naming the
  adapter. Its needle is assembled from three fragments at runtime so the test's
  own source does not contain the string it searches for.

The witness was **mutation-checked**, not merely run: `extract.rs` feeds it a
fixture with a ninth producer appended and asserts it notices.

```
witness(fixture)   = ["engine_take_pending_trust_check", "negotiated_suite_name"]
witness(regressed) = ["engine_take_pending_trust_check", "negotiated_suite_name", "some_new_site"]
ALL SHAPES OK
```

The same extract executes the landed call shapes against stand-ins mirroring
`SupportedCipherSuite`/`suite()` (`rustls-0.23.38/src/suites.rs:64`-`79`,
`src/common_state.rs:155`-`157`): `TLS_AES_256_GCM_SHA384` for TLS 1.3, the
`UNKNOWN`/`?`/`TLS_AES_256_GCM_SHA384` fallbacks on `None`, and TLS 1.2
pass-through untouched. `SupportedCipherSuite` is `Copy`, so taking it by value
costs nothing.

## 4. THE DENOMINATOR — how many callers there really are

The brief asked for this rather than for seven fixes, on the grounds that this
is the **eighth instance this session** of *"a correct helper exists and the
callers don't use it"*. Grepping the shape — every place a rustls suite name is
produced — over first-party code (`native-builtins/vendor/` is a vendored rustls
fork; `examples/` is not shipped):

| # | producer | status before | status after |
|---|---|---|---|
| 1 | `http_url_connection.rs:2723` | **correct** — the helper's only caller | unchanged |
| 2–5 | `t27_tls.rs:3254 :3418 :3601 :3676` | wrong, Java-visible via `rustls_session_info` | adapted |
| 6 | `t27_tls.rs:9419` | wrong, Java-visible directly | adapted |
| 7 | `t27_tls.rs:5898` | wrong, VM-private diagnostic | adapted |
| 8 | `t27_tls.rs:9011` | raw, never reaches Java | left raw, documented |

**Eight producers. Seven hand the name to Java, and exactly one of those seven
used the helper — a denominator of 1/7.** `jsse_cipher_suite_name` additionally
had six *test* callers and no others: **a helper better covered by its unit tests
than by its callers.**

The wider sweep found no ninth. Every other cipher-suite string in the tree is a
hardcoded JSSE-spelled literal (`net_phase_e.rs:203`, `tls.rs:27`,
`ssl_security.rs:2463`, `servlet.rs:2280`, `tls_impl.rs:90`) or the reverse
mapping `java_cipher_name_to_suite`. **`cs.suite()` is the whole family**, and it
now has one translation point per file.

### The variation worth recording

The other seven instances this session were *"someone forgot the helper"*. This
one is not. The helper was written in the same commit as its single caller, by a
lane that had **measured the divergence, knew the other seven sites existed, and
listed them by line number** — then landed the fix for the site it was standing
on and wrote the rest into a document. C12-2's own §2 says the idiom is used
"at this call site and at seven more".

So the failure mode is not ignorance of the helper. It is that **the family was
named in a document instead of in code**, and a document cannot fail a build.
The seven sat unpatched for a day, through at least two lanes that read the
nomination.

The corollary is the reason this lane added the witness test rather than only the
six call edits: **the helper's six unit tests test the FUNCTION, and nothing
asserted that anyone CALLS it.** That is the gap that made 1/7 survivable, and it
is the same gap in every one of the eight instances. A unit-tested helper with an
unenforced call graph is the shape to grep for next.

## 5. How to verify

No in-tree fixture observes this across a real network handshake. In order of
cost:

1. **`cargo test -p cratonvm-native-builtins t27_loopback_self_test`** — a real
   in-process TLS 1.3 handshake, now asserting `cipher=TLS_` and not
   `cipher=TLS13_`. This is the cheapest real check and it needs no network and
   no VM. **It fails today without the fix**, which is what makes it worth more
   than the witness.
2. **`... the_only_rustls_suite_spelling_left_is_the_adapters_own`** — pure
   source reading, catches a ninth producer or a revert.
3. **`cratonvm ... cratonvm.tls.T27SelfTest.run()`** — needs a configured
   keystore, no network. PREDICTED to print
   `OK proto=TLSv1.3 cipher=TLS_AES_256_GCM_SHA384 alpn=h2`; before the change it
   printed the `TLS13_` name.
4. **The embedded-Tomcat HTTPS fixture** (`P4A-TOMCAT-20260812.md` §2):
   `SSLSession.getCipherSuite()` after a successful handshake must match `^TLS_`
   and must not match `^TLS13_`. Covers the client and server stream sites
   depending on which side the fixture inspects.
5. **`SSLEngine.getSession().getCipherSuite()`** — the only route to
   `build_synthetic_ssl_session`.
6. **The round trip that never closed**: `java_cipher_name_to_suite(
   session.getCipherSuite())` should now resolve, where before it returned
   nothing for every TLS 1.3 connection. A pure Rust assertion, and the cheapest
   regression guard nobody has added yet.

| | before | HotSpot 25 (MEAS) | after (PRED) |
|---|---|---|---|
| `SSLSession.getCipherSuite()`, TLS 1.3 | `TLS13_AES_256_GCM_SHA384` | `TLS_AES_256_GCM_SHA384` | `TLS_AES_256_GCM_SHA384` |
| same, TLS 1.2 | `TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256` | identical | unchanged |
| `T27SelfTest.run()` | `cipher=TLS13_…` | n/a (VM-private) | `cipher=TLS_…` |
| `checkServerTrusted` `authType` | `"RSA"`/`"ECDSA"` | `"RSA"`/`"ECDSA"` | unchanged (invariant) |
| `java_cipher_name_to_suite(getCipherSuite())` | `None` on every TLS 1.3 conn | n/a | resolves |

## Residuals

1. **The `unwrap_or_else` fallbacks are all wrong, and this lane did not touch
   them.** Measured on this host, and new to this record — no prior record in
   this family checked it:

   ```
   --- F. unhandshaken SSLSocket session ---
     getProtocol()    = NONE
     getCipherSuite() = SSL_NULL_WITH_NULL_NULL

   --- G. SSLEngine unhandshaken getSession() ---
     getProtocol()    = NONE
     getCipherSuite() = SSL_NULL_WITH_NULL_NULL
     getHandshakeSession() = null
   ```

   JSSE answers `SSL_NULL_WITH_NULL_NULL` / `"NONE"` for a session that has not
   handshaken. The five adapted sites fall back to `"UNKNOWN"`, `"?"` or — at
   `build_synthetic_ssl_session` — the **fabricated** `"TLS_AES_256_GCM_SHA384"`,
   which is a plausible-looking wrong answer of exactly the kind this directory
   keeps recording. The sibling `protocol` expression falls back to `"TLS"`,
   where JSSE says `"NONE"`. D3-3 residual 3 noticed the protocol half from
   source; the cipher half and the HotSpot values are measured here. **Not fixed:
   it is a behaviour change with its own consumers** (`ssl_security.rs:2882` and
   `:9419` both carry fabricated literals that something may depend on), and it
   deserves its own reviewable commit rather than riding along on a rename.
   `getHandshakeSession()` returning `null` before a handshake is a third answer
   again, and `build_synthetic_ssl_session` serves both.
2. **`jsse_cipher_suite_name` is still a prefix rewrite, not a table**
   (C12-2 residual 3). Now measured exact for all 23 variants rustls 0.23.38
   declares, with zero false positives — but a future rustls variant that
   diverges some other way returns unchanged and the divergence is silent.
   Narrower than it was, not closed.
3. **The two CCM suites are unwitnessed.** HotSpot implements no CCM suite, so
   the asserted names `TLS_AES_128_CCM_SHA256` / `TLS_AES_128_CCM_8_SHA256` come
   from the IANA registry via rustls's own ordinals, not from the oracle. rustls's
   default provider does not offer them, so nothing reaches that row today.
4. **Nothing here was built or run against CratonVM.** The Rust was type-checked
   and executed only as a standalone extract with stand-in types.

## NOMINATION 1 — `docs/known-issues/jdk-only/INDEX.md` (not this lane's file)

The index is a 2026-08-13 00:07 snapshot and says so; this record and D3-3 are
both newer. When it is next retaken, `D3-3-rustls-cipher-names-reaching-jsse.md`
should be marked **SUPERSEDED** by this record: its patch is applied, its §2
verdict on `:9011` is upheld, and its two unmeasured claims (the pass-through
half, and residual 3's protocol fallback) are measured here.

No literal edit is offered because the file is a generated snapshot with a
recount instruction in its own header — a hand-patched row would be the exact
rot it warns about.

## NOMINATION 2 — `native-builtins/src/phases_late/ssl_security.rs` (not this lane's file)

Two fabricated cipher literals sit in the consumer of the values this record
fixes, and they are the other half of residual 1. **No edit is proposed** — the
right answer depends on what `getCipherSuite()` should say for an unhandshaken
session, which is residual 1's open question, and changing a fabricated literal
to `SSL_NULL_WITH_NULL_NULL` without tracing its consumers is how a rename turns
into an outage.

Flagged for whoever picks up residual 1:

* `ssl_security.rs:1713` — `String::from("TLS_AES_128_GCM_SHA256")` as the
  `SSLSession` builder's fallback.
* `ssl_security.rs:2882` — the same literal as `new13_finish_socket`'s fallback.

Both are JSSE-spelled, which is a tell that the consumer has always expected the
registry name — that is the second of the two independent tells that the six
adapted sites were wrong. Neither is reached when a handshake completed.
