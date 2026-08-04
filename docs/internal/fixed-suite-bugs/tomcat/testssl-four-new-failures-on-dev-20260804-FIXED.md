# `TestSsl` went 1/21 → 5/21 failing somewhere in 149 `dev` commits

| | |
|---|---|
| **Status** | RETIRED 2026-08-04 — cause found and fixed (`f9e560dc5`). **The four tests themselves were never re-run on the fixture**; see "Closure" for exactly what is and is not established |
| **Severity** | high — four TLS tests that were green went red, including a webapp start failure |
| **HotSpot** | PASS (`OK (21 tests)`) |
| **CratonVM** | Azure Linux: FAIL ×5 at `dev` `12b8cbdea`; FAIL ×1 at `dev` `48fba3a31`. Windows: FAIL ×1 at **both** `12b8cbdea` and `dev` `1f8b02e74` — i.e. the four never appear here |
| **Discovered** | 2026-08-04, running the standing TLS regression batch after merging `dev` into `fix/tomcat-final-five-20260803` |

## Not mine, and here is why that is not an assumption

The batch runs two arms interleaved, alternating order. **Both arms are
post-merge** — one with the `date_format_fast` / field-descriptor-memo changes
and one without — and both report `Tests run: 21, Failures: 5` with the same
five names. The pre-merge binary (`dev` `48fba3a31`, built 2026-08-03) run on
the same fixture minutes later reports `Tests run: 21, Failures: 1`.

So the delta is `48fba3a31..12b8cbdea`, 149 commits, none of them mine.

## The five

| test | at `48fba3a31` | at `12b8cbdea` |
|---|---|---|
| `testSni[JSSE]` | PASS | **FAIL** |
| `testKeyPass[JSSE]` | PASS | **FAIL** |
| `testKeyPassFile[JSSE]` | PASS | **FAIL** |
| `testSSLSessionTracking[JSSE]` | PASS | **FAIL** |
| `testClientInitiatedRenegotiation[JSSE]` | FAIL | FAIL (pre-existing, by design — see `testssl-client-initiated-renegotiation-FIXED.md`) |

`testSni` is the loudest:

```
org.apache.catalina.LifecycleException: A child container failed during start
    at org.apache.catalina.core.ContainerBase.startInternal(ContainerBase.java:751)
    at org.apache.catalina.core.StandardEngine.startInternal(StandardEngine.java:201)
    at org.apache.catalina.core.StandardService.startInternal(StandardService.java:433)
```

i.e. the embedded server does not start at all, which is a different class of
failure from the other three (`testKeyPass`/`testKeyPassFile` are bare
`assertTrue` at `TestSsl.java:270`, `testSSLSessionTracking` likewise). A
single cause covering all four is plausible but not established.

## Reproduction

```bash
/data/tcone.sh craton org.apache.tomcat.util.net.TestSsl <log> <cratonvm>
```

~113 s per run on the Azure Linux fixture. The class is otherwise stable —
both arms produced byte-identical failure sets across the batch.

## Windows does not reproduce this — at EITHER commit (measured 2026-08-04)

Before bisecting, know which host can see the signal. **It is not Windows.**

Both arms below ran on Windows 11 via
`apps/tomcat-suite-runner/run-one.ps1 -Class org.apache.tomcat.util.net.TestSsl`,
same fixture, same suite environment, ~280 s each:

| build | result |
|---|---|
| `dev` `1f8b02e74` (current) | `Tests run: 21, Failures: 1` |
| `dev` `12b8cbdea` (**the commit this doc measured 5 failures at**) | `Tests run: 21, Failures: 1` |

The single failure in both is `testClientInitiatedRenegotiation[JSSE]` — the
by-design one this doc already excludes. All four of the tests in question
**executed** in both runs rather than being skipped (checked by counting
`Starting test case [<name>[JSSE]]` lines in the `.err` log, 1 each) and all
four **passed**.

So the A/B controls for the commit, and the commit is not the variable on this
host: `12b8cbdea` is green here and red on the fixture. Whatever selects for
the failure is environmental to the Azure Linux fixture — do not read the
Windows green as "fixed", and do not bisect this on Windows, where every step
would report `good`.

