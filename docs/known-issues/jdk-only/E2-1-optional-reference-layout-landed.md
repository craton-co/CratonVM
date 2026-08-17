# E2-1 — the `Optional` reference layout, landed: eleven sites fixed, one deliberately left alone, and the builder family that keeps four fixture rows red

**2026-08-13, lane E2.** Applies `D3-2-http2-optional-reference-layout.md` (from
`C12-3`, sharpened by `C19-1`) to `native-builtins/src/http2.rs`, which this lane
owns. The patch is applied and verified in the working tree.

**This lane may not build or run the VM, and did not.** Every JDK fact below is
from `javap` on this host (25.0.3+9-LTS). Every claim about CratonVM's behaviour
— before and after — is **PREDICTED** from source. **The nine Rust tests this
lane added have never been executed**; they are reviewed-by-inspection only, and
their first run is ahead. That is stated up front because a record that lands
tests and implies they are green is worse than one that lands none.

---

## 0. Verdict

| claim | verdict |
|---|---|
| slot 0 of a real `java.util.Optional` is the reference `value`; an `Int` there makes an EMPTY Optional report PRESENT | **CONFIRMED** — re-derived independently (§1) |
| the primitive Optionals really do carry `(isPresent, value)` and must not be swept up | **CONFIRMED**, and one such site is in this file and was left alone (§2) |
| "nine sites" | **ELEVEN**, as D3-2 found. All eleven are patched (§2) |
| `RJdkOptionalShape`'s four `httpmint` present rows go green | **NO** — D3-2's §6 prediction table is wrong on four rows, and the reason is a second defect family in this same file (§4) |

## 1. The fact the whole patch turns on, re-derived

```
$ javap -p --module java.base java.util.Optional java.util.OptionalInt \
                              java.util.OptionalLong java.util.OptionalDouble

java.util.Optional         private final T value;              <- ONE slot, a REFERENCE
java.util.OptionalInt      private final boolean isPresent;    <- slot 0
                           private final int value;            <- slot 1
java.util.OptionalLong     private final boolean isPresent; private final long value;
java.util.OptionalDouble   private final boolean isPresent; private final double value;
```

`isPresent()` compiles to `getfield value; ifnull`, and `ref_operand_is_null`
(`vm/src/runtime/interpreter.rs:8246`) counts `Object(None)`, `Uninitialized`
and `Long(0)` as null — **and nothing else**:

```rust
fn ref_operand_is_null(v: &Value) -> bool {
    match v {
        Value::Object(None) | Value::Uninitialized => true,
        Value::Long(0) => true,
        _ => false,
    }
}
```

`Value::Int(0)` is not null. So the flag inverted the empty case, and in the
present case `get()` returned the flag itself.

**This is why a blanket fix would have been a regression.** The `(flag, payload)`
idiom is the *correct* layout for three of the four classes in the family. The
eleven sites applied the primitive family's layout to the reference one.

## 2. What was patched, and what was not

All eleven reference-`Optional` producers now allocate **1 slot** and write a
**reference or `Object(None)`** into slot 0. Post-patch sweep of the file:

```
$ grep -n 'set_field(opt, 0, Value::Int' native-builtins/src/http2.rs
2303:                ctx.set_field(opt, 0, Value::Int(1));
2306:                ctx.set_field(opt, 0, Value::Int(0));
$ grep -n 'set_field(opt, 1,' native-builtins/src/http2.rs
2304:                ctx.set_field(opt, 1, Value::Long(20));
2307:                ctx.set_field(opt, 1, Value::Long(0));
```

**Those four lines are the only `(flag, payload)` writes left in the file, and
they are all `firstValueAsLong` — a `java.util.OptionalLong`, whose real layout
IS `(boolean isPresent, long value)`.** It is CORRECT and was not touched. It
sits twelve lines below the last site that was wrong, which remains the best
available explanation of how this happened: the idiom is right for the class one
method away.

