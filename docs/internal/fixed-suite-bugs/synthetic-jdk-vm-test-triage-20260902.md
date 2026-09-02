# Triage: the 30 failures the synthetic-JDK test module had been hiding

`vm/src/vm/tests.rs` is gated `#[cfg(all(test, feature = "synthetic-jdk"))]` and
had not compiled since `InlineSite` grew an `ir_new_info` field — one missing
initializer entry took **4291 tests** out of the build, and
`cargo test -p cratonvm-vm --lib <name>` printed `0 passed; 0 failed`, which
reads exactly like a clean run. The iterator-carrier census fixed the
initializer; this is the triage of what came out.

**30 failures → 12.** Eighteen were stale by construction; the twelve that
remain are real questions.

## The finding behind most of them

These tests assert that native STUBS exist. The tree has spent months deleting
them, for a reason its own commits state plainly:

> A registered native SHADOWS the Java bytecode for its class+method+descriptor,
> so a constant-returning handler does not "leave a method unimplemented" — it
> silently replaces a working method with one that does nothing.
> — `077c84a18`, the wave-3 stub sweep (669 → 462)

So the tests encode the OLD policy, and each retirement left its paired test
behind. Every one of those retirements ran a consumer sweep and none of them
found these tests, **because the module did not compile** — including
`b81aae8fc`, whose sweep is written out as "no Rust caller, no second
registration, no synthetic-jdk corpus call".

## Stale — retired (16)

| tests | retired by | why it was right |
|---|---|---|
| `cyclic_barrier_*` ×4 | real-AQS default | the natives register only in synthetic-AQS mode or at runtime under `--synthetic-jdk`; the harness builds neither, and the real JDK class is correct |
| `m3_predicate_{and,or,negate}`, `m3_consumer_and_then` | `b81aae8fc` 2026-08-20 | `javap`: none of the seven `java.util.function` stubs is `ACC_NATIVE`, all have real bodies |
| `p86_thread_group_*` ×3, `thread_group_basics_p71` | `80d60e911` 2026-08-23 | a FIX, not a removal — `activeCount()` was a VM-WIDE count ignoring its receiver; 17 checks, identical to HotSpot after |
| `input_stream_read_all_bytes_p72`, `sb_repeat_string_p64` | `077c84a18` and later waves | constant-valued stubs shadowing working bytecode |
| `http_version_enums_p60`, `http_redirect_enums_p60` | — | enum CONSTANTS are not populated here; see the cluster below |

`#[ignore]`d with a per-group reason rather than deleted: each one's assertions
become right again if its native is ever reintroduced.

## Stale test DATA — fixed (1)

`system_get_property_native` asserted `java.version == "25.0.1"` exactly, and
went red when the host JDK became 25.0.3. **A test that must be edited on every
JDK update pins nothing about this VM.** Now asserts the `25.` line it targets.

## Live — 12, handed off

Not investigated here; this is the classification, with the failure signature
that a reader would otherwise have to rebuild a 4291-test module to see.

```text
enum-constant cluster (2, and 2 more ignored above)
  countdown_latch_await_timeout   NPE: Cannot invoke "TimeUnit.toNanos(long)"      <- constant is null
  enum_map_put_get_size           NPE: Cannot invoke "Class.getEnumConstantsShared()"

null-shaped (4)
  basic_file_attributes_p59       NPE, no message
  optional_or_present_returns_self NPE, no message
  log_manager_p61                 NPE: "Object.hashCode()" because "key" is null
  proxy_new_instance_stores_handler NPE: array length because "interfaces" is null

wrong value (2)
  async_socket_channel_p67        Int(0) vs Int(1)
  object_output_stream_p70        Int(0) vs Object(None)   <- kind mismatch, not just value

Scanner (2)
  scanner_close                   NoSuchMethodError java/util/Enumeration$Impl.close()V
  scanner_from_bais               NoSuchElementException "no more elements"

other (2)
  hex_format_from_hex_digits_p64  IllegalArgumentException "string length greater than 8: 55"
  u6_datagram_channel_connect_disconnect  IOException, message is a Windows error string
                                  in the host locale — environment-specific, verify elsewhere
```

**The enum-constant cluster is the one to take first.** Four tests point at it —
two ignored above (`http_*_enums_p60`, whose constants are "not registered") and
two live (a null `TimeUnit` constant, and `getEnumConstantsShared` on a null
class). That is one defect wearing four faces, and fixing it closes two of these
twelve and un-ignores two more.

## Flaky, not counted

`string_reader_skip_and_reset` failed the first full run (`Int(108)` vs
`Int(65)` — `'l'` vs `'A'`, so a skip/reset landing at the wrong offset) and
passed the next two. Recorded because a 1-in-3 failure in a module nobody could
build would otherwise be discovered as "intermittent" by whoever fixes the rest.

## The lesson worth keeping

**A test module that does not compile reports zero failures**, and a `--lib`
name filter prints a green `0 passed; 0 failed` for a target that never built.
Every retirement above ran a consumer sweep that could not see its own paired
test. A sweep that greps the source finds callers; it does not find callers that
would not compile.
