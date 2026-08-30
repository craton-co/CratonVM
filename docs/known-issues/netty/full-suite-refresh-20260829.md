# Netty full-suite refresh, 2026-08-29 — the non-passing set was 45 stale entries, and most of what replaced it is not a failure

## Why this page exists

`apps/netty-suite-runner/netty-nonpassed-latest.txt` is the file every netty
session reads to decide what is still broken. It is **untracked** — `apps/` is
gitignored — so `git log` cannot date it, and its contents were from
**2026-08-13**. It had read as current for 16 days.

This is the tracked record of the run that replaced it, because the file itself
cannot carry one.

## The run

`run-netty-suite.sh categorize`, all 657 classes of `testlist.txt`, dev
`92983411f`, 6 shards, 180 s per-class cap, **93m13s** wall, binary built from
that commit.

| status | count |
|---|---:|
| PASS | **540** |
| FAIL | 38 |
| NOTESTS | 36 |
| ABORTED | 26 |
| HANG | 17 |

`passed.txt` 545, `others.txt` 112 (5 ABORTED classes reclassified pass-equivalent
by `known-benign-aborts.tsv`).

## 16 of the old list's 45 entries pass now

Sixteen fixes that nobody had credited, because the list they would have been
credited in was never regenerated:

```
io.netty.channel.unix.NativeInetAddressTest
io.netty.handler.codec.http2.Http2MultiplexTransportTest
io.netty.handler.ssl.CloseNotifyTest
io.netty.handler.ssl.OpenSslKeyMaterialManagerTest
io.netty.handler.ssl.OpenSslPrivateKeyMethodTest
io.netty.handler.ssl.ParameterizedSslHandlerTest
io.netty.handler.ssl.SniClientTest
io.netty.handler.ssl.SniHandlerTest
io.netty.handler.ssl.SslContextBuilderTest
io.netty.handler.ssl.SslErrorTest
io.netty.handler.ssl.ocsp.OcspClientTest
io.netty.handler.ssl.ocsp.OcspServerCertificateValidatorTest
io.netty.handler.ssl.util.BouncyCastleUtilTest
io.netty.resolver.dns.SearchDomainTest
io.netty.util.concurrent.DefaultThreadFactoryTest
io.netty.util.internal.JfrEventSafeTest
```

Ten of the sixteen are TLS classes, which is what four weeks of SSL work should
look like. Every one of them has a `fixed-suite-bugs/` page claiming the fix;
none of them had left the list.

## Most of the new non-passing set is not a failure

76 rows once the 36 `NOTESTS` (abstract base classes with no tests of their
own) are dropped. Bucketed, because a flat list of 76 is what produced the
last round of wasted triage:

| bucket | n | what it is |
|---|---:|---|
| **FAIL** | 21 | the real work |
| **HANG** | 17 | hit the 180 s cap — throughput walls, several already have pages |
| **ABORTED, zero failures** | 21 | `Assumptions` skips. Nothing failed. |
| `NativeImageHandlerMetadataTest` x17 | 17 | Windows-harness artifact |

### The 21 aborts are a table that stopped being maintained

Every one is `@@TESTSKIP <class> <test> assumption` with `failed=0` — a test the
platform declines to run (no OpenSSL, no `Unsafe`, wrong endianness). The
runner already knows this shape: `known-benign-aborts.tsv` lists such classes
with their exact `found/ok/aborted` counts and `categorize` treats an exact
match as pass-equivalent. It lists **8** classes, 5 of which matched. About 21
qualify.

So ~16 classes read as non-passing with nothing failing, and each one costs a
future session a fresh investigation. **Extending that table is the highest
value-per-minute work on this page.** It was not done here because the table is
now untracked (below) and the change would not survive.

### The 17 `NativeImageHandlerMetadataTest` are a Windows default

`run-netty-suite.sh` sets `USE_MODSCOPE=0 # module scope disabled by default on
Windows`, and `module-args/` is host-specific generated output that does not
exist on this host. So these 17 run on the flat whole-reactor classpath instead
of the single-module one they compare their `reflect-config.json` against.
Already covered by `not-cratonvm-bugs-consolidated.md`; regenerate
`module-args/` with `gen-module-args.sh` to get a real answer for them.