| # | line | accessor | was | now |
|---|---|---|---|---|
| H1 | 1289 | `HttpClient.connectTimeout()` | 2 slots, `Int(ms>0)` | 1 slot, a `java.time.Duration` or null |
| H2 | 1319 | `HttpClient.executor()` | 1 slot, `Int(has)` | 1 slot, `Object(None)` |
| H3 | 1335 | `HttpClient.cookieHandler()` | 1 slot, `Int(has)` | 1 slot, `Object(None)` |
| H4 | 1348 | `HttpClient.proxy()` | 1 slot, `Int(has)` | 1 slot, `Object(None)` |
| H5 | 1364 | `HttpClient.authenticator()` | 1 slot, `Int(has)` | 1 slot, `Object(None)` |
| H6 | 1706 | `HttpRequest.bodyPublisher()` | 2 slots, publisher at slot 1 | 1 slot, publisher at slot 0 |
| H7 | 1727 | `HttpRequest.timeout()` | 2 slots, `Int(ms>0)` | 1 slot, a `Duration` or null |
| H8 | 1750 | `HttpRequest.version()` | 2 slots, ordinal at slot 1 | 1 slot, `Object(None)` (§4) |
| H9 | 2108 | `HttpResponse.previousResponse()` | **1 slot**, `Int(has)` | 1 slot, `Object(None)` |
| H10 | 2121 | `HttpResponse.sslSession()` | 2 slots, session at slot 1 | 1 slot, session at slot 0 |
| H11 | 2209 | `HttpHeaders.firstValue(String)` | 2 slots, string at slot 1 | 1 slot, string at slot 0 |
| — | 2242 | `HttpHeaders.firstValueAsLong(String)` | 2 slots, `OptionalLong` | **UNCHANGED — correct** |

H2–H5 and H9 are the five with the *right arity and the wrong type*. The
layout-alias instrument compares slot COUNTS, so it is structurally blind to all
five — which is why a census reading `requested=2 vs declared=1` finds six of
eleven. `H2`–`H5`, `H9` and `H8` answer `empty()` because **no `Executor`,
`ProxySelector`, `Authenticator`, `CookieHandler` or previous `HttpResponse` is
stored anywhere in these layouts** — only a flag is. That is a MISSING answer
replacing a wrong one, which C12-3 itself names as the acceptable step.

`unused_variables = "allow"` is set workspace-wide (`Cargo.toml`
`[workspace.lints.rust]`), so the now-unused `let this = obj_arg(args, 0)?;`
receiver null-checks in H2–H5 and H9 cost no warning under CI's
`cargo clippy --workspace --all-targets -- -D warnings`. `clone_on_copy` is
likewise allowed, though this lane avoided relying on it.

The file remains uniformly CRLF (3,911 `\r\n`, **0** bare `\n`), so the Linux
worktree diff stays clean.

## 3. VERIFICATION SCOPE — stated honestly

### 3a. What actually covers these sites now: nine Rust tests, added by this lane

`http2.rs` had **64 tests and every one of them was registration-only**
(`registry.find(...).is_some()`). All 64 stayed green through the entire life of
this defect. That is the finding behind the tests this lane added: they INVOKE
the natives through the registry against a `MockNativeContext` and assert on the
slot the native actually wrote.

| test | covers |
|---|---|
| `e2_http_client_presence_accessors_answer_empty_not_a_flag` | H2, H3, H4, H5 |
| `e2_connect_timeout_absent_is_null_and_present_is_a_duration` | H1, both arms |
| `e2_request_timeout_absent_is_null_and_present_is_a_duration` | H7, both arms |
| `e2_body_publisher_moves_the_publisher_into_slot_zero` | H6, both arms |
| `e2_request_version_is_empty_never_an_ordinal` | H8 |
| `e2_previous_response_is_empty_not_a_flag` | **H9** |
| `e2_ssl_session_moves_the_session_into_slot_zero` | **H10**, both arms |
| `e2_first_value_moves_the_string_into_slot_zero` | **H11**, both arms |
| `e2_optional_long_keeps_the_primitive_flag_payload_layout` | **NEGATIVE CONTROL** |

