# E13-1 — the six builders that decode a reference as an `Int`: all six fixed, four more sites the idiom did not mark, and the one row that still cannot go green

**2026-08-13, lane E13.** Applies `E2-1-optional-reference-layout-landed.md`'s
**N2** to `native-builtins/src/http2.rs`, which this lane owns. Answers its
**N3** with a finding: **N3 was already landed and following it literally would
have broken the fixture.** The patch is applied and verified in the working
tree.

**This lane may not build or run the VM, and did not.** Every JDK fact below is
from `javap`/`java` on this host (25.0.3+9-LTS) and is quoted. Every claim about
CratonVM's behaviour — before and after — is **PREDICTED** from source. **The
twelve Rust tests this lane added have never been executed.** The file was
parse-checked (`rustfmt --emit stdout` on a scratch copy, exit 0), which rules
out syntax errors and nothing else; it does not type-check. `RJdkOptionalShape`
WAS compiled and run on HotSpot 25 and is green (§5).

---

## 0. Verdict

| claim | verdict |
|---|---|
| E2-1 §4's six builder sites and their line numbers | **CONFIRMED, all six, exactly** (§1) |
| `expectContinue(Z)` and `firstValueAsLong` are correct controls | **CONFIRMED** — both untouched, both now pinned by a named test (§2, §4) |
| the family is exactly those six | **NO — it is ten.** Four more sites discard a reference argument without the `Value::Int` tell (§3) |
| E2-1 N3: "add four `laws(...)` for executor/cookieHandler/proxy/authenticator" | **ALREADY LANDED.** Adding them would have duplicated four rows and broken an exact denominator (§5) |
| `mint-connectTimeout-present-millis` / `-timeout-present-millis` / `-bodyPublisher-present-len` go green | **PREDICTED YES** (§6) |
| `mint-version-present-name` goes green | **NO — still red, for a THIRD reason upstream of both the builder and the accessor** (§6, §7) |

## 1. The six, re-verified against the descriptor

E2-1 §4's table was checked line by line against the pre-patch working tree.
All six line numbers were exact, and `grep -n 'Some(Value::Int(n))'` returned
those six plus `expectContinue` and nothing else.

| pre | post | site | descriptor parameter | what arrived | fallback that always won |
|---|---|---|---|---|---|
| 1468 | 1644 | `HttpClient$Builder.version` | `Ljava/net/http/HttpClient$Version;` | enum ref | `HTTP_VERSION_2` |
| 1485 | 1661 | `HttpClient$Builder.connectTimeout` | `Ljava/time/Duration;` | `Duration` ref | `0` |
| 1501 | 1677 | `HttpClient$Builder.followRedirects` | `Ljava/net/http/HttpClient$Redirect;` | enum ref | `REDIRECT_NEVER` |
| 1983 | 2202 | `HttpRequest$Builder.timeout` | `Ljava/time/Duration;` | `Duration` ref | `0` |
| 2015 | 2241 | `HttpRequest$Builder.version` | `Ljava/net/http/HttpClient$Version;` | enum ref | `0` (no override) |
| 2375 | 2606 | `BodyPublishers.ofByteArray` | `[B` (STATIC, so `args[0]`) | array ref | `0` |

The decode now goes through three helpers (`http2.rs:1177`, `:1227`, `:1270`).
The facts they rest on, all quoted from this host:

```
$ javap -p java.time.Duration          ->  private final long seconds;   (slot 0)
                                           private final int  nanos;     (slot 1)
$ java -e  Version.values()            ->  HTTP_1_1=0  HTTP_2=1
$ java -e  Redirect.values()           ->  NEVER=0  ALWAYS=1  NORMAL=2
```

The JDK's ordinals agree with this file's constants, so the ordinal fallback is
safe here — that is a checked fact, not an assumption, and `test_redirect_constants`
records that they were 0/2/1 once and disagreed.

**Two decisions inside the helpers are load-bearing:**