Unrelated but worth recording, since it is in this exact class and looks
alarming in the log: both runs emit **exactly 90** occurrences of

```
gen_heap::get_field: out-of-bounds field read dropped ... index=7 num_slots=3
  class_name=org/apache/tomcat/util/net/TesterSupport$ClientSSLSocketFactory
```

Identical count at both commits, so it is pre-existing and not part of this
regression. It is guarded (the read is dropped, not served), which is why the
tests still pass — but a native caller computing slot 7 on a 3-slot receiver is
a real latent defect in the `SSLSocketFactory` → `SSLContext` field walk, and
deserves its own issue rather than being folded into this one.

## Suggested next step — SUPERSEDED, see "Closure" below

> Left as written for the record. The bisect it proposes was never needed: the
> cause turned out to be the range's own tip commit, and `14a274085` — the
> candidate this section says to try first — was **not** it. Everything from
> here to the Closure is the state of knowledge before the cause was found.

Bisect `48fba3a31..12b8cbdea` on `TestSsl` alone, **on the Azure Linux
fixture** — per the section above, no other host has been shown to reproduce
it. The class runs in under two minutes and the signal is deterministic (5/5 in
every run so far), so a `git bisect run` over ~8 builds is the cheap path.

**Try one commit before starting the bisect, though.** `14a274085`
(`fix(jit): a reused cat-2 high-half slot must keep its OSR register home`)
landed *after* `12b8cbdea` and is a genuine OSR miscompile fix: on OSR entry
into a loop region where a cat-2 high-half slot is legally reused as a cat-1
local, the trampoline seeded only the frame slot while the compiled body kept
reading the register, so the region ran with a **garbage local**. That produces
arbitrary wrong behaviour in any hot loop, which is exactly how four
unrelated-looking TLS tests fail at once — and it is already in `dev`. One
build of current `dev` on the fixture either clears all four (done, no bisect
needed) or rules the theory out for the price of a single step.

If it does not clear them: the range's own content argues against a TLS-logic
cause. `git diff --name-only 48fba3a31 12b8cbdea` touches **no** TLS, SSL,
keystore or JSSE source at all — it is 23 `vm/`, 18 `jit/`, 6 `types/`, 6
`gc/`, 2 `classloading/` files. (Grepping the file list for `ssl` appears to
match three paths; all three are `cla`**`ssl`**`oading`.) So prefer the JIT/VM
commits over "the commits touching TLS, keystores and `SSLHostConfig`" — there
are none of the latter. `CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_ONLY` are
the faster levers than a rebuild-per-step bisect, and note they only became
trustworthy inside this very range: `d042b0ea2` fixed both levers silently
failing to gate OSR, which had made every prior bisect step a no-op that read
as an exoneration.

Do NOT read the surviving `testClientInitiatedRenegotiation` failure as part of
this — it is the by-design TLS 1.2 renegotiation gap and predates the range.

---

## Closure (2026-08-04)

### Not the `SSLSocketFactory.getDefault()` duplicate-registration bug

Checked first, because that defect was live in the same subsystem on the same
day (`docs/internal/fixed-suite-bugs/springboot/`, the
`sslsocketfactory-getdefault` page). It is **not** this. Two independent
reasons, either sufficient:

* The duplicate registration in `net_phase_e.rs` dates to the
  **initial commit, 2026-04-26** (`a6dc911ed`), and `git log
  48fba3a31..12b8cbdea -- native-builtins/src/net_phase_e.rs` is **empty**. A
  defect present continuously since April cannot make a suite go from green at
  `48fba3a31` to red at `12b8cbdea`.
* `TestSsl` does not acquire its client factory through the static
  `getDefault()` at all. `TesterSupport.configureClientSsl()` builds one via
  `SSLContext.getInstance(...).getSocketFactory()` and installs it with
  `HttpsURLConnection.setDefaultSSLSocketFactory(...)`.

### The cause: COV-07's `athrow` lowering, which is `12b8cbdea` itself