Three properties make these worth more than the count suggests:

1. **They set the backing field DIRECTLY** rather than driving the builders, so
   they measure the accessor's `Optional` shape in isolation. This matters
   enormously given §4 — the present arms of H1/H7/H8 are currently unreachable
   *through Java*, and these tests are the only thing that exercises them.
2. **Every one asserts `object_num_fields(opt) == 1`** and routes through
   `assert_not_a_flag`, which fails with the diagnosis rather than a bare
   inequality. The mock initialises fields to `Value::Int(0)`, so a native that
   wrote *nothing* to slot 0 also fails — the same shape as the bug.
3. **The negative control is the load-bearing one.** It pins
   `firstValueAsLong` at 2 slots with `Int(1)` at slot 0 and `Long(20)` at slot
   1, and its message says a flag there is CORRECT. Anyone who later
   "simplifies" this fix into a blanket one breaks that test by name.

**These tests have not been run.** This lane may not invoke cargo.

### 3b. What `RJdkOptionalShape` can and cannot verify

`RJdkOptionalShape` (C19-1: 1,416 checks, green on HotSpot 25) is already
registered — `regression-suite/run.sh:124` lists it in `CORE_CLASSES`, so
C19-1's nomination has landed.

**As scheduled it does not gate this patch at all.** D3-2 §3 established that
`register_http2_natives` has exactly one production caller
(`native-builtins/src/lib.rs:24187`), inside
`#[cfg(feature = "synthetic-jdk")] register_synthetic_overrides`, gated at
runtime by `config.use_synthetic_jdk` (`vm/src/vm/vm_init.rs:1930`). `CORE_CLASSES`
runs default mode, where `net_phase_e.rs`'s `re5_optional` (`Optional.ofNullable`)
answers these accessors and is already correct. This lane re-confirmed the single
caller by grep. **A green `httpmint` in `CORE_CLASSES` says nothing about this
patch; it exercises a different, already-correct implementation.**

Under `--synthetic-jdk`, the fixture reaches **four of eleven** sites:

| site | `httpmint` absent row | `httpmint` present row |
|---|---|---|
| H1 `connectTimeout()` | **YES** | reaches it, but see §4 |
| H7 `timeout()` | **YES** | reaches it, but see §4 |
| H6 `bodyPublisher()` | **YES** | reaches it, but see §4 |
| H8 `version()` | **YES** | reaches it, but see §4 |
| H2–H5 `executor`/`cookieHandler`/`proxy`/`authenticator` | **no row exists** (reachable in principle — four `laws(...)` calls, no network) |
| H9 `previousResponse()`, H10 `sslSession()`, H11 `firstValue()` | **no** — need a real `HttpResponse` |

**So: four sites reachable by the fixture (absent arms only, per §4); seven not
reachable by any Java fixture in this repository.** `previousResponse()` — the
row C12-3 calls "the one that settles what this is" — is covered by neither the
fixture nor the layout-alias instrument. §3a's `e2_previous_response_is_empty_not_a_flag`
is now its **only** coverage, and the same is true of H10 and H11.

## 4. THE CORRECTION TO D3-2 — six builders read a reference argument as an `Int`

D3-2 §4 found this for `HttpRequest$Builder.version` and made it a residual
("the same shape should be swept for across `http2.rs`'s builders"). **This lane
ran that sweep, and it is six sites, not one.** Matching `Some(Value::Int(n))`
against `args[1]` when the descriptor's first parameter is a reference means the
`_ =>` fallback ALWAYS wins:

| line | builder | descriptor param | fallback that always wins |
|---|---|---|---|
| 1468 | `HttpClient$Builder.version` | `HttpClient$Version` | `HTTP_VERSION_2` |
| **1485** | **`HttpClient$Builder.connectTimeout`** | **`Duration`** | **`0`** |
| 1501 | `HttpClient$Builder.followRedirects` | `HttpClient$Redirect` | `REDIRECT_NEVER` |
| **1983** | **`HttpRequest$Builder.timeout`** | **`Duration`** | **`0`** |
| 2015 | `HttpRequest$Builder.version` | `HttpClient$Version` | `0` |
| 2375 | `BodyPublishers.ofByteArray` | `[B` | `0` |

`expectContinue(Z)` (`:1998`) uses the identical `Some(Value::Int(n))` idiom and
is **CORRECT**, because its descriptor really is a primitive `boolean`. That is
the same control structure as `firstValueAsLong` in §2: the idiom is right for
the neighbours and wrong here.

**Consequence, and it changes D3-2 §6's prediction table.** `CLIENT_CONNECT_TIMEOUT`
and `REQ_TIMEOUT` can never be non-zero, exactly as D3-2 showed for `REQ_VERSION`.
So H1's and H7's present arms are currently dead code too — and the `Optional`
they produce through Java is EMPTY, not populated:

| `CK` observable | before | **after this patch (PREDICTED, `--synthetic-jdk`)** | why |
|---|---|---|---|
| `mint-connectTimeout-absent-present` | `1` | **`0`** | fixed by H1 |
| `mint-timeout-absent-present` | `1` | **`0`** | fixed by H7 |
| `mint-bodyPublisher-absent-present` | `1` | **`0`** | fixed by H6 |
| `mint-version-absent-present` | `1` | **`0`** | fixed by H8 |
| `mint-connectTimeout-present-millis` | throws / not a `Duration` | **STILL RED** — Optional is empty | builder `:1485` discards the `Duration` |
| `mint-timeout-present-millis` | throws / not a `Duration` | **STILL RED** — Optional is empty | builder `:1983` discards the `Duration` |
| `mint-version-present-name` | throws | **STILL RED** — Optional is empty | builder `:2015` discards the `Version` |
| `mint-bodyPublisher-present-len` | throws / not a `BodyPublisher` | **STILL RED** — length `0`, not `2` | `POST` (`:1893`) sets `REQ_HAS_BODY` but never `REQ_BODY_LEN` |

**D3-2 §6 predicted `1500`, `7000` and `2` for three of those rows. That is
wrong, and this record supersedes it.** What this patch delivers on the present
rows is not a correct value but a *fail-safe* one: an honestly-empty `Optional`
or a dereferenceable object, instead of an `Int` handed to a caller that will
dereference it. The four ABSENT rows — the ones that directly encode the
inverted-`isPresent()` diagnosis — are the rows that go green.

**This lane deliberately did not fold the builder fixes in**, even though
`http2.rs` is its file. They are a distinct defect family (argument decoding, not
`Optional` layout) with six members; combining two independent fixes in one
change is precisely the "concurrent fix combination untested" hazard, and a
red-to-green movement on the absent rows is a cleaner signal than a mixed one.
The exact shape is nominated in §6.

## 5. Residuals

1. **The nine new tests have never been executed.** Reviewed by inspection
   against the mock's API (`find` → `Option<NativeCallback>`;
   `class_num_total_fields` defaults to `0` so `num_fields.max(real)` honours the
   1-slot request; `try_alloc_object_gc_safe` defaults to `alloc_object`;
   `create_string`/`read_string` round-trip). Their first run is the gate.
2. **Seven of eleven sites have no Java-fixture coverage** and now rest entirely
   on §3a's Rust tests. H2–H5 are the cheapest gap to close in Java — four
   `laws(...)` calls against `HttpClient.newBuilder().build()`, no network.
   H9/H10/H11 need a loopback `com.sun.net.httpserver.HttpServer` (a 302 reaches
   `previousResponse()`; TLS is required for `sslSession()`).