1. **Both are located by the CLASS-SIDE WITNESS**
   (`resolve_field_index_by_class_id(cid, "seconds")`, `unwrap_or(slot)`), not
   by a `get_field_by_name` VALUE read. `test_utils::MockNativeContext::get_field_by_name`
   answers `Value::Int(0)` for an unresolvable name where production answers
   `Value::Object(None)` — its own doc comment names two bugs already paid for
   by that divergence. A value-side fallback would have been steered by the
   mock. `resolve_field_index_by_class_id` is the read the mock answers
   faithfully, so the tests in §4 measure the VM.
2. **`enum_arg_ordinal` returns `Option`, and an unrecognised ordinal is
   `None`.** A decoder that decays to ordinal 0 is how `phases_early::tu_ordinal`
   turned `SECONDS.toNanos(1)` into `1`. Each caller keeps its own documented
   default; `e13_an_undecodable_enum_argument_keeps_each_sites_own_default`
   pins all three.

Name is read before ordinal. `p57_alloc_enum` writes the name into slot 0 and
`java.lang.Enum` declares `name` before `ordinal`, so the two encodings
reconcile at consumption. Neither helper calls `invoke_virtual` — unlike
`phases_early::enum_value_ordinal` — because no native in this file re-enters
the interpreter and an argument decoder is the wrong place to start.

## 2. THE CONTROLS — not touched, and now failing by name if swept up

`expectContinue(Z)` (`:2214`, decode at `:2222`) is the **only** live
`Some(Value::Int(n))` match arm left in the file. Its descriptor really is a
primitive `boolean`, so the idiom is CORRECT. `firstValueAsLong`'s 2-slot
`OptionalLong` is likewise untouched. Both now carry a comment saying DO NOT
sweep this up, and both have a test that fails by name:
`e13_expect_continue_still_reads_a_primitive_boolean` and E2's
`e2_optional_long_keeps_the_primitive_flag_payload_layout`.

This is the same trap the `Character` family produced this session (`charCount`
needs a signed compare while `isBmpCodePoint`/`isValidCodePoint` are genuinely
unsigned): one file, one idiom, and the descriptor is the only thing that says
which member is which.

## 3. THE FAMILY IS TEN, NOT SIX — what the idiom did not mark

E2-1 defined the family by an IDIOM (`Some(Value::Int(n))` against a reference).
Grepping the SHAPE instead — *a builder that does not read the reference it was
handed* — finds four more, and one of them is the reason a fixture row is red:

| post | site | what it discarded |
|---|---|---|
| 2097 | `HttpRequest$Builder.POST(BodyPublisher)` | set `REQ_HAS_BODY=1`, never `REQ_BODY_LEN`, so `bodyPublisher().get().contentLength()` was 0 for EVERY body |
| 2118 | `HttpRequest$Builder.PUT(BodyPublisher)` | same |
| 2046 | `HttpRequest$Builder.method(String, BodyPublisher)` | same |
| 1968 | `HttpRequest.version()` — the ACCESSOR | see below |

