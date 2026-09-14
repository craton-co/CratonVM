# D3-2 — `http2.rs`'s `Optional`s: eleven sites, not nine — and every one of them is dead outside synthetic-JDK mode

**2026-08-13, lane D3.** Applies wave D's second queue entry
(`WAVE-D-QUEUE.md` row 2, from `C12-3`, sharpened by `C19-1`). The patch below
is apply-ready and verified byte-for-byte against the working tree.

**This lane may not write `.rs` and may not build or run the VM.** JDK facts are
from `javap` on this host (25.0.3+9-LTS); everything about CratonVM is a source
reading, and the two findings that change the queue entry are both registration
facts anyone can re-derive with `grep`.

---

## 0. Verdict, up front

| claim | verdict |
|---|---|
| slot 0 of a real `java.util.Optional` is the reference `value`, and an `Int` there makes an EMPTY Optional report PRESENT | **CONFIRMED**, and now on BOTH readers (§1) |
| the primitive Optionals genuinely do carry `(isPresent, value)` and must not be "fixed" | **CONFIRMED** — and one such site exists in this very file and is CORRECT (§2) |
| "nine sites" | **ELEVEN** (§2). Four `HttpClient` accessors with the right arity and an `Int` in slot 0 were not on C12-3's list |
| `RJdkOptionalShape`'s `httpmint` block reaches four of them | **NOT AS SCHEDULED** (§3). `register_http2_natives` runs only under `use_synthetic_jdk`; in the mode `CORE_CLASSES` runs, a DIFFERENT and CORRECT implementation answers |

The patch is right and should land. What must not be carried forward is the
belief that a default-mode fixture run gates it.

## 1. Both readers model slot 0 as a reference — so the flag is wrong twice over

The real class, `javap -p --module java.base`, JDK 25:

```
java.util.Optional         private final T value;             <- ONE slot, a REFERENCE
java.util.OptionalInt      private final boolean isPresent;   <- slot 0
                           private final int value;           <- slot 1
java.util.OptionalLong     private final boolean isPresent; private final long value;
java.util.OptionalDouble   private final boolean isPresent; private final double value;
```

`isPresent()` is `getfield value; ifnull`, and `ifnull` on `Value::Int(0)` does
not take the branch — `ref_operand_is_null`
(`vm/src/runtime/interpreter.rs:8232`) counts `Object(None)`, `Uninitialized`
and `Long(0)` as null, and **nothing else**.

**C12-3 could not check the other reader, and it agrees.** In synthetic-JDK mode
`java.util.Optional` is not bytecode at all — it is six natives registered by
`phases_early::register_core_stdlib_extras` (`native-builtins/src/phases_early.rs:1553`),
and they model exactly the same thing:

```rust
    // Optional = 1-field (value=0)
    r.register(opt, "get", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(opt, "isPresent", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match ctx.get_field(this, 0) {
            Value::Object(None) => Ok(Some(Value::Int(0))),
            _ => Ok(Some(Value::Int(1))),
        }
    });
```

So in the ONE mode where the `http2.rs` sites actually run (§3), the VM's own
`Optional.isPresent` answers `true` for `Int(0)` and its own `Optional.get`
returns the flag. **The two implementations of `Optional` in this repository
agree with each other and with the JDK; the eleven producers disagree with all
three.**

## 2. The census — eleven producers, one correct primitive, and what each owes

Every `try_alloc_concurrent_synthetic(ctx, "java/util/Optional", …)` in
`native-builtins/src/http2.rs`, read in full:

| line | accessor | declared | slots | slot 0 today | what exists to put there |
|---|---|---|---|---|---|
| 1289 | `HttpClient.connectTimeout()` | `Optional<Duration>` | 2 | `Int(ms>0)` | a `Duration` from `ms` |
| **1319** | `HttpClient.executor()` | `Optional<Executor>` | 1 | `Int(has)` | **nothing** — only a flag is stored |
| **1335** | `HttpClient.cookieHandler()` | `Optional<CookieHandler>` | 1 | `Int(has)` | **nothing** |
| **1348** | `HttpClient.proxy()` | `Optional<ProxySelector>` | 1 | `Int(has)` | **nothing** |
| **1364** | `HttpClient.authenticator()` | `Optional<Authenticator>` | 1 | `Int(has)` | **nothing** |
| 1706 | `HttpRequest.bodyPublisher()` | `Optional<BodyPublisher>` | 2 | `Int(has_body)` | the publisher, today parked at slot 1 |
| 1727 | `HttpRequest.timeout()` | `Optional<Duration>` | 2 | `Int(ms>0)` | a `Duration` from `ms` |
| 1750 | `HttpRequest.version()` | `Optional<HttpClient$Version>` | 2 | `Int(ver!=0)` | an enum mirror — **but see §4** |
| 2108 | `HttpResponse.previousResponse()` | `Optional<HttpResponse>` | **1** | `Int(has)` | **nothing** |
| 2121 | `HttpResponse.sslSession()` | `Optional<SSLSession>` | 2 | `Int(has)` | the session, today parked at slot 1 |
| 2209 | `HttpHeaders.firstValue(String)` | `Optional<String>` | 2 | `Int(1)`/`Int(0)` | the string, today parked at slot 1 |

