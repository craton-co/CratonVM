# `TestSsl` went 1/21 → 5/21 failing somewhere in 149 `dev` commits

| | |
|---|---|
| **Status** | OPEN — not bisected. **Azure-Linux-fixture-only: Windows does not reproduce it at either commit** (measured 2026-08-04, see below) |
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

## Suggested next step

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
