# F6-1 — the arm that had to move, what it decides, and the two minters it deliberately keeps wrong

**Status: FIXED-UNVERIFIED (`native-builtins/src/t27_tls.rs`, `native-builtins/src/tls.rs` — this lane's files); NOMINATED (the rest).**
**Prov: HotSpot column MEAS for §4 (this host, `scratchpad/f6/F6Invalidate.java`, JDK 25.0.3+9-LTS `Microsoft-13877124`, three byte-identical runs); every other HotSpot row cited from E12-1/E42-1 rather than re-derived; tree citations READ; CratonVM column PRED.**
**2026-08-13, lane F6.** Lands E42-1's **NOMINATION 1 (BLOCKING)**, **NOMINATION 2**, and the two-of-five files half of **NOMINATION 3**.

**This lane may not build or run the VM.** Every CratonVM "after" below is
**PREDICTED**. No `cargo`, no VM, no fixture. The Rust was parse-checked with
`rustfmt --edition 2021 --emit=stdout` on a scratch copy, and **the check was
mutation-verified** — inserting one `{` after `3 | 4 => match ctx.get_field(this, 2) {`
makes it report `unclosed delimiter` and name `session_has_negotiated` as the
opener. Parsing is not type-checking; see §7.

---

> **VERIFIED AGAINST A BINARY 2026-09-04. §"How to verify" step 1 was run as
> written, and the mutation matrix reproduces exactly.** The status block says
> *"No `cargo`, no VM, no fixture"*, with only `rustfmt` parse-checking — and
> §7's own caution that *"parsing is not type-checking"*.
>
> The record does not ask for a green; it specifies a 2-by-3 experiment, because
> two tests passing says nothing about whether they are complementary. Run on a
> binary built from this tree:
>
> ```text
>                                       widened_null   null_socket_session   negotiated_keeps
>                                       _not_negotiated  _no_id_not_valid     _id_and_validity
> baseline                                  ok               ok                    ok
> mutation A:  3 | 4  ->  3                FAILED           FAILED                 ok
> mutation B:  session_has_negotiated
>              returns false                ok               ok                  FAILED
> ```
>
> That is precisely what §"How to verify" predicts: A turns the two null-session
> tests red *"while `a_session_that_negotiated_keeps_its_id_and_its_validity`
> stays green"*, and B turns only that third one red. Both mutations were
> reverted and `t27_tls.rs` left byte-identical to `HEAD`.
>
> **The value is in the off-diagonal.** Each mutation is caught by exactly the
> tests that should catch it and ignored by the one that should not, so neither
> test is vacuous and neither is merely a copy of the other. A pair that both
> went red on both mutations would have been consistent with a single
> over-broad assertion; this rules that out.
>
> **What this does NOT verify.** The `cargo test` bar is step 1, the cheapest of
> the list; the later steps involve a real HTTPS handshake and
> `scratchpad/f6/F6Invalidate.java`, which did not survive its session and cannot
> be re-run. §4's HotSpot column is cited from E12-1/E42-1 rather than
> re-derived, as this record itself says, and this note does not re-derive it
> either. The two minters this record *"deliberately keeps wrong"* are still
> wrong on purpose — nothing here changes or checks them, and a green above is
> not evidence about them.

## 0. Verdict

1. **The tree was already in the hazardous state when this lane opened it.**
   `NEW13_SSL_SESS_FIELDS` is `4` at HEAD 892b2ccb5 and
   `session_has_negotiated` still read `3 =>`, so width 4 was falling into
   `_ => true`. This was not a landing-order risk to be avoided — it was a live
   defect to be closed, and `cargo test -p cratonvm-native-builtins` is
   PREDICTED RED at HEAD on
   `ssl_security::new13_tests::the_widened_null_session_is_still_not_negotiated`.
   §1.
2. **The production change is ONE LINE**: `3 =>` becomes `3 | 4 =>`. Everything
   else in this commit is comments, doc tables and tests. §1.
3. **The merge is a provable no-op for the only other width-4 minter**, and I
   re-derived that rather than inheriting it: `SSLServerSocket.accept` writes
   `crate::servlet::RUSTLS_SOCK_ID_BASE + stream_id` into slot 2
   (`t27_tls.rs:5100`, `:5166`), and `RUSTLS_SOCK_ID_BASE` is
   `0x4000_0000` (`servlet.rs:2415`), so slot 2 is `>= 0` for every session it
   has ever minted — which is precisely what `_ => true` answered. §2.
4. **All eleven width-indexing readers were enumerated before the edit, and
   width 3 -> 4 shifts none of them.** The one that had to move is the one that
   moved. §2.
5. **MEASURED, and it changes a comment this lane wrote from an argument into a
   fact:** `invalidate()` on a session that negotiated nothing is a **no-op on
   HotSpot too**, on the socket door and the engine door alike. So `tls.rs`'s
   width-gated no-op is not "a quiet miss we tolerate" for the shape this
   change governs — it is the oracle's own behaviour. §4.
6. **MEASURED: `putValue` round-trips on HotSpot's null session and disturbs
   nothing** — `isValid` stays `false`, `getId().length` stays `0`,
   `getValueNames()` goes `[]` -> `[cratonvm.f6]`. That is the exact contract
   the widening plus this arm merge produce together, and it is an oracle for
   E42-1's NOMINATION 6 block that did not exist before. §4.
7. **The merge deliberately PRESERVES a known-wrong answer for HTTPS, and that
   is the most important thing in this record.** `http_url_connection.rs` and
   `net_phase_e.rs` mint the same width-4 shape with `NEW13_SESS_TLSID = -1`
   for connections whose handshake genuinely *completed*. Before this commit
   the broken `_ => true` arm was making those two accidentally RIGHT. Landing
   the correct arm makes them wrong again — visibly, and for a stated reason.
   NOMINATION 1. §5.
8. **E42-1's NOMINATION 3 over-counted by one file.** It names
   `http_url_connection.rs` *and* `net_phase_e.rs`; `net_phase_e.rs` has no
   "3-field" comment (`grep -n "3-field"` returns nothing on it). Only
   `http_url_connection.rs:2183` and `tls.rs`'s three are real, and the three
   in `tls.rs` are fixed here. §6, NOMINATION 2.

## 1. The change

`native-builtins/src/t27_tls.rs`, `session_has_negotiated`:

```rust
-        3 => match ctx.get_field(this, 2) {
+        3 | 4 => match ctx.get_field(this, 2) {
```

That is the entire production diff of this commit, across both files.
`git diff -U0 | grep -v '^[+-]\s*//'` returns this line and nothing else that
is not a test.

Width 3 is **kept in the arm, not replaced by 4**. It is retired as a minted
shape but three of this file's own tests construct it, and — the load-bearing
reason — the arm's job is to refuse, so its lower bound must stay wide. A
future minter that reaches for a narrow shape must land on "not negotiated",
never on `_ => true`. The comment on the arm says so.

The `_ => true` arm below now serves widths 5 and 6 only. Nothing mints 5.

## 2. The enumeration — every reader that indexes an `SSLSession` width

The brief's first constraint, and the one that decides whether this commit is
safe. Done by `grep -n object_num_fields` over `t27_tls.rs`, `tls.rs`,
`ssl_security.rs`, `http_url_connection.rs`, `net_phase_e.rs`, `http2.rs`, then
by reading each hit's receiver class. **A hit is only a hazard if its answer
differs between width 3 and width 4.**

| reader | file | rule | at 3 | at 4 | shifts? |
|---|---|---|---|---|---|
| `session_has_negotiated` | `t27_tls.rs` | the arms | slot 2 `>= 0` | **`_ => true`** | **YES — this commit** |
| `session_cipher_slot` | `t27_tls.rs` | `>= 6 ? 0 : 1` | 1 | 1 | no |
| `session_proto_slot` | `t27_tls.rs` | `>= 6 ? 1 : 0` | 0 | 0 | no |
| `sslsess_attrs_slot` | `t27_tls.rs` | `4 => Some(3)` | `None` | `Some(3)` | intended — the point of the widening |
| `getCreationTime` | `t27_tls.rs` | `> 5` | now | now | no |
| `getLastAccessedTime` | `t27_tls.rs` | `> 5` | now | now | no |
| `SSLSession.isValid` | `tls.rs` | `> SES_CREATION_TIME` (5) | predicate only | predicate only | no |
| `SSLSession.invalidate` | `tls.rs` | `> SES_CREATION_TIME` | no-op | no-op | no |
| `SSLSessionImpl.isValid` | `tls.rs` | `> SES_CREATION_TIME` | predicate only | predicate only | no |
| `getPeerHost`/`getPeerPort`/`getCreationTime` | `tls.rs` | `<= SES_CREATION_TIME` | fallback | fallback | no |
| `getPeerPrincipal` &co. (six sites) | `ssl_security.rs` | `> NEW13_SESS_TLSID` (2) | true | true | no |

**Exactly one reader distinguishes 3 from 4, and it is the predicate.** That is
why the widening is additive and why the co-requisite is a single arm.

**The other direction, which the table above does not cover and which the
widening genuinely creates:** slot 3 did not exist on this shape before, so a
reader that indexes slot 3 on a session would go from "past the end" to "reads
the attribute map". Checked:
`grep -rn "get_field(this, 3)\|get_field(session, 3)\|get_field(sess, 3)"` over
the six files returns five hits, and **not one has an `SSLSession` receiver** —
they are `SSLEngine.getEnabledProtocols` and `SSLEngineResult.bytesProduced`
(`ssl_security.rs`), `x509_mirror_der` (an X.509 mirror), and two in
`net_phase_e.rs` on URL/file shapes. The sixth is this lane's own new
assertion. **No slot-3 hazard exists.** Stated with the denominator because "I
found nothing" is otherwise unusable.

### Why 4 and not 9, re-derived rather than inherited

E42-1 argues this; the brief forbids re-deriving *measurements*, not
*safety arguments about my own file*, and this one is about my arms. Width 9
lands on `n if n >= 7`, needing no change here — but `session_cipher_slot` and
`session_proto_slot` split at `>= 6`, so at width 9 the cipher slot becomes 0
and the protocol slot becomes 1, i.e. **swapped**, and the stream id would have
to move to 6. Three slots shift under every reader in the table above. Width 4
shifts none. The argument holds independently.

## 3. NOMINATION 2 — `sslsess_attrs_slot`'s width table

No code change; `4 => Some(3)` was already correct. The doc table listed 2- and
3-field rows that no longer exist and closed with a sentence pointing at a
nomination that has now landed — "the 3-field one is `ssl_security.rs`'s and
cannot be widened without moving `session_has_negotiated`'s arms in the same
commit". Leaving that sends the next reader hunting for an open nomination that
is closed. Rewritten: the retired rows collapse into a note, the closing
paragraph points here, and the `_ => None` arm is documented as serving the
6-field shape, **now the only shape with no attribute slot**.

Added to that doc, because it is the part a "simplification" would delete: at
width 4, `num_fields - 1` and `sslsess_attrs_slot` **agree**. A reader who
reverts the helper to the bare arithmetic passes every width-4 test. The guard
against that is the 6-field row (where `num_fields - 1` is
`tls.rs::SES_CREATION_TIME`), and the new test says so in its own assertion
message rather than leaving it to be rediscovered.

## 4. MEASURED — `scratchpad/f6/F6Invalidate.java`, three byte-identical runs

HotSpot 25.0.3+9-LTS, `Microsoft-13877124`. No network: the socket is from the
zero-arg `createSocket()` and the engine from `createSSLEngine()`.

```
SOCKET before-invalidate isValid=false idLen=0 cipher=SSL_NULL_WITH_NULL_NULL proto=NONE names=[]
SOCKET after-invalidate  isValid=false idLen=0 cipher=SSL_NULL_WITH_NULL_NULL proto=NONE names=[]
SOCKET putValue.roundTrip=v
SOCKET after-putValue    isValid=false idLen=0 cipher=SSL_NULL_WITH_NULL_NULL proto=NONE names=[cratonvm.f6]
SOCKET removeValue=null
ENGINE before-invalidate isValid=false idLen=0 cipher=SSL_NULL_WITH_NULL_NULL proto=NONE names=[]
ENGINE after-invalidate  isValid=false idLen=0 cipher=SSL_NULL_WITH_NULL_NULL proto=NONE names=[]
```

Three things this settles that were previously arguments:

1. **`invalidate()` on a never-negotiated session is a no-op on the oracle.**
   `tls.rs`'s `invalidate` is gated `> SES_CREATION_TIME` and therefore no-ops
   at width 4. That gate was justified in-tree as "a quiet miss beats a loud
   corruption" — a trade. For this shape it is not a trade: it is HotSpot's
   answer. The comment now says that and carries the transcript. The residual
   is only a session that DID negotiate on a width-4 shape, and — per E12-1 §1
   arm E — `invalidate()` touches `isValid()` alone there, so the miss cannot
   spread to the other twelve accessors.
2. **`putValue` round-trips AND disturbs nothing.** `isValid` stays `false`,
   `getId().length` stays `0`. This is the *entire* E42 bargain measured on the
   oracle in one line, and CratonVM now matches it only because the widening
   and this arm merge are both present: the widening gives the write a slot,
   the arm merge keeps slot 2 authoritative.
3. **`getValueNames()` goes `[]` -> `[cratonvm.f6]`.** E42-1's NOMINATION 6
   warns that the fixture's `getValueNames` check asserts `array[0]` and is
   only true *before* a `putValue`. Confirmed — whoever lands that block must
   reorder, not just append.

## 5. What this commit does NOT fix, and keeps visibly wrong

`http_url_connection.rs::huc_verify_hostname` and
`net_phase_e::https_session_object` both allocate `NEW13_SSL_SESS_FIELDS` and
both write `NEW13_SESS_TLSID = Value::Int(-1)`, with the same comment: *"this
connection owns its rustls state inside `perform` and is never registered in
the `servlet` TLS id space, so there is no id to record."*

Their handshake **completed**. So after this commit:

| | HotSpot | CratonVM (PRED) |
|---|---|---|
| HTTPS session `isValid()` | `true` | **`false`** |
| HTTPS session `getId().length` | 32 | **0** |

**This is not a regression introduced here, and it is not left unfixed by
oversight.** Before this commit, width 4 fell into `_ => true`, which made
those two answer `true`/32 — *accidentally right*, for a reason that had
nothing to do with them and that simultaneously made the null session valid.
E42-1 §2 flagged exactly this and asked that the arm be landed "for the stated
reason rather than notice the accident". Landing it restores their old wrong
answer.

The right fix is theirs, not the predicate's: record a real marker in slot 2
instead of `-1`. NOMINATION 1. I am not making `session_has_negotiated` lenient
to paper over it — a predicate that answers "negotiated" for a `-1` id would
undo E12/E22 for every door at once, which is the whole defect this family
exists to remove.

## 6. NOMINATION 3, the half in this lane's files

`tls.rs` carried three comments reasoning about "the 3-field
`new13_alloc_null_ssl_session` shape". In each the *reasoning* survives — slot
2 on that shape is still the stream id — and only the number was wrong. Fixed
by naming the constant or the shape rather than a width, since the width has
now moved twice.

One of them gained a sentence worth more than the number: `SSLSessionImpl
.isValid`'s comment explained that a width test mis-reads the null session one
way and the accept session the other. **Those two shapes are now the same
width**, so no width test can ever separate them — only the value in slot 2
can. That is the clearest available statement of why this predicate must read a
field, and it was one edit away from being lost.

Two more stale widths found in `t27_tls.rs` while enumerating, both fixed:

* `register_ssl_session_real`'s doc names *"the 7-field session from
  `SSLEngineImpl.getSession()`"* and *"the 3-field session from
  `SSLServerSocket.accept()`"*. Both wrong, and wrong **before** E42 — they are
  8 and 4. `session_cipher_slot`'s doc already records catching this exact
  off-by-one ("it named a 7-field and a 3-field shape that are actually 8 and
  4"); this instance is the same pair, in the same file, missed by that sweep.
  The pattern is worth naming: **a doc fix that corrects one spelling of a
  number does not find the others — only a grep for the number does.**
* `getCreationTime`'s residual note pointed at "E22-1's NOMINATION to widen
  `NEW13_SSL_SESS_FIELDS`" as the way to get a stable timestamp. That widening
  has landed and **did not** give it one; slot 3 is the attribute map. Reaching
  a timestamp slot means width >= 6, which is the three-slot shift §2 rejects.
  Corrected rather than deleted, because the two are easy to conflate.

## 7. The tests, and what each one would catch

`ssl_security::new13_tests` (not this lane's file) holds the co-requisite from
the other side. This lane added the same line from inside its own crate,
because a test in a file another lane owns is a dependency, not a guard.

| test | width | fails when |
|---|---|---|
| `the_null_socket_session_has_no_id_and_is_not_valid` | **3 and 4** | the arm regresses to `3 =>` (width-4 iteration goes red) |
| `a_session_that_negotiated_keeps_its_id_and_its_validity` | 3, **4**, **4-accept**, 8 | the predicate is made unconditionally `false` |
| `a_widened_null_session_put_value_does_not_touch_the_stream_id` | **4** | `sslsess_attrs_slot`'s `4 => Some(3)` regresses, or the write lands on slot 2 |
| `a_put_value_cannot_resurrect_the_null_sessions_id_or_validity` | 3 | the `None` arm regresses |
| `a_shape_with_a_real_attribute_slot_still_round_trips` | 8 | `putValue` no-ops for everything |

The first two are a **mutation pair** and were built as one: revert the arm and
the first goes red while the second stays green; make the predicate `return
false` and the second goes red while the first stays green. If a mutation ever
leaves both green, the pair has collapsed onto one branch and is measuring
nothing — the failure mode this directory records repeatedly.

The width-4 row of the second test is deliberately **two** rows, `Int(0)` and
`RUSTLS_SOCK_ID_BASE + 3`. `>= 0` is the boundary and `0` sits on it; the
historical bugs in this family (the `invalidate()` collision) produced exactly
`Int(0)`. One row would not have distinguished "accept sessions still work"
from "the boundary is right".

## 8. Residuals

1. **Nothing was built, type-checked or run.** `rustfmt` proves the files
   *parse*; it does not prove they compile. The type risks in the new test are
   `crate::servlet::RUSTLS_SOCK_ID_BASE` (`pub(crate) const … : i32`, and slot
   2 takes `Value::Int(i32)` — checked at `servlet.rs:2415` against
   `t27_tls.rs:5100`'s existing `RUSTLS_SOCK_ID_BASE + stream_id`) and the
   `r.find(...)` bindings hoisted above a loop, which is byte-for-byte the
   pattern the neighbouring `a_session_that_negotiated_keeps_its_id_and_its_validity`
   already compiles with.
2. **`MockNativeContext` is the thing being trusted for the new test.** It
   round-trips a `putValue` into slot 7 for the 8-field shape in an existing
   green test, so `new_object_initialized("java/util/HashMap")` returns an
   object under the mock; the width-4 test makes the identical claim at slot 3.
   This directory's warning that a mock's name-to-slot fallback can measure the
   mock does not bite here — every field is addressed by index, never by name.
3. **CRLF preserved, zero bare LF introduced.** `t27_tls.rs` 13,002 CRLF /
   13,002 LF; `tls.rs` 5,400 / 5,400. `LF - CRLF == 0` on both after every
   hunk. All edits went through the editor. Counts must be taken from the file
   on disk — `git show HEAD:` reports 0 CRLF for the same files because git
   stores LF.
4. **This worktree is SHARED and a dozen sibling-lane files are dirty in it** —
   `bigint.rs`, `jdk_baseline.rs`, `lang_math.rs`, `lang_string.rs`, `lib.rs`,
   `math_bignum.rs`, `phases_late.rs`, `phases_late/nio_file.rs`,
   `regression-suite/run.sh`, `scripts/jdk-baseline/classes.txt` and two new
   `.md`/probe files, none of them this lane's. Anyone committing this work
   must stage `native-builtins/src/t27_tls.rs`, `native-builtins/src/tls.rs`
   and this record **by path**, never `-a` and never `git commit -am`. The
   worktree was reported clean when this lane opened it, so that list grew
   during the lane.
5. **The two native-TLS acceptor `"UNKNOWN"` arms were NOT touched**, and the
   judgement behind them is correct: they are reached *by succeeding*, so
   `SSL_NULL_WITH_NULL_NULL` there would assert "no cipher negotiated" about a
   live encrypted connection — false in the dangerous direction. The sentinel
   is safe elsewhere precisely because it is unofferable, which
   `t27_tls::tests::the_sentinel_is_never_offerable_and_the_fabrication_always_was`
   pins against `SUPPORTED_CIPHER_SUITE_NAMES`. `NATIVE_TLS_UNNAMEABLE_SUITE`
   is already a named constant with that reasoning on it.
6. **`isValid` and `getId` were NOT collapsed into one predicate**, in either
   file. `tls.rs` composes negotiated AND not-invalidated for `isValid` and
   gates `getId` on negotiated alone, mirroring `isRejoinable()` reading the id
   and `invalidated` while `getId()` reads neither. Recorded because this
   commit touches every comment around that split and a later reader may see
   the duplication as something to tidy.

## 9. Which of `RSslNullSession`'s 47 checks this lane decides

The honest form, since "N checks flip" is the number that gets misquoted.
`nullSession(door, s)` is 13 checks (1 non-null + 12 `ck`), run for three
doors, plus 8 standalone = 47.

**Real-JDK mode** — the mode `--jdk-only` runs. DOOR 1 (`socket`) and its
`socketClosed` repeat resolve to `ssl_security::new13_resolve_socket_session`'s
width-4 null session (`close()` re-writes `NEW13_SESS_TLSID = -1`,
`ssl_security.rs:4364`). DOOR 2 (`engine`) does **not**: `net_phase_e
::register_re6_ssl_context` re-registers `createSSLEngine` to allocate
`sun/security/ssl/SSLEngineImpl`, so it lands on the 8-field shape and the
`n >= 7` arm, which this commit does not touch.

| check, per door | reads the predicate | flips without this arm |
|---|---|---|
| `.isValid` | yes | **YES** — `false` -> `true` |
| `.getId.length` | yes | **YES** — `0` -> `32` |
| `.getId.isNull` | yes | no — non-null array either way |
| `.getId.stable` | yes | no — stable either way |

**So: 8 of the 47 checks read `session_has_negotiated` on the widened shape
(4 per door × the `socket` and `socketClosed` doors), and 4 of those 8 are
DECIDED by this one line — they report the truth with it and a fabrication
without it.** The other 4 are on its path and answer the same either way; they
are listed so the 8 is not quietly inflated into 8 flips.

Under `--synthetic-jdk` the engine door joins them (E42 retired the 2-field
engine fallback onto `new13_alloc_null_ssl_session`) and `tls.rs`'s shadowing
registrations answer instead — but those call the same predicate, so the count
becomes **12 read, 6 decided**.

Today **1 of 47 executes** (E31-1 §6); all 47 run once E31's
`SSLSocket.getHandshakeSession` registration lands. The one check running today
is `socket.isConnected`, which is E42's line, not this one. **Every check this
lane decides is currently invisible** — which is exactly why the co-requisite
was mechanised as a unit test instead of a sentence, and why this lane added
its own rather than depending on another file's.

---

## NOMINATION 1 — `http_url_connection.rs` / `net_phase_e.rs`: a completed handshake reports `isValid() == false`

**The visible cost of landing E42-1's NOMINATION 1 correctly.** Both files mint
the width-4 session with `NEW13_SESS_TLSID = Value::Int(-1)` for a connection
whose handshake succeeded (`http_url_connection.rs:2215`,
`net_phase_e.rs:7935`). `session_has_negotiated` now reliably answers `false`
for them, so an HTTPS session that really did negotiate reports
`isValid() == false` and `getId() == byte[0]` where HotSpot reports `true` and
32 bytes.

Do **not** fix this by loosening the predicate. Slot 2 is documented as a
stream id and `-1` is documented as "never connected"; a predicate that called
`-1` negotiated would re-validate the null session at every door.

Fix it at the minters, which is where the information is. Both comments already
concede the shape of the answer — *"this connection owns its rustls state
inside `perform` and is never registered in the `servlet` TLS id space"*. Two
options, in preference order:

1. **Register the connection in the `servlet` id space** and write the real
   offset id, as `SSLServerSocket.accept` does. Then `getId()` and `isValid()`
   are right for the same reason as every other door, with no new convention.
2. If (1) is too invasive, write **any** `>= 0` id these two agree on, and say
   in the comment that the value is a presence marker rather than a lookup key.
   That is strictly worse — it makes slot 2 mean two things — so take it only
   with a measurement showing (1) is not viable.

The consumer is Tomcat's `JSSESupport.getSessionId`, which tests
`ssl_session.length == 0` exactly. Today it sees an HTTPS request as untrackable.

## NOMINATION 2 — `http_url_connection.rs:2183`: the last "3-field" comment

The fourth of E42-1's NOMINATION 3, in a file this lane does not own. Its
allocation already uses `NEW13_SSL_SESS_FIELDS` and widens correctly; only the
prose is stale. E42-1 gives the exact replacement text.

**Correction to that nomination while it is being actioned:** it names
`native-builtins/src/net_phase_e.rs` as carrying one too. It does not —
`grep -n "3-field" native-builtins/src/net_phase_e.rs` returns nothing. There
are four such comments in the tree, not five: three in `tls.rs` (fixed here)
and this one.

## NOMINATION 3 — `regression-suite/src/RSslNullSession.java`: the oracle for the attribute block now exists

E42-1's NOMINATION 6 proposes a `putValue` round-trip block and marks it
PREDICTED. §4 above **measures** it, so it can land as MEAS rather than PRED:

```
putValue.roundTrip = v
after putValue     isValid=false idLen=0 names=[cratonvm.f6]
removeValue        = null
```

Both doors agree, three byte-identical runs. Two cautions that the measurement
also confirms, and which that nomination flags:

* the existing `getValueNames` check asserts `array[0]` and must run
  **before** the `putValue`, not after — measured, the array becomes
  `[cratonvm.f6]`;
* the block moves the count from 47 to 65, which `harness-guard.sh` reads.

Also still open from E42-1's NOMINATION 6: `socket.isBound`,
`socket.isInputShutdown`, `socket.isOutputShutdown` are unasserted.

## NOMINATION 4 — `docs/known-issues/jdk-only/INDEX.md`: five records, one investigation

Re-raising E42-1's NOMINATION 5, which re-raised E31-1's NOMINATION 7, still
open. `INDEX.md` lists none of `E12-1`, `E22-1`, `E31-1`, `E42-1` or this
record, and they are one continuous chain across five lanes in which each
record's nomination is the next one's task. A reader who finds any one of them
finds no path to the other four. `INDEX.md` is not this lane's file.

```
- E12-1-the-null-session-and-the-fabricated-cipher.md
- E22-1-the-null-session-in-the-registrar-that-actually-answers.md
- E31-1-the-unregistered-door-and-the-slot-that-resurrects-a-fabrication.md
- E42-1-the-slot-that-was-never-there-and-the-predicate-that-was-its-own-negation.md
- F6-1-the-arm-that-had-to-move-and-the-two-minters-it-keeps-wrong.md
```

## How to verify

Cheapest first. Every CratonVM row is PREDICTED.

1. **`cargo test -p cratonvm-native-builtins`** — and run the mutation pair the
   mutation way, in both files. Revert `3 | 4` to `3` and
   `ssl_security::new13_tests::the_widened_null_session_is_still_not_negotiated`
   **and** `t27_tls::tests::the_null_socket_session_has_no_id_and_is_not_valid`
   must both go red while
   `a_session_that_negotiated_keeps_its_id_and_its_validity` stays green. Then
   make `session_has_negotiated` `return false` and that one must go red while
   the first two stay green. Both green under either mutation means the pair
   has collapsed.
2. **`cargo test -p cratonvm-native-builtins --features synthetic-jdk`** —
   `tls.rs`'s shadowing `isValid`/`invalidate`/`getId` are live only there.
3. **`--dump-native-registry`** (flags BEFORE `-cp`) — confirm `t27_tls`'s
   `javax/net/ssl/SSLSession.isValid`/`getId` still own their slots in
   real-JDK mode. The shadowing comments in both files are arguments from
   registration order; the dump is the authority.
4. **`bash regression-suite/run.sh` with `ONLY="RSslNullSession"`**, after
   E31's `getHandshakeSession` registration. §9 names the 8 checks to watch and
   the 4 that decide.
5. **Any embedded-HTTPS fixture.** §5's divergence is the one to look for:
   a completed HTTPS handshake reporting `isValid() == false`. It is expected,
   it is NOMINATION 1, and it must not be diagnosed as this commit breaking
   HTTPS.