**The four bolded rows are new.** C12-3's table listed seven `http2.rs` sites; the
queue said nine. The four `HttpClient` accessors have the CORRECT arity (1) and
the WRONG type in slot 0 — the same shape as `previousResponse()`, which C12-3
calls "the row that settles what this is". There are four more of it, in the
same file, twelve lines apart. **The layout-alias instrument is blind to all
five** (it compares counts, and these counts are right), which is exactly why a
census that reads `requested=2 vs declared=1` finds six of eleven.

**The one site that must NOT be touched is twelve lines below the last of them:**

```rust
    // firstValueAsLong(String name) -> OptionalLong
    let opt = try_alloc_concurrent_synthetic(ctx, "java/util/OptionalLong", 2)?;
    ctx.set_field(opt, 0, Value::Int(1));
    ctx.set_field(opt, 1, Value::Long(20));
```

`http2.rs:2242`. Two slots, `isPresent` at 0, `value` at 1 — **that is
`OptionalLong`'s real layout** (§1), so this row is CORRECT and the patch leaves
it alone. It is also the best available explanation of how the eleven happened:
the idiom is right for the class one line away.

### What "nothing to put there" means for the five flag-only rows

`CLIENT_HAS_EXECUTOR` / `_PROXY` / `_AUTH` / `_COOKIE` and `RESP_HAS_PREV` are
each written in exactly two places: `Value::Int(0)` by `alloc_http_client` /
`alloc_http_response`, and a verbatim copy from the builder in
`HttpClient$Builder.build()` (`:1594`-`:1608`). **No `Executor`, `ProxySelector`,
`Authenticator`, `CookieHandler` or previous `HttpResponse` object is stored
anywhere in these layouts.** So `Optional.empty()` is the only answer this
representation can give truthfully — a MISSING answer where there is currently a
wrong one, which is the smaller step C12-3 itself names. It also fixes a live
crash shape: `net_phase_e.rs:11938` records that Spring's
`JdkClientHttpRequestFactory` constructor calls `executor()`, and today it
receives an Optional that says PRESENT and then hands it an `Int` where an
`Executor` is expected.

## 3. WHERE THESE NATIVES ACTUALLY RUN — the finding that reframes the entry

`register_http2_natives` has exactly one production caller
(`grep -rn register_http2_natives --include=*.rs .`; every other hit is a `#[cfg(test)]`
registry in the same file):

```
native-builtins/src/lib.rs:24187   register_http2_natives(registry);
```

which is inside

```rust
#[cfg(feature = "synthetic-jdk")]
pub fn register_synthetic_overrides(registry: &mut NativeMethodRegistry) {   // lib.rs:21555
```

and `vm/src/vm/vm_init.rs:1930`-`1934` gates the whole family at RUNTIME:

```rust
        #[cfg(feature = "synthetic-jdk")]
        {
            if config.use_synthetic_jdk {
                register_builtins(&mut native_methods);      // -> register_synthetic_overrides
            } else {
                // Real-JDK mode: register essential natives only. Do NOT use
                // register_builtins — synthetic overrides assume synthetic field
                // layouts and corrupt real JDK objects.
```

**So all eleven sites are dead in default mode, in `--real-jdk` mode and under
`--jdk-only`.** In those modes the same five `HttpClient` accessors are served by
`net_phase_e.rs:11940`-`11953`, whose `register_client_optional!` macro calls
`re5_optional` — which is `Optional.ofNullable(value)` and is **correct**
(`net_phase_e.rs:10717`). `HttpRequest.timeout()` is served by
`net_phase_e.rs:12110`, likewise via `Optional.of`/`Optional.empty`. Registration
order settles the overlap the other way in a synthetic build:
`register_essential_natives_with_shims` (lib.rs:7103, containing net_phase_e) runs
BEFORE `register_synthetic_overrides` (lib.rs:21555), and
`NativeMethodRegistry::register` is last-write-wins (`native-api/src/registry.rs:6901`,
the `Some(idx) => slot.callback = callback` arm). So under `--synthetic-jdk` the
broken bodies overwrite the correct ones.