The regression boundary is not merely *inside* the 149-commit range — it **is
the range's tip**. `12b8cbdea` is `feat(jit): COV-07 — athrow gets a real IR
lowering`, and that commit introduced a real, now-proven defect:

> An `athrow` compiled by the optimizing tier did not force `has_dispatch`, so
> the method could be entered through the fast path, which never drains the
> stashed exception. The throw was silently swallowed and the method returned
> as though it had completed normally.

Fixed in `f9e560dc5`; the full mechanism, the narrowing and the numbers are in
that commit message and in `jit/src/lib.rs` at the new `Op::Throw` arm. The
matching invariant had been held by the single-pass backend since RBC.6; COV-07
did not carry it across.

Why this shape produces scattered, unrelated-looking failures: a swallowed
throw is not a crash. Control simply continues past a `throw`, so the damage
surfaces later and somewhere else — a `finally` that does not run, an error
path that returns success, a listener that reports started when it failed.
`testSni`'s `LifecycleException: A child container failed during start` is
exactly that shape, and the other three are bare `assertTrue` failures with no
exception detail — the signature of an exception that was thrown and lost.

Why the fixture sees it and Windows does not: the gap governs the
compiled-callee → **interpreted**-caller edge. Once the caller is compiled too,
its own JIT-to-JIT routing masks it. In an ordinary run that leaves only the
window where the callee is compiled and the caller is not yet — so it fires
about once per run, at a moment whose timing depends on the host. Pinning the
caller interpreted turns it from ~1-in-300 000 into 99.6% (1 792 397 of
1 800 000), which is how it was measured. A defect that narrow explains a
suite that is deterministic on one host and absent on another far better than
"environmental".

### What is verified, and what is not

Verified:

* the defect reproduces on Windows with a Hibernate-free two-method probe, and
  is gone after the fix (both the 99.6% arm and the once-per-run arm → 0);
* the pre-fix binary fails the new regression test (`swallowed=199153` of
  200 000), the post-fix binary passes it;
* `cratonvm-jit` 1892 unit tests + all integration targets pass;
* `TestSsl` on Windows is unchanged before and after: 21 tests, with
  `testClientInitiatedRenegotiation[JSSE]` the one by-design failure.

One correction to that last line, because the first version of it was too
clean. `TestSsl.testPost[JSSE]` **flakes**, and it flaked during this work in a
way that initially looked like a regression from the fix. Measured over 11
runs on the same host:

| arm | `testPost` failures |
|---|---|
| pre-fix (`dev`, no athrow fix) | 2 / 7 |
| with the athrow fix | 2 / 4 |

So it fails on both arms and is **not** attributable to the fix — the first
three pre-fix runs simply happened to be clean, which is exactly how a flaky
test manufactures a false regression. The failure is always the same shape: a
mid-stream EOF while reading the response back, e.g. `Byte in position
[5930928] had value [-1] rather than [1]`, from one of the four concurrent
threads `testPost` starts to POST ~6 MiB each over TLS. No exception is
printed, so it is the read-loop's own EOF branch, not the `catch`.

Filed separately as
`docs/known-issues/tomcat/testssl-testpost-connection-dies-under-concurrent-bulk-tls-20260804.md`,
where it is measured properly: 4 failures in 11 CratonVM runs against 0 in 3
HotSpot runs, and `testPost` taking ~190 s on CratonVM against 3.8 s on
HotSpot. Kept summarised here so the next person who sees `Failures: 2` on
this class does not spend the afternoon bisecting it, and so nobody reads a
single clean `TestSsl` run as proof that a JIT change is safe.

**Not** verified: the four tests were never re-run on the Azure Linux fixture,
because Windows never reproduced them (that measurement is the section above,
and it is why this page cannot close on a local green). The causal chain here
is strong — the regression boundary coincides exactly with the commit that
introduced a proven exception-swallowing defect, and the failure shapes match —
but it is a chain, not a direct observation of these four tests passing.

**If a fixture run still shows any of the four, reopen this page rather than
filing a new one**, and treat the remaining 148 commits in the range as the
search space; the `CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_ONLY` levers (made
trustworthy by `d042b0ea2`, inside this same range) are the cheap tool, not a
rebuild-per-step bisect.