3. **The two-object GC window is unchanged in kind but H1 and H7 now join it.**
   H6, H10 and H11 already allocated a payload after the `Optional` and wrote
   through the earlier `ObjectRef`; H1 and H7 now do the same via
   `util_time::alloc_duration`. This is the native-stale-local family, it is
   exactly what the correct sibling `http_client.rs:1632` does, and it is NOT
   pinned here — per D3-2's reasoning that pinning some of a file's sites is
   worse than pinning none. It wants one file-wide `pin_native_root` /
   `read_native_pin` / `unpin_native_roots` pass.
4. **`http_client.rs`'s eight 1-slot sites are still unaudited** (C12-3 residual
   3, D3-2 residual 2). This lane read `:1632` (`connectTimeout` — correct, and
   the model for H1) and `:1652` (`authenticator` — correct). The other six were
   not read, and **right arity is not evidence of the right type — that is this
   record's whole point** (H2–H5, H9). Not this lane's file.
5. **`layout_alias::classify` will stop reporting `java/util/Optional`** with
   `requested=2 declared=1`, because the six 2-slot requests are gone. A census
   that used those rows as its tracking signal loses it — and it never saw the
   five 1-slot rows anyway. Do not read the disappearance as proof of anything
   beyond the arity.

## 6. NOMINATIONS

### N1 — `regression-suite/run.sh` (not this lane's file): the fixture needs a `--synthetic-jdk` arm

`RJdkOptionalShape` is in `CORE_CLASSES` (`:124`), which runs default mode —
where `net_phase_e.rs` answers and this patch's code never executes (§3b).
**The fixture as scheduled cannot gate this patch.** Either add a synthetic-mode
arm, or record in `run.sh` that `httpmint` is a default-mode compatibility check
only and does not cover `http2.rs`. Without one of those, a future reader will
reasonably conclude from a green `httpmint` that these eleven sites are verified.
The command that actually gates it:

```
cratonvm --synthetic-jdk -cp regression-suite/classes RJdkOptionalShape --only=httpmint
```

with `--only=core` and `--only=prim` first, in their own processes, as negative
controls — if either is red the `httpmint` result is unreadable.

### N2 — `native-builtins/src/http2.rs` (THIS lane's file; deliberately deferred, not overlooked)

Six builders decode a reference argument as `Value::Int` (§4). The two `Duration`
ones are the cheapest and unblock two fixture rows; neither allocates, so neither
adds a GC window.

REPLACE (`:1483`, `HttpClient$Builder.connectTimeout`) and the identical block at
`:1981` (`HttpRequest$Builder.timeout`):

```rust
            let ms = match args.get(1) {
                Some(Value::Long(n)) => *n,
                Some(Value::Int(n)) => *n as i64,
                _ => 0,
            };
```

WITH a decode of the `java.time.Duration` the descriptor actually passes —
`DUR_FIELD_SECONDS` (slot 0, `Long`) and `DUR_FIELD_NANOS` (slot 1, `Int`), the
same two slots `util_time::alloc_duration` writes — keeping the existing
`Long`/`Int` arms as a fallback for any caller that passes raw millis.

`:1468`, `:1501`, `:2015` and `:2375` need the same treatment against an enum
mirror, an enum mirror, an enum mirror and a byte array respectively. `:2015` is
what stands between H8 and a correct `version()`; when it is fixed, H8 becomes a
two-line change to build the enum mirror via `version_enum` — and that arm MUST
pin `opt` across the allocation (residual 3).

### N3 — `regression-suite/src/RJdkOptionalShape.java` (not this lane's file)

Add four `laws(...)` calls for `HttpClient.executor()`, `.cookieHandler()`,
`.proxy()` and `.authenticator()` off `HttpClient.newBuilder().build()`. They
need no network and would take fixture coverage of the eleven sites from four to
eight. All four must be ABSENT rows; `empty()` is the correct answer and the
present case is unreachable by construction (§2).