### What this does to C19-1's reachability claim

C19-1 §3 says the four `httpmint` rows "reach it, verified by construction". They
reach it **only under `--synthetic-jdk`**. C19-1's own nomination puts
`RJdkOptionalShape` in `CORE_CLASSES`, which runs default mode — where
`connectTimeout()` answers through `re5_optional` and `timeout()` through
`net_phase_e`, both already correct. A green `httpmint` there says nothing about
this patch.

**Which sites the fixture can verify, honestly:**

| site | `RJdkOptionalShape` under `--synthetic-jdk` | under default / `--jdk-only` |
|---|---|---|
| `connectTimeout()` (1289) | **YES** — `mint-connectTimeout-absent-present`, and the present row dereferences a `Duration` | no (net_phase_e answers) |
| `timeout()` (1727) | **YES** | no (net_phase_e answers) |
| `bodyPublisher()` (1706) | **YES** | not established |
| `version()` (1750) | **partly** — the absent row, yes. The present row will still fail, for a DIFFERENT reason (§4) |
| `executor()` (1319) | reachable in principle; **no row exists** | no |
| `cookieHandler()` / `proxy()` / `authenticator()` (1335/1348/1364) | reachable in principle; **no rows exist** | no |
| `previousResponse()` (2108), `sslSession()` (2121) | **no** — need a real `HttpResponse` (C19-1 residual 1) | no |
| `firstValue()` (2209) | **no** — needs a real `HttpResponse` to get `HttpHeaders` | no |

So: **three sites fully verifiable, one half, seven unverified.** The four
`HttpClient` presence accessors are the cheapest gap to close — they need only
`HttpClient.newBuilder().build()` and four `laws(...)` calls, no network.

## 4. `version()` is unconditionally empty, and that is a measurement not a shortcut

`REQ_VERSION` has one writer,
`HttpRequest$Builder.version(HttpClient$Version)` (`http2.rs:1966`):

```rust
            let ver = match args.get(1) {
                Some(Value::Int(n)) => *n + 1, // 1=HTTP_1_1, 2=HTTP_2 (0 = no override)
                _ => 0,
            };
```

The descriptor is `(Ljava/net/http/HttpClient$Version;)…`, so `args[1]` is
always `Value::Object`, so the `_ => 0` arm always wins and `REQ_VERSION` is
**always 0**. A present arm in `version()` would therefore be dead code — and
dead code that holds the freshly-allocated `Optional` live across
`version_enum`'s allocation (`p57_alloc_enum` allocates on every call and pins
internally for exactly this reason). The patch answers `Optional.empty()` and
names the builder as the thing to fix first.

**Consequence to state plainly:** `RJdkOptionalShape`'s
`mint-version-present-name=HTTP_2` row will still be red after this patch, and
that is a SEPARATE defect (`Builder.version` discarding its argument), not this
one. Its `mint-version-absent-present=0` row will go green.

## 5. THE PATCH — `native-builtins/src/http2.rs`

**The file is uniformly CRLF (3,519 `\r\n`, 3,519 `\n`).** All eleven OLD blocks
were verified with `str.count()` to occur **exactly once** and to remain unique
after the earlier edits in this list are applied. Apply in the order given.

Nothing here changes an arity that was already 1 except by changing what goes in
slot 0; nothing here touches `OptionalLong`.

### H1 — `HttpClient.connectTimeout()` (`:1289`)

REPLACE:

```rust
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 2)?;
            ctx.set_field(opt, 0, Value::Int(if ms > 0 { 1 } else { 0 }));
            ctx.set_field(opt, 1, Value::Long(ms));
            Ok(Some(Value::Object(Some(opt))))
```

WITH:

```rust
            // `java.util.Optional` has ONE instance field and it is a
            // REFERENCE (`javap -p java.util.Optional`, JDK 25): `isPresent()`
            // is `value != null` and `get()` returns `value` itself. This VM's
            // own synthetic `Optional` natives model it identically
            // (`phases_early::register_core_stdlib_extras`). A presence flag in
            // slot 0 therefore IS the value, and `Value::Int(0)` is not null to
            // either reader -- so an EMPTY Optional reported PRESENT and
            // `get()` handed back the flag. The (flag, payload) layout is the
            // real layout of `OptionalInt`/`OptionalLong`/`OptionalDouble`, not
            // of this class. See
            // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md
            // Declared `Optional<Duration>`; the same body as
            // `http_client.rs`'s `connectTimeout`.
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            if ms > 0 {
                let nanos = ((ms % 1000) * 1_000_000) as i32;
                let dur = crate::util_time::alloc_duration(ctx, ms / 1000, nanos);
                ctx.set_field(opt, 0, Value::Object(Some(dur)));
            } else {
                ctx.set_field(opt, 0, Value::Object(None));
            }
            Ok(Some(Value::Object(Some(opt))))
```

`crate::util_time::alloc_duration(ctx, seconds, nanos) -> ObjectRef` is
`pub(crate)` (`util_time.rs:122`), normalises the pair, and writes the same two
slots (`DUR_FIELD_SECONDS = 0`, `DUR_FIELD_NANOS = 1`, `DUR_NUM_FIELDS = 2`) that
`http_client.rs:1636`'s inline version writes by hand. **C12-3 said
`alloc_duration_millis` "does not exist and is the real work in this
nomination"; it does exist, under a different name, with a working caller.**

### H2 — `HttpClient.executor()` (`:1319`)

REPLACE:

```rust
        let has = match ctx.get_field(this, CLIENT_HAS_EXECUTOR) {
            Value::Int(n) => n,
            _ => 0,
        };
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        ctx.set_field(opt, 0, Value::Int(has));
```

WITH:

```rust
        // Slot 0 of a `java.util.Optional` is `value`, a REFERENCE -- an `Int`
        // there reads as PRESENT and `get()` returns the flag. See
        // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md.
        // The synthetic client stores a presence FLAG and never the `Executor`
        // itself (`CLIENT_HAS_EXECUTOR` is only ever written `Int(0)` by
        // `alloc_http_client`, then copied by `Builder.build()`), so `empty()`
        // is the only answer this layout can give truthfully: a MISSING answer
        // where there was a wrong one.
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        ctx.set_field(opt, 0, Value::Object(None));
```

### H3 — `HttpClient.cookieHandler()` (`:1335`)

REPLACE:

```rust
            let has = match ctx.get_field(this, CLIENT_HAS_COOKIE) {
                Value::Int(n) => n,
                _ => 0,
            };
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            ctx.set_field(opt, 0, Value::Int(has));
```

WITH:

```rust
            // See `executor()` above and
            // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md:
            // slot 0 is the reference `value`, and this layout holds a flag
            // rather than the `CookieHandler`, so `empty()` is the only
            // truthful answer.
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            ctx.set_field(opt, 0, Value::Object(None));
```

### H4 — `HttpClient.proxy()` (`:1348`)

REPLACE:

```rust
        let has = match ctx.get_field(this, CLIENT_HAS_PROXY) {
            Value::Int(n) => n,
            _ => 0,
        };
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        ctx.set_field(opt, 0, Value::Int(has));
```

WITH:

```rust
        // See `executor()` above: slot 0 is the reference `value`, and this
        // layout holds a flag rather than the `ProxySelector`.
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        ctx.set_field(opt, 0, Value::Object(None));
```

### H5 — `HttpClient.authenticator()` (`:1364`)

REPLACE:

```rust
            let has = match ctx.get_field(this, CLIENT_HAS_AUTH) {
                Value::Int(n) => n,
                _ => 0,
            };
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            ctx.set_field(opt, 0, Value::Int(has));
```

WITH:

```rust
            // See `executor()` above: slot 0 is the reference `value`, and
            // this layout holds a flag rather than the `Authenticator`.
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            ctx.set_field(opt, 0, Value::Object(None));
```

### H6 — `HttpRequest.bodyPublisher()` (`:1706`)

REPLACE:

```rust
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 2)?;
            ctx.set_field(opt, 0, Value::Int(has_body));
            if has_body == 1 {
                let len = match ctx.get_field(this, REQ_BODY_LEN) {
                    Value::Long(n) => n,
                    _ => 0,
                };
                let bp = alloc_body_publisher(ctx, len)?;
                ctx.set_field(opt, 1, Value::Object(Some(bp)));
            }
```

WITH:

