# CratonVM — Hibernate ORM full-suite census, dev run 2026-06-17

Full Hibernate ORM 8.0 test suite (4533 classes) under CratonVM built from **dev tip `19fd6707`**
(branch `suite-dev-run` in worktree `CratonVM-hibrun`), vs the JDK-25 HotSpot baseline.
Mode: fork-per-class (`BATCH=1`, precise crash/hang attribution), **JIT-off** interpreter, **600s** per-class
hang timeout. Harness: `.cratonvm-suite/run-devrun.sh` → `run-sharded.sh` (10 shards).

> ⚠️ Run was under heavy CPU contention (concurrent peer session). Crashes (SIGSEGV/rc=1) are reliable;
> a few large hangs may be slow-not-hung — see per-cluster watchdog dumps.

## Status totals (partial — census ~58% at time of writing; will be finalized on completion)

| status | classes | notes |
|---|---:|---|
| PASS | ~2214 | |
| FAIL | ~62 | wrong-result assertions (86 individual test methods) |
| HANG | 22 | grouped below |
| CRASH | 12 | all categorized below |
| LOADERR | 264 | mostly the shared `BytecodeEnhancedTestEngine is disabled` harness gate (HotSpot has the same) — **not** CV bugs |
| NOTESTS | 45 | |

Individual-test level (so far): ~7178 found, ~6684 pass, ~86 fail.

---

## Crashes (12) — all categorized

### JSON-function SIGSEGV (4) — ✅ FIXED & verified
`function.json.JsonExistsTest`, `JsonQueryTest`, `JsonTableTest`, `JsonValueTest` — `rc=139` SIGSEGV.
Root: `al_state` read ArrayList slots off a non-ArrayList operand → garbage `Value` tag → wild jump-table
read. Fixed (`native-collections/src/lib.rs` layout guard); all 4 now pass == HotSpot.
→ [JSON-function-sigsegv-al_state-foreign-receiver.md](JSON-function-sigsegv-al_state-foreign-receiver.md)

### JTA / transaction (8) — 🟡 crash fixed, deeper hang open (handoff)
`actionqueue.JtaCustomAfterCompletionTest`, `connections.AggressiveReleaseTest`,
`connections.CurrentSessionConnectionTest`, `idgen.foreign.ForeignGeneratorJtaTest`,
`interceptor.InterceptorJtaTransactionTest`, `jpa.transaction.CloseEntityManagerWithActiveTransactionTest`,
`jpa.transaction.TransactionJoiningTest`, `jpa.txn.JtaTransactionJoiningTest` — `rc=1`.
Root: `ServerSocket.getInetAddress()` null → Narayana `TxControl.<clinit>` NPE. `getInetAddress` fixed; a
deeper Narayana XA transaction-completion + socket-loopback hang remains (handoff).
→ [JTA-txcontrol-clinit-gethostaddress-null.md](JTA-txcontrol-clinit-gethostaddress-null.md)
→ known-issue: [docs/known-issues/hibernate-jta-narayana-xa-completion-and-socket-loopback.md](../../../../docs/known-issues/hibernate-jta-narayana-xa-completion-and-socket-loopback.md)

## Hangs (22) — clustered

→ [HANG-clusters-summary.md](HANG-clusters-summary.md)
- **H1 JAXB `retainAll` infinite loop** (XML mapping) — confirmed real. → [XML-jaxb-retainAll-infinite-hang.md](XML-jaxb-retainAll-infinite-hang.md)
- **H2 ByteBuddy `MethodGraph` proxy-factory hang** (entity bootstrap) — confirmed real stall.
- **H3 JTA / socket** — documented (same family as the JTA crashes).
- **H4 JSON unnest** — re-check post-fix; 1 environmental (`DefaultCatalogAndSchemaTest`, HotSpot hangs too).

## FAILs — investigated clusters
- **Reversed exception stack-trace order** — `getStackTrace()`/`printStackTrace()` returned frames upside-down. ✅ **FIXED + committed**. → [FAIL-throwable-stacktrace-order-reversed.md](FAIL-throwable-stacktrace-order-reversed.md)
- **Deserialized SessionFactory null (5 classes)** — Hibernate SF reconnection (`SessionFactoryRegistry`) fails after deser; deeper, open. → [FAIL-deserialization-sessionfactory-null.md](FAIL-deserialization-sessionfactory-null.md)
- Weld CDI `IllegalArgumentException` (11) = known **HIB-CV-20**; assorted `AssertionError`/`NPE`/`ISE`/`ExceptionInInitializerError` (JTA-adjacent) still to triage.

## Fixes committed to `dev` this run (verified)
1. `1231b07b` `native-collections/src/lib.rs` — `al_state` ArrayList-layout guard (JSON SIGSEGV). ✅ 4 JSON tests pass on dev binary.
2. `ada6cebf` `native-builtins/src/net_phase_e.rs` — `ServerSocket.getInetAddress()` native (JTA crash). ✅ removes crash.
3. `c3867a4e` `native-builtins/src/lang_misc.rs` — Throwable stack-trace order (was reversed). ✅ `getStackTrace()[0]` == throw site, no StackWalker regression.
4. `13e8c761` `native-builtins/src/locale_bootstrap.rs` — `Locale.toLanguageTag()` dropped all subtags for real Locales (`new Locale("","DE")` → `und` instead of `und-DE`). ✅ `LocaleJavaTypeDescriptorTest` 0→4/5.

## Narrow open items found while grinding FAILs
- `LocaleJavaTypeDescriptorTest`'s last method: `Locale.Builder.setExtension(char,String)` throws `ClassCastException: Integer cannot be cast to Character` — a `char→Character` box mishandled in JDK `InternalLocaleBuilder`/`LocaleExtensions` interpreter path (general `char` autoboxing is fine; setExtension-specific). Repro `jsonrepro/CharBox.java`. Deep/narrow (locale extensions rare); empty exception stack hampers pinpointing.
- `engine.action.{Sorted,NonSorted}ExecutableListTest` — Hibernate custom sorted `ExecutableList` order assertions fail (sort/insert path); next candidate.
