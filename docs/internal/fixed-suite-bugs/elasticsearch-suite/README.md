# CratonVM — Elasticsearch full unit-test suite triage

**Suite:** Elasticsearch 9.5.0 (`apps/elasticsearch`), scope = **`libs/*` + `:server` unit tests** (2701 JUnit classes), run per-class.
**VM:** `C:\craton\CratonVM-esrun\target\release\cratonvm.exe`, built from `dev` @ `16b69363`.
**Baseline:** HotSpot JDK 25.0.1 (same classpath, flags, 600 s budget). A class is a CratonVM bug only when HotSpot completes but CratonVM crashes/hangs.
**Method:** each compiled test class run standalone via `org.junit.runner.JUnitCore <FQCN>` under both VMs, in two configs — default (JIT on) and `--nojit`. Harness + gradle trims under `apps/elasticsearch/cratonvm-suite/`.
**Date:** 2026-06-18

---

> **Getting `:server` tests to actually *pass*** is a multi-fix effort tracked in
> [EPIC-server-suite-green.md](EPIC-server-suite-green.md) — every `ESTestCase`
> hits an independent CratonVM gap at each layer of Lucene/randomizedtesting
> suite setup. 4 layers fixed (ES-FAIL-04/05/06 + the dev ES-HANG-01); chain
> continues.

## Distinct CratonVM defects

| ID | Kind | Title | Status |
|----|------|-------|--------|
| [ES-HANG-01](ES-HANG-01-lucenetestcase-suite-livelock.md) | HANG (JIT livelock) | Every `LuceneTestCase`/`ESTestCase` suite hangs in setup — JIT miscompile of `WeakHashMap$ValueSpliterator.tryAdvance`. | ✅ **FIXED on dev** (`1cd0ab26`, = kafka-bug-C); suite binary `16b69363` predated it. Confirmed independently here. |
| [ES-FAIL-04](ES-FAIL-04-arraylist-sublist-toarray-missing.md) | linkage (synthetic gap) | **Dominant `:server` blocker** — synthetic `ArrayList.subList().toArray(T[])` missing → `NoSuchMethodError` on ~every `ESTestCase`. | ✅ **FIXED** (`fix/es-fail-04-arraylist-sublist-toarray`, `ec979daf`); byte-identical to HotSpot. Not yet merged. |
| [ES-HANG-02](ES-HANG-02-restclient-integ-http-server.md) | HANG (socket/HTTP) | `RestClient*IntegTests` hang against an embedded HTTP server (not Lucene, not JIT). | **HANDOFF** (socket layer, ~3 classes). |
| [ES-FAIL-03](ES-FAIL-03-RETRACTED-nativeaccess-not-a-bug.md) | ~~exception-dispatch~~ | ❌ **RETRACTED** — misdiagnosis. The native-access `catch (LinkageError)` works fine on CratonVM (logs "Unable to load native provider" → Noop, same as HotSpot). | n/a |

**Residual (open, not yet a separate doc):** with ES-FAIL-04 fixed, server `ESTestCase` suites *execute* but some still end with a suite-level `RandomizedRunner` failure (`1) <ClassName>`, no method) — a downstream issue (likely thread-leak / `@AfterClass`), needs its own investigation. No genuine hard crashes (SIGSEGV/panic) were found in 2701 classes; the `rc=127` "crashes" were a parallel-load artifact of ES-FAIL-04.

---

## Run order of the investigation (for context)

1. Default (JIT-on) run: ~all `ESTestCase` classes HANG (ES-HANG-01). Non-`LuceneTestCase` tests (`client/rest`, some `libs`) run.
2. `--nojit` run: gets past the JIT hang; ~all `:server` then fail at the `ArrayList.subList().toArray(T[])` `NoSuchMethodError` (ES-FAIL-04). The native-access EIIE seen in logs is caught/benign (ES-FAIL-03 retraction).
3. Fixes: ES-HANG-01 already on dev; ES-FAIL-04 fixed here.