The first three are fixed by `body_publisher_arg_len` (slot 0, a `Long`; `-1`
"unknown length" is carried through unchanged, which is the streaming
publishers' contract). E2-1 §4 correctly blamed `POST` for
`mint-bodyPublisher-present-len` but filed it outside the six; it is the same
defect with a quieter tell.

**`HttpRequest.version()` (`:1968`) had to move with the builder, not after
it.** E2-1 left it unconditionally EMPTY and said so in a comment whose stated
premise was "`REQ_VERSION` can never be non-zero". This patch makes it non-zero,
so that comment became FALSE the moment `:2241` was fixed, and the accessor
would have answered "no version override" for a request that has one — a NEW
wrong answer introduced by this patch. It is the two-line change E2-1
pre-specified, **with the `pin_native_root` / `read_native_pin` /
`unpin_native_roots` E2-1 §6 demanded** around `version_enum`'s allocation.
E2's `e2_request_version_is_empty_never_an_ordinal` asserted the old behaviour
and would have gone red; it is amended in place to
`e2_request_version_is_never_an_ordinal`, keeping the law it was actually
written to state (never an ordinal, never a flag) and recording why the `Int(2)`
row moved.

### 3a. A SLOT COLLISION this patch would otherwise have ACTIVATED

`followRedirects` also wrote `Int(policy != NEVER)` into **BUILDER slot 7**.
`build()`'s copy table maps builder slot 7 to **`CLIENT_HAS_COOKIE`**, not to
`CLIENT_FOLLOW_REDIR` — which is slot **8**, and the copy loop is `0..8`, so
`build()` never copied it at all. `CLIENT_FOLLOW_REDIR` has **no reader
anywhere in the file**: two hits, the declaration and one initialiser write.

That stray write was inert only because `policy` could never be anything but
`NEVER`, so it always wrote the same `0` the initialiser had just written. The
instant the decode works, `followRedirects(ALWAYS)` starts stamping a redirect
answer into the cookie-handler flag. The write is removed and `build()` derives
`CLIENT_FOLLOW_REDIR` from the copied policy instead;
`e13_follow_redirects_decodes_the_enum_and_leaves_the_cookie_flag_alone` sets a
cookie handler first and asserts it survives.

**This is the general shape worth carrying forward: a dead decode can be hiding
a second bug downstream, and fixing the decode is what detonates it.** Six sites
were nominated; a blind application of the six would have shipped a regression.

## 4. VERIFICATION SCOPE — stated honestly

### 4a. Twelve new Rust tests, none of them ever executed

`http2.rs` had 64 tests before E2 and **every one was registration-only**
(`find(...).is_some()`), so all 64 stayed green through both defect families.
E2 added nine behavioural ones; this lane adds twelve more in the same shape —
invoke through the registry, assert the slot actually written.

| test (`http2.rs`) | covers |
|---|---|
| `e13_client_builder_version_stores_the_constant_the_caller_passed` (`:4245`) | site 1, **both** constants — `HTTP_1_1` is the discriminating one, `HTTP_2` only ever agreed with the old default |
| `e13_enum_decode_reads_the_name_first_and_the_ordinal_as_a_fallback` (`:4274`) | the two encodings, **separated**: a name-only constant whose ordinal slot reads `0` must still answer `HTTP_2` |
| `e13_client_builder_connect_timeout_decodes_a_duration` (`:4302`) | site 2, incl. the floor-normalised negative (`ofMillis(-1500)` is `seconds=-2, nanos=+5e8`) |
| `e13_request_builder_timeout_decodes_a_duration` (`:4338`) | site 4 |
| `e13_follow_redirects_decodes_the_enum_and_leaves_the_cookie_flag_alone` (`:4360`) | site 3 **and §3a** |
| `e13_request_builder_version_stores_the_ordinal_plus_one` (`:4420`) | site 5 |
| `e13_an_undecodable_enum_argument_keeps_each_sites_own_default` (`:4441`) | the no-silent-decay rule, all three defaults |
| `e13_of_byte_array_reads_the_array_length` (`:4477`) | site 6 |
| `e13_post_put_and_method_carry_the_publishers_content_length` (`:4502`) | §3's first three |
| `e13_expect_continue_still_reads_a_primitive_boolean` (`:4560`) | **NEGATIVE CONTROL** |
| `e13_request_version_optional_now_holds_the_enum_mirror` (`:4586`) | §3's accessor, end to end through `build()` |
| `e13_the_duration_optionals_are_reachable_through_the_builder_now` (`:4631`) | E2's H1/H6 present arms, driven the way JAVA reaches them rather than by writing the backing field |

The last two matter beyond their count: E2's tests set the backing field
directly because the present arms were unreachable through Java. These drive
builder → `build()` → accessor, which is the path the fixture executes, so they
are the first thing in the repository that would notice the builder and the
accessor disagreeing.

**None has been run.** This lane may not invoke cargo. Their first run is the
gate, and `e2_request_version_is_never_an_ordinal` (amended) is the one most
likely to surprise.

### 4b. Fixture coverage of `http2.rs` is still ZERO — do not read the row count as coverage

`RJdkOptionalShape` now runs **268** checks in `httpmint` (was 266) and **1418**
overall (was 1416). **That number is not coverage of anything in this file.**

`register_http2_natives` has one production caller
(`native-builtins/src/lib.rs:24190`), inside
`#[cfg(feature = "synthetic-jdk")] register_synthetic_overrides`, gated at
runtime by `config.use_synthetic_jdk`. `RJdkOptionalShape` sits in `CORE_CLASSES`
(`regression-suite/run.sh:164`), which runs DEFAULT mode, where
`net_phase_e.rs`'s `re5_optional` answers and is already correct.

`run.sh:138-163` already records this at length — E2-1's **N1 has landed** — and
it adds the fact that closes the question: **`--synthetic-jdk` is not a flag on
this binary.** A stock build refuses it (exit 1); gating this patch needs a
SECOND BINARY built with `--features synthetic-jdk`. That is why no arm exists,
and why adding one now would produce either a gate that cannot fail or a gate
that fails every run for every lane.

So: **eight `httpmint` rows reach these natives IN PRINCIPLE and zero reach them
AS SCHEDULED.** What would actually gate it:

```
cratonvm-synthetic --synthetic-jdk -cp regression-suite/classes RJdkOptionalShape --only=httpmint
```

with `--only=core` and `--only=prim` first, in their own processes, as negative
controls — if either is red the `httpmint` result is unreadable.

## 5. E2-1's N3 WAS ALREADY LANDED — and executing it would have broken the fixture

E2-1 §6 N3 asked for four `laws(...)` calls for `HttpClient.executor()`,
`.cookieHandler()`, `.proxy()` and `.authenticator()`. **They are already in the
file, and have been since `c59efd3eb`:**

```
$ git show HEAD:regression-suite/src/RJdkOptionalShape.java | grep -n 'http.client.\(auth\|proxy\|cookie\|exec\)'
873:  laws("http.client.authenticator", ...   876:  laws("http.client.cookieHandler", ...
875:  laws("http.client.proxy", ...           878:  laws("http.client.executor", ...
```

E2-1 §3b's row "`H2`–`H5` … **no row exists**" is wrong, and its residual 2
("the cheapest gap to close in Java") is closed. Fixture REACH is 8 of 11, not
4 — with §4b's caveat that reach and coverage are different things here.

**Adding them a second time would have made the fixture RED.** Each block ends
with `sectionEnd(name, expected)` which throws `AssertionError` on an exact
mismatch, and `httpmint`'s header said `12 * LAWS + 14` = 266 — a number that
already counted those four. Four more `laws(...)` = +84 checks against an
unchanged header.

This is the failure mode this session keeps finding, arriving from the other
direction: a nomination written from a stale read, whose literal execution
damages a working gate. The check is one `grep` before one `Edit`.

### What this lane added to the fixture instead (2 rows, +2 checks)

Two builder round-trips that need **no enum constant** — deliberately, because
§7 shows the enum constants are exactly what is unavailable in the mode this
patch runs in:

| row | asserts | reaches |
|---|---|---|
| `mint-ofByteArray-len` | `ofByteArray(new byte[5]).contentLength() == 5` | site 6 |
| `mint-expectContinue-roundtrip` | `expectContinue(true)` round-trips | **the negative control, in Java** |

Header updated to `12 * LAWS + 16`. **Verified on the HotSpot 25 oracle:**
`httpmint=268`, `checks=1418`, `PASS`.

## 6. PREDICTION TABLE — every CratonVM "after" is PREDICTED

Under `--synthetic-jdk` on a `--features synthetic-jdk` binary. "Before" is the
post-E2-1 working tree.

| `CK` observable | before | **after (PREDICTED)** | why |
|---|---|---|---|
| `mint-connectTimeout-absent-present` | `0` | `0` | E2-1; unchanged |
| `mint-timeout-absent-present` | `0` | `0` | E2-1; unchanged |
| `mint-version-absent-present` | `0` | `0` | E2-1; unchanged |
| `mint-bodyPublisher-absent-present` | `0` | `0` | E2-1; unchanged |
| `mint-connectTimeout-present-millis` | RED (empty) | **`1500` — GREEN** | site 2 decodes the `Duration` |
| `mint-timeout-present-millis` | RED (empty) | **`7000` — GREEN** | site 4 decodes the `Duration` |
| `mint-bodyPublisher-present-len` | RED (`0`) | **`2` — GREEN** | §3: `POST` carries the length |
| `mint-version-present-name` | RED (empty) | **STILL RED** | §7 — nothing in this file can fix it |
| `mint-ofByteArray-len` (new) | — | **`5`** | site 6 |
| `mint-expectContinue-roundtrip` (new) | — | **`1`** | the control, unchanged by design |
| `mint-client-version-direct` | `HTTP_2` | `HTTP_2` | unchanged |

**Rows that CANNOT flip in the current schedule: all of them.** Every row above
is answered by `net_phase_e.rs` in the default mode `CORE_CLASSES` runs, so the
scheduled run is bit-identical before and after this patch. The predictions
describe a binary nobody on this box can build.

## 7. WHY `mint-version-present-name` CANNOT GO GREEN — a third defect, upstream of both

The fixture writes `HttpRequest.newBuilder(uri).version(HttpClient.Version.HTTP_2)`.
That is a `GETSTATIC java/net/http/HttpClient$Version.HTTP_2`, and under
`--synthetic-jdk` **there is no such constant to get.**

Traced this session: **the VM has no getstatic-to-native path at all.**
`Instruction::Getstatic` (`vm/src/runtime/interpreter/opcodes.rs:1205-1367`) and
its JIT twin `jit_getstatic` (`vm/src/jit/helpers.rs:6541-6721`) do class
resolution, a JVMTI watchpoint, a `System.out/err/in` intercept, a
`java/lang/Boolean` `TRUE`/`FALSE` special case, and then `get_static_shared`.
No registry lookup. `net_channels.rs:743-751` states the rule in the codebase's
own words: the registry is keyed on `(class, method, descriptor)` and every
lookup comes from an invoke instruction, whose descriptor starts with `(`.

So the field-shaped rows `r.register(hcv, "HTTP_2", "Ljava/net/http/HttpClient$Version;", …)`
(`net_channels.rs:1033-1065`) are **dead registrations** — reachable only by an
explicit `call_native`, which is what `vm/src/vm/tests.rs:46096` does, and by no
bytecode. `java/net/http/HttpClient$Version` has **no** synthetic class
declaration in `classloading/src/class_manager.rs` and **no** `<clinit>` native
anywhere. The GETSTATIC therefore pushes **null** (the prepared default) or
fails to resolve — either way the builder receives `Object(None)`,
`enum_arg_ordinal` correctly answers `None`, `REQ_VERSION` stays `0`, and
`version()` is honestly EMPTY.

**The fix in this file is necessary and not sufficient**, and this is the honest
order of the three: the constant must exist, then the builder must decode it,
then the accessor must mint the mirror. Two of the three are now done. The third
is nominated in §9 N1, with the working model named.

Two consequences worth stating plainly:

* `httpmint`'s `check(... .get() == HttpClient.Version.HTTP_2)` (identity) can
  never pass under a per-call `p57_alloc_enum` even with N1 half-done: that
  helper allocates fresh every call, so `client.version() != client.version()`.
  Interning requires the `<clinit>` shape, not a bridge.
* If the GETSTATIC *throws* rather than pushing null, `httpmint()` dies at its
  fourth statement and **none** of the eleven rows above prints. A run that
  reports no `mint-*` lines at all is that, not a regression in this patch.

## 8. Residuals

1. **The twelve new tests and the one amended test have never been executed.**
   The file parses (`rustfmt`, exit 0); it has not been type-checked. The
   amended `e2_request_version_is_never_an_ordinal` is the first thing to look
   at if the suite is red.
2. **`HttpClient$Builder.version(null)` still defaults to HTTP_2** and
   `connectTimeout(null)` to `0`, where HotSpot throws NPE. Behaviour-preserving,
   deliberately: this patch changes decoding, not null contracts. Same for
   `ofByteArray(null)` → length `0`.
3. **`CLIENT_FOLLOW_REDIR` is still write-only.** §3a stops it being a *lie*
   (it now derives from the policy) but nothing reads it. `followRedirects()`
   the accessor reads `CLIENT_REDIRECT`. It is a candidate for deletion, not for
   more writers.
4. **The GC window E2-1 residual 3 describes is unchanged in kind, and
   `HttpRequest.version()` now joins it — pinned.** This is the FIRST pinned
   site in the file; E2-1 deliberately left its own sites unpinned on the
   grounds that pinning some is worse than pinning none. That reasoning is now
   broken by one site, on purpose, because this one allocates an enum *and* a
   string inside the window. The file still wants one uniform pass.
5. **The other six `phases_late`/`net_phase_e` copies of these same builders
   were not audited.** `net_phase_e.rs:11816` (`connectTimeout`) stores the
   `Duration` object itself and is correct; `net_channels.rs:1010-1022` returns
   `args.first()` and stores nothing. Not this lane's files, and not this
   lane's mode.
6. **`ofString` measures `String::len()` — UTF-8 BYTES.** Correct for
   `BodyPublishers.ofString`'s default UTF-8 charset, and it agrees with
   HotSpot for the fixture's `"hi"`. It would disagree for non-ASCII if anyone
   compares it against `String.length()`.

## 9. NOMINATIONS

### N1 — `native-builtins/src/phases_late/net_channels.rs` + `classloading/src/class_manager.rs` (NOT this lane's files): the enum constants are unreachable from bytecode

**This is what stands between §6's last red row and green, and it is worth more
than that one row: it is every `GETSTATIC` of a synthetic enum constant.**

The five rows at `net_channels.rs:1033-1065` (`HttpClient$Version.HTTP_1_1`,
`.HTTP_2`; `HttpClient$Redirect.NEVER`, `.ALWAYS`, `.NORMAL`) register a FIELD
name with a FIELD descriptor. The registry is only ever consulted from invoke
instructions, so they can never be selected — the same dead shape
`net_channels.rs:743-751` already documents for `SelectionKey.OP_READ` and
`dc55e8057` deleted for `FileVisitResult`.

REPLACE with the shape the VM actually supports — a native
`("java/net/http/HttpClient$Version", "<clinit>", "()V")`, which class init
consults at `vm/src/vm/vm_util.rs:1248-1252` and invokes exactly once — that
mints both constants, publishes them with `set_static_field_by_name`, and
re-reads through the static to build `$VALUES`. The working model is
`native-builtins/src/stack_walker.rs:381-427` (`StackWalker$Option`), whose
comment states the requirement: *re-read through the static so `$VALUES[i]` is
`==` the constant*. Its companion static-field declarations for the stub class
go in `classloading/src/class_manager.rs`, alongside the `PosixFilePermission`
entry at `:13438`. `phases_late/nio_file.rs:19604-19642` is a second worked
example, with an idempotence guard.

Once that lands, §6's `mint-version-present-name` should read `HTTP_2` and the
identity check should pass — both of which this patch has already prepared for.

### N2 — `regression-suite/run.sh` (NOT this lane's file): nothing to do, and that should be recorded as settled

E2-1's N1 is **already answered** at `run.sh:138-163`, more thoroughly than the
nomination asked: it records the mode gap, refuses to add an unrunnable arm, and
names the constraint (a second binary, not a flag). No edit is wanted. This
entry exists so the next lane does not re-open it.

### N3 — `native-builtins/src/http_client.rs` (NOT this lane's file): eight unaudited 1-slot `Optional` sites

Unchanged from E2-1 residual 4 and C12-3 residual 3. E2 read `:1632`
(`connectTimeout`, correct) and `:1652` (`authenticator`, correct); six were
never read. **Right arity is not evidence of the right type** — that is E2-1's
whole point, and five of its eleven sites had the right arity. Add: the same
file should be swept for the §3 shape (a setter that records only THAT a
reference arrived), which the `Value::Int` grep does not find.