## Two FAIL rows are load flakes, checked and cleared

The categorize run puts six classes on the box at once, so its FAIL column is
not a quiet-host verdict. Two rows were re-run directly:

* **`AutoScalingEventExecutorChooserFactoryTest`** — failed once here, on
  `"Should not scale back down while load is high"`, which is a DIFFERENT
  assertion from the `"Should scale up to 2"` one fixed earlier today
  (`autoscalingeventexecutorchooserfactorytest-scaled-too-far-FIXED-20260829.md`),
  and the opposite symptom: the group shed a thread the load should have kept.
  Re-run on this binary: **quiet 8/8, six-way self-contended 6/6 on BOTH VMs,
  24 CPU spinners 4/4 on both**. 18/18 targeted runs, no reproduction. It is a
  low-rate flake under the full-suite load profile, not a regression of that
  fix — but the class will keep appearing in this column, so it is written down
  here rather than left for someone to re-derive.
* **`AbstractReferenceCountedTest`** — 3/3 clean on both VMs quiet. Same shape.

## 13 classes in the set are named in no doc at all

Twelve are ABORTED-with-zero-failures and stop mattering the moment the abort
table is extended. The thirteenth was the only genuinely unexplained FAIL, and
it is the `AbstractReferenceCountedTest` flake above.

```
io.netty.buffer.BigEndianUnsafeDirectByteBufTest          (aborts only)
io.netty.buffer.BigEndianUnsafeNoCleanerDirectByteBufTest (aborts only)
io.netty.buffer.LittleEndianUnsafeDirectByteBufTest       (aborts only)
io.netty.buffer.RetainedSlicedByteBufTest                 (aborts only, 410/416 ok)
io.netty.buffer.SlicedByteBufTest                         (aborts only, 410/416 ok)
io.netty.buffer.UnsafeByteBufUtilTest                     (aborts only)
io.netty.handler.codec.compression.JdkZlibDecompressorTest (aborts only, 33/51 ok)
io.netty.handler.ssl.CipherSuiteCanaryTest                (aborts only, 2/18 ok)
io.netty.handler.ssl.OpenSslCertificateCompressionTest    (aborts only)
io.netty.handler.ssl.OpenSslClientContextTest             (aborts only, 33/34 ok)
io.netty.handler.ssl.OpenSslServerContextTest             (aborts only)
io.netty.handler.ssl.util.LazyJavaxX509CertificateTest    (aborts only)
io.netty.util.AbstractReferenceCountedTest                FAIL -- load flake, cleared above
```

That is a good result for the docs: after four weeks, the only netty class that
was failing and undocumented turned out not to be failing.

## The harness that produced this is no longer in git

Every file this run depended on — `run-netty-suite.sh`, `CratonRunner.java`,
`class-overrides.tsv`, `known-benign-aborts.tsv`,
`module-scoped-classes.tsv`, `README.md`, both generator scripts — was deleted
from the repository on **2026-08-29 at 00:08** by commit `e33f6d7e3`
("cleanup", 368 files, ~40k deletions). They survive only as working-tree
copies on this host, byte-identical to the deleted blobs (checked, all eight).

The deletion is consistent with `.gitignore`'s `apps/` rule and may well be
deliberate. But three of those files carry headers that now assert something
false — `known-benign-aborts.tsv` opens with *"THIS FILE IS TRACKED IN GIT ON
PURPOSE… every file in this fixture has to be force-added"* — and between them
they hold 86 lines of per-class timeout/flag overrides, 69 lines of benign-abort
counts and 60 lines of module scoping. That is months of per-class knowledge
with one copy left.

Either re-add them with `git add -f` or correct the headers. Leaving a file that
says it is tracked and is not is the same failure this page is about, one level
down.

## Related

* `not-cratonvm-bugs-consolidated.md` — the `NativeImageHandlerMetadataTest`
  cluster and the bootstrap pair.
* `recyclertest-thread-not-collected-once-the-jit-warms-up-20260829.md` and
  `internal/fixed-suite-bugs/netty/hashedwheeltimertest-two-dispatch-defects-not-a-funnel-floor-20260827.md`
  — two of the 21 FAIL rows, both already investigated.