```rust
            // Slot 0 of a `java.util.Optional` is `value`, a REFERENCE. The
            // publisher belongs THERE; parked at slot 1 nothing ever read it.
            // See
            // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            if has_body == 1 {
                let len = match ctx.get_field(this, REQ_BODY_LEN) {
                    Value::Long(n) => n,
                    _ => 0,
                };
                let bp = alloc_body_publisher(ctx, len)?;
                ctx.set_field(opt, 0, Value::Object(Some(bp)));
            } else {
                ctx.set_field(opt, 0, Value::Object(None));
            }
```

### H7 — `HttpRequest.timeout()` (`:1727`)

REPLACE:

```rust
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 2)?;
        ctx.set_field(opt, 0, Value::Int(if ms > 0 { 1 } else { 0 }));
        ctx.set_field(opt, 1, Value::Long(ms));
```

WITH:

```rust
        // Declared `Optional<Duration>`; slot 0 is the reference `value`. See
        // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        if ms > 0 {
            let nanos = ((ms % 1000) * 1_000_000) as i32;
            let dur = crate::util_time::alloc_duration(ctx, ms / 1000, nanos);
            ctx.set_field(opt, 0, Value::Object(Some(dur)));
        } else {
            ctx.set_field(opt, 0, Value::Object(None));
        }
```

### H8 — `HttpRequest.version()` (`:1750`) — see §4

REPLACE:

```rust
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 2)?;
        ctx.set_field(opt, 0, Value::Int(if ver != 0 { 1 } else { 0 }));
        ctx.set_field(opt, 1, Value::Int(if ver > 0 { ver - 1 } else { 0 }));
```

WITH:

```rust
        // Slot 0 of a `java.util.Optional` is `value`, a REFERENCE, and this
        // is declared `Optional<HttpClient$Version>` -- so slot 0 must hold an
        // ENUM MIRROR or null, never an ordinal and never a flag. See
        // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md.
        //
        // It is unconditionally EMPTY, and that is not laziness: `REQ_VERSION`
        // can never be non-zero. Its only writer is
        // `HttpRequest$Builder.version(HttpClient$Version)`, which matches
        // `Some(Value::Int(n))` against an argument whose descriptor is
        // `(Ljava/net/http/HttpClient$Version;)` -- always a reference, so the
        // `_ => 0` arm always wins. `ver > 0` is therefore unreachable, and a
        // present arm here would be dead code that also holds `opt` live across
        // `version_enum`'s allocation. Fix the BUILDER first (its own
        // nomination); this site becomes a two-line change once `REQ_VERSION`
        // can actually carry an ordinal.
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        ctx.set_field(opt, 0, Value::Object(None));
```

### H9 — `HttpResponse.previousResponse()` (`:2108`)

REPLACE:

```rust
            let has = match ctx.get_field(this, RESP_HAS_PREV) {
                Value::Int(n) => n,
                _ => 0,
            };
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            ctx.set_field(opt, 0, Value::Int(has));
```

WITH:

```rust
            // The arity was already right and the TYPE was not: slot 0 of a
            // `java.util.Optional` is the reference `value`, so an `Int` there
            // reads as PRESENT. This is the row that shows the defect is the
            // flag and not the slot count. `RESP_HAS_PREV` is only ever written
            // `Int(0)` and no previous response is stored anywhere, so
            // `empty()` is the truthful answer. See
            // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            ctx.set_field(opt, 0, Value::Object(None));
```

### H10 — `HttpResponse.sslSession()` (`:2121`)

REPLACE:

```rust
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 2)?;
        ctx.set_field(opt, 0, Value::Int(has));
        if has == 1 {
            let ssl = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", 6)?;
            ctx.set_field(opt, 1, Value::Object(Some(ssl)));
        }
```

WITH:

```rust
        // Slot 0 of a `java.util.Optional` is the reference `value`; the
        // session belongs THERE. See
        // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        if has == 1 {
            let ssl = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", 6)?;
            ctx.set_field(opt, 0, Value::Object(Some(ssl)));
        } else {
            ctx.set_field(opt, 0, Value::Object(None));
        }
```

### H11 — `HttpHeaders.firstValue(String)` (`:2209`)

REPLACE:

```rust
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 2)?;
            let lower = queried.to_lowercase();
            if lower == "content-type" && has_ct == 1 {
                let sv = ctx.create_string("application/json");
                ctx.set_field(opt, 0, Value::Int(1));
                ctx.set_field(opt, 1, Value::Object(Some(sv)));
            } else if lower == "content-length" && has_cl == 1 {
                let sv = ctx.create_string("20");
                ctx.set_field(opt, 0, Value::Int(1));
                ctx.set_field(opt, 1, Value::Object(Some(sv)));
            } else {
                ctx.set_field(opt, 0, Value::Int(0));
                ctx.set_field(opt, 1, Value::Object(None));
            }
```

WITH:

```rust
            // Slot 0 of a `java.util.Optional` is the reference `value`; the
            // header string belongs THERE. See
            // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            let lower = queried.to_lowercase();
            if lower == "content-type" && has_ct == 1 {
                let sv = ctx.create_string("application/json");
                ctx.set_field(opt, 0, Value::Object(Some(sv)));
            } else if lower == "content-length" && has_cl == 1 {
                let sv = ctx.create_string("20");
                ctx.set_field(opt, 0, Value::Object(Some(sv)));
            } else {
                ctx.set_field(opt, 0, Value::Object(None));
            }
```

### Lint note

H2–H5 and H9 drop the `let has = …` binding, leaving `let this = obj_arg(args, 0)?;`
unused. That is deliberate — it is the receiver null-check — and it costs no
warning: `unused_variables = "allow"` is set workspace-wide
(`Cargo.toml`, `[workspace.lints.rust]`), which matters because CI runs
`cargo clippy --workspace --all-targets -- -D warnings`.

## 6. How to settle it

```
cratonvm --synthetic-jdk -cp regression-suite/classes RJdkOptionalShape --only=httpmint
```

**The `--synthetic-jdk` flag is not optional and is the whole point of §3.**
`--only=core` and `--only=prim` first, in their own processes: they are the
negative controls, and if either is red the `httpmint` result is unreadable.

Expected movement from this patch, per C19-1's own `CK` observables:

| observable | before | after |
|---|---|---|
| `mint-connectTimeout-absent-present` | `1` | `0` |
| `mint-timeout-absent-present` | `1` | `0` |
| `mint-bodyPublisher-absent-present` | `1` | `0` |
| `mint-version-absent-present` | `1` | `0` |
| `mint-connectTimeout-present-millis` | throws / not a `Duration` | `1500` |
| `mint-timeout-present-millis` | throws / not a `Duration` | `7000` |
| `mint-bodyPublisher-present-len` | throws / not a `BodyPublisher` | `2` |
| `mint-version-present-name` | throws | **still red — §4, a different defect** |

Also worth one `--dump-native-registry` on a synthetic-mode run: `java/util/Optional`
should stop appearing in any layout-alias census with `requested=2 declared=1`,
because the six 2-slot requests are gone.

## Residuals

1. **Seven of the eleven sites stay unverified by any in-tree instrument.**
   `previousResponse()`, `sslSession()` and `firstValue()` need a real
   `HttpResponse` (a loopback `com.sun.net.httpserver.HttpServer` fixture; a
   302 reaches `previousResponse()`, TLS is needed for `sslSession()`). The four
   `HttpClient` presence accessors need only four more `laws(...)` calls in
   `httpmint` and nobody has written them. This is carried forward from C19-1
   residual 1, widened by the four sites C12-3 did not list.
2. **`http_client.rs`'s eight 1-slot sites were still not audited** (C12-3
   residual 3). This lane read `:1630`-`:1643` (`connectTimeout`, correct, and
   the model for H1) and `:1646`-`:1655` (`authenticator`, correct — it copies a
   stored reference) but not the other six. Right arity is not evidence of the
   right type; that is this record's whole point and it applies to the sites it
   did not read.
3. **`HttpRequest$Builder.version(HttpClient$Version)` discards its argument**
   (§4). Separate defect, separate patch, and it is what stands between H8 and a
   correct `version()`. The same `Some(Value::Int(n))`-against-a-reference shape
   should be swept for across `http2.rs`'s builders before it is fixed one at a
   time.
4. **The two-object GC window is pre-existing and this patch neither adds nor
   removes it.** H6, H10 and H11 allocate a payload (or a `String`) after
   allocating the `Optional` and then write through the earlier `ObjectRef` — the
   native-stale-local family — exactly as the code they replace did, and exactly
   as `http_client.rs:1630` does. H1 and H7 join that pattern; H8 deliberately
   does not (§4). The fix is `pin_native_root` / `read_native_pin` /
   `unpin_native_roots` (the idiom `p57_alloc_enum` already uses internally),
   applied to the whole file in one change, and it is NOT folded in here because
   pinning three of eleven sites is worse than pinning none.
