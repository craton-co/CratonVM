# INDEX — every record in `docs/known-issues/jdk-only/`

**Built by lane C18, 2026-08-13.** Snapshot: `ls docs/known-issues/jdk-only/*.md`
taken at **00:07 local on 2026-08-13**, **155 files** including this one, on
branch `claude/jdk-only-mode-completion-1351c0` at `HEAD = 7c00dee66`
(records land untracked, so `git ls-files` and `ls` disagree here by design).

> **This index is a snapshot and it will rot.** Records were still landing from
> lanes C14, C15, C16, C17 and C19 while it was being written — five arrived
> *during* this lane's own pass. If a file exists that is not listed here, it is
> newer than this snapshot, not a stray. Re-take the listing before trusting the
> count. Do not carry the number 153 forward without recounting; that is exactly
> the mistake `README.md`'s own headline documents.

## How to read the columns

**Status** — derived by **reading each record's own status prose**, never from
its filename or its title. Records state status a dozen different ways
(`**Status: OPEN`, `Status: the`, a blockquote banner, a table row, or plain
prose in the opening paragraph), so a grep would have missed roughly a fifth of
them.

| status | means |
|---|---|
| `OPEN` | live defect, or partly fixed with a named live residual. **Partly-fixed records are listed OPEN**, with `partial` in Notes — a half-closed record is a trap if it reads as closed. |
| `FIXED-UNVERIFIED` | the fix is in source; **no binary carrying it has been built or run** |
| `FIXED-MEASURED` | verified by running a binary that carries the fix |
| `SUPERSEDED` | the work moved to another record; kept for its history |
| `META` | not a defect record — handoff, census, retirement audit, run report, queue |

**Prov(enance)** — where the numbers came from. This column exists because
wave-C lanes were forbidden to build, so most CratonVM "after" values in this
directory are **predictions**, and a reader must not mistake one for a
measurement.

| prov | means |
|---|---|
| `MEAS` | executed against a CratonVM binary |
| `PRED` | CratonVM column is predicted; nothing was built or run |
| `SRC` | source reading only (still stronger than a prediction, weaker than a run) |
| `MIXED` | typically: "before" executed on an old binary, "after" PREDICTED; or HotSpot arm executed, CratonVM arm predicted |

**Nothing in this pass upgraded a PREDICTED value to measured.** The only
records marked `MEAS` for a *correction* are the eight facts listed in
"Corrections applied" below, each of which was executed by the orchestrator.

---

## Corrections applied 2026-08-13 (lane C18) — the facts that moved

Each was measured or source-verified elsewhere and was still being asserted the
old way somewhere in this directory. The record now carries a `RECONCILED
2026-08-12/13 (lane C18)` banner naming the source.

| fact | now | where it was still wrong |
|---|---|---|
| `Math.floorDiv`/`floorMod` at `MIN_VALUE / -1` aborting the VM | **FIXED, MEASURED on a real binary** | `W7-95` headline + §, `W7-99` §, `W8-C3-1` §prediction |
| the six "VM-fatal" intrinsic triples | **no family aborts the VM; every second-generation-census failure is a Java `AssertionError`. MEASURED** | `W7-95` headline, `W8-C3-1` headline |
| `Math.pow` | **TWO items.** Special values FIXED + MEASURED; the **fast path on ORDINARY inputs** (`a.powi(b as i32)`) measured at **1.4 ulp at \|b\|=2, 44.3 ulp at \|b\|=63 against a 1-ulp contract** — separate, and not closed | `W7-99` (treated pow as one item), `W7-95` closure table |
| `Math.ulp` | **NOT a defect.** Proved equivalent over **all 4,294,967,296 `float` bit patterns**. MEASURED | `W7-95` divergent-triple list + closure table, `W7-99` §2 |
| "645 triples registered" | **645 registry ROWS = 614 DISTINCT triples** (31 duplicate registrations). Coverage is 258/614 = **42%**; never-invoked is **356** distinct, not 387 | `W7-95` ×2, `STUB-CENSUS` ×2 |
| corpus `DIVERGE` counts from a `junit`-kind corpus | **62 of 75 stored rows (83%) were harness artefacts** (`C8` §4.1, replayed row-for-row by `C17`). `h2` is a `main`-kind corpus and correctly did not move — 0 of 3 | `P4A-CORPORA`, `W7-92` §0 |
| `SimpleTimeZone` skew for `America/Sao_Paulo` | predicted `10,800,000` ms, **measured `7,200,000`** — São Paulo was in DST on 2002-01-22 | `C6-2` §D. **`P4A-CORPORA`'s `10800000` is a different, correct quantity** — see the contradiction note below |
| shutdown hooks | **MEASURED never to run.** HotSpot prints three hook lines, CratonVM none, **with no output-lost marker** — so "the hook ran and its output was lost" is ruled out. The *fix* is still unverified | `W7-92` (hedged), `WAVE-D-QUEUE` measurement 3 |
| `register_queue_deque_interface_natives` | **`:38096`, 23 rows** — not `:37787`, not 18. Four rows come out of a `for` loop, so counting `registry.register(` sites by grep undercounts. **SOURCE-VERIFIED, not measured** | `C7-2`, `P2` |
| `register_interface_natives` | **28773–28991, 38 registrations**; its own in-file header comment saying "these 23" is stale. **SOURCE-VERIFIED** | `C7-2` (opens-at figure), `P2` §2.2 |
| the `HashMap$Values` "interface door wins for inherited methods" hazard | **DOES NOT EXIST.** The native-above-receiver walk follows `superclass` only and never enumerates interfaces. **SOURCE-VERIFIED** (`invoke.rs:3281-3323` + two mirrors, `C13-1` §1.1). It had been briefed as "the highest-risk item" | `C7-1`, `C7-2` §4 row A2 |
| the collections census direction | retiring 8 `ArrayList` rows while adding the ~25 real view-class rows is **NET +17**. Correctness here **costs** registrations | `P2` title/framing, `C7-1` |

### Number collision, resolved

Two records both claimed `W7-39`. The **older** is
`W7-39-jca-missing-algorithms.md` (created 2026-08-12 00:09:45 −0300); the
**newer**, `W7-39-aastore-interface-component-blanket.md` (23:24:35 −0300), was
renumbered to **`W7-101`** — the first number above the highest allocated
(`W7-100`), chosen over the free low gaps (`W7-4`, `-6`, `-7`, `-11`, `-13`,
`-28`, `-32`, `-43`, `-45`, `-47`, `-48`, `-52`, `-59`, `-67`, `-82`) because
**all but three of those are still cited by live records** and reusing one would
manufacture a second collision. `W7-101` was checked unreferenced before use.

**13 cross-references rewritten** in 2 files (`W8-C10-1` ×11, `W8-C16-1` ×2).
Every remaining `W7-39` in this directory (README, `RETIREMENT-20260812B`,
`W7-15`, `W7-21`) refers to the JCA record and is correct.

**Two references live in Rust and could not be touched by this lane — see
NOMINATIONS at the bottom of this file.**

---

## Meta / deliverables — not defect records

| record | subject | status | prov | notes |
|---|---|---|---|---|
| README | index and standing rules for this directory | META | MIXED | headline record count is self-admittedly stale arithmetic; per-record tables ~86 rows |
| INDEX (this file) | every record, status read from the record | META | — | snapshot; recount before quoting |
| WAVE-D-QUEUE | wave-C nominations no lane applied, plus measurements owed | META | — | measurement 3 (shutdown probe) narrowed by C18 |
| HANDOFF-20260812 | first-wave orchestration handoff | META | MEAS | corrects the 70/0 suite row to 67/5 and 68/3 |
| HANDOFF-20260812B | second-wave handoff; 3,232 uncompiled Rust lines | META | MIXED | supersedes the first; nothing in that wave built |
| RETIREMENT-20260811 | audit moving 30 records out, keeping 22 | META | MEAS | carries a correction that three of its own "kept" reasons were false |
| RETIREMENT-20260812 | audit moving 4 records out, holding W6-2 | META | MIXED | |
| RETIREMENT-20260812B | second-pass audit, 8 out, 2 held | META | MEAS | |
| STUB-CENSUS-20260812 | per-registration census of stub/bridge surface | META | MEAS | **corrected**: `intrinsic=645` is ROWS = 614 triples |
| JDK-ONLY-REPORT-CENSUS-20260812 | `--jdk-only-report` is a complete, unused census | META | MEAS | census over-reports, probe under-reports |
| APP-READINESS-20260812 | what stops real Java apps under `--jdk-only` | META | MEAS | stderr banner under-reports blockers by half |
| P1-BASELINE-20260812 | phase-1 before-measurement, corrected exit criterion | META | MEAS | true worklist 57 candidates, not 3 |
| P1-RESULT-20260812 | phase-1 after: nine blocking families closed | FIXED-MEASURED | MEAS | 54/54 from 28/54 |
| P2-COLLECTIONS-SHADOWS-20260812 | per-triple adjudication of the collections slice | META | MIXED | **corrected**: net +17, both registrar ranges |
| P4B-SYNTHETIC-JDK-MODE-20260812 | synthetic-JDK mode needs its own feature build | META | MEAS | superseded by the FIRST-RUN doc |
| P4B-SYNTHETIC-JDK-FIRST-RUN-20260812 | first run of the `--synthetic-jdk` binary | META | MEAS | reports two defects it does not file |
| W7-55-record-reconciliation | status-line reconciliation across 58 records | META | MIXED | six of its own line anchors rotted the same day |
| W7-78-inherited-residual-closeout | inherited residuals closed out | META | MIXED | its own headline finding is superseded in-tree |
| W7-5-registrars-that-never-shipped | 301 registrars absent from the shipping binary | OPEN | SRC | partial; own gap count self-corrected 32→20 |
| W7-100-absent-marker-in-a-composite-key | one missing feature inflates DIVERGE verdicts | META | MEAS | method record; its 12/9/36 counts superseded by C8/C17 |
| W7-40-tier-parity-fixtures-and-fast-throw | a tier-parity fixture goes red on the oracle | META | MEAS | cause is `-XX:+OmitStackTraceInFastThrow` |

## Harness / corpus

| record | subject | status | prov | notes |
|---|---|---|---|---|
| C8-CORPUS-HARNESS-DEFECTS-20260812 | nine corpus-driver faults and the results they invalidate | OPEN | MEAS | partial; **the authority for "62 of 75 DIVERGE rows were the harness"** |
| C17-CORPUS-READJUDICATION-20260812 | all 163 stored rows re-adjudicated without the VM | META | MEAS (replay) | reproduces C8 row-for-row; finds 4 "agreements" where CratonVM ran zero tests; h2 control moved 0 of 3 |
| C8-CORPUS-TIMEOUT-TAXONOMY-20260812 | CV-TIMEOUT split into SIGNAL/STALLED/BUSY/UNKNOWN | FIXED-MEASURED | MEAS | never exercised against a real access violation |
| C8-H2-TESTBACKUP-SHARED-WORKDIR-20260812 | TestBackup flake is a shared working directory | OPEN | MEAS | not a VM defect; remedy declared by no corpus |
| P4A-FIRST-CORPUS-RUN-20260812 | first H2 corpus run, fourteen classes | META | MEAS | no `--real-jdk` control taken |
| P4A-H2-DIVERGENCES-20260812 | H2 failures with the control arm | OPEN | MEAS | timeout caps differ 200s vs 420s — every TIMEOUT row confounded |
| P4A-CORPORA-20260812 | bc-java + commons-math wired and run | OPEN | MEAS | **corrected twice**: DIVERGE counts (C8/C17) and the two different timezone quantities |
| W6-5-vacuous-tests | tests that passed without testing anything | OPEN | MIXED | partial; adds shape C — a 1-in-155 load-bearing assertion |
| W7-51-vacuous-sweep-round-2 | thirteen tests still cannot fail | OPEN | MIXED | detector recall measured 3 of 9; read §7 first |
| W7-60-harness-extract-blindness | `run.sh` `extract()` discarded vectors' evidence | FIXED-MEASURED | MEAS | one residual vector |
| W7-62-ratchets-and-dead-code | three stale ratchets, six tests guarding nothing | OPEN | MIXED | its Status line and its own top banner disagree — see contradictions |
| W7-42-differential-instrument-holes | the differential compared two class files | FIXED-MEASURED | MIXED | divergence count re-stated 14 → 9 |
| W7-30-stub-ratchet-boot-path-scope | ratchet censused 6 of 48 boot registrars | OPEN | MIXED | the 2026-08-11 fix was inert; gate now firing unfrozen |
| W7-33-differential-dead-sections | dead sections in the differential | FIXED-UNVERIFIED | MIXED | residual unobservable — no suite runs `--synthetic-jdk` |

## Math / intrinsics

| record | subject | status | prov | notes |
|---|---|---|---|---|
| W7-94-math-min-max-nan-and-negative-zero | `Math.min`/`max` violated the JLS for NaN and −0.0 | FIXED-MEASURED | MEAS | found by running a probe, not by reading a record |
| W7-95-intrinsic-semantics-census | the `Intrinsic` category never had its semantics checked | OPEN | MIXED | partial — String code-point triples open. **Carries the C18 reconciliation banner: VM-fatal, ulp, pow-split, 645-rows** |
| W7-95a-string-code-point-family | String code-point natives read UTF-16 through a Rust `str` | OPEN | MIXED | **its two `VM ABORT` rows are NOT covered by the "no family aborts the VM" result** — see contradictions |
| W7-99-parse-and-pow-grammars | Rust std routines where Java specifications were meant | FIXED-UNVERIFIED | MIXED | **banner**: special values MEASURED-fixed; the pow FAST PATH is a separate open item |
| W7-98-character-unicode | `java.lang.Character` answered Rust's Unicode, not Java's | OPEN | MIXED | partial; fixes inert until the duplicate deregistrations land |
| W7-54-strictmath-fdlibm-family | StrictMath fdlibm port, bit-exact on 29 of 29 | FIXED-MEASURED | MEAS | **no `Status:` line** — status prose is the RETIRED blockquote |
| W8-C3-1-intrinsic-census-round-2 | second-generation vector over the never-invoked 40% | OPEN | MIXED | **banner**: it HAS since been run; no VM aborts. 614-triple arithmetic is its own |
| W8-C15-2-option-objects-with-no-reader | `HexFormat`/`UUID`/`Base64` option object with no reader | OPEN | PRED | partial; the load-bearing half is NOMINATIONS N1–N3 |

## Collections

| record | subject | status | prov | notes |
|---|---|---|---|---|
| C7-1-map-values-is-an-abstractcollection-not-a-list | `Map.values()` returns an `ArrayList` | OPEN | PRED | partial. **Banner**: the inherited-method hazard does not exist; the rewrite costs +17 registrations |
| C7-2-the-interface-doors-and-what-must-move-together | interface-door line numbers and the atomic set | OPEN | SRC | **Banner**: 23 rows not 18, `:38096` not `:37787`, hazard disproved |
| C7-3-ll-get-overlay-first-and-the-four-second-writers | LinkedList overlay-first reads, four second writers | OPEN | MIXED | partial; fixture green on HotSpot, never run on CratonVM |
| C13-1-the-interface-doors-never-open-for-a-values-view | the doors never open; routing layer landed | OPEN | MIXED | partial — routing layer landed but **inert today**. Authority for the two corrections above |
| C13-2-the-five-values-view-classes-are-not-one-family | per-class native needs for the five view classes | OPEN | MIXED | analysis + a costed change NOT taken; 25 registrations, not 32 |
| C13-3-native-map-key-set-returns-a-hashset | `keySet()` returns a real `HashSet`, not a view | OPEN | MIXED | nothing changed; Serializable divergence unmeasured |
| W7-1-treemap-views-and-iterator-remove-contract | TreeMap views and the `Iterator.remove` contract | OPEN | MEAS | partial; modCount on sort/replaceAll left open deliberately |
| W7-16-arraydeque-and-linkedlist-residuals | ArrayDeque streamed empty; LinkedList iterator gates | FIXED-MEASURED | MEAS | closed in three arms; **files a NEW open synthetic-mode finding** |
| W7-20-refusal-laundered-into-wrong-answer | a refusal laundered into a wrong answer | FIXED-MEASURED | MEAS | includes a confirmed negative prediction |
| W7-2-primitive-stream-terminal-surface | primitive stream terminals compiled out of the binary | FIXED-MEASURED | MIXED | partial; Double/LongStream residuals source-only |
| W7-33 / W7-36-differential-view-families | views not writing through; refusals never firing | OPEN | MIXED | partial; 19 of 20 changed in source, nothing rebuilt |
| W7-65-stream-reuse-throws | modelling `linkedOrConsumed` for stream reuse | OPEN | MIXED | partial; a named residual set left open on purpose |
| W7-96-chm-table-never-populated | `ConcurrentHashMap.table` never populated | OPEN | MIXED | partial; retirement half not unblocked |
| W2-1-strict-refuses-the-synthetic-stream-stack | strict refuses the synthetic stream stack | OPEN | MIXED | partial; its own original warrant declared FALSE, conclusion survives |

## JCA / crypto

| record | subject | status | prov | notes |
|---|---|---|---|---|
| W7-15-cipher-silently-wrong-algorithm | `Cipher.getInstance("ChaCha20")` returned AES-256-ECB | FIXED-MEASURED | MEAS | RFC 8439 known-answer vector; three before/after rows now stale |
| W7-21-keygen-and-the-synthetic-secretkeyspec-twin | `KeyGenerator` ignored its algorithm | FIXED-MEASURED | MEAS | `Key.getAlgorithm` hardcoded `"AES"` still open (synthetic-jdk only) |
| W7-39-jca-missing-algorithms | HmacSHA224 / Blowfish / RC4 refused; advertising reconciled | FIXED-UNVERIFIED | MIXED | **the original `W7-39`** — keeps the number |
| W7-29-jca-advertise-implement-gaps | engines answering algorithms never advertised | SUPERSEDED | MIXED | retired as work; residuals live under W7-63 |
| W7-63-jca-advertise-vs-serve | advertises what it refuses, serves what it never advertised | FIXED-UNVERIFIED | MIXED | second pass found an ALIAS half that never landed; true count 5 → 7 |
| W7-71-jca-exception-types-and-line-separator | RSA padding raised unchecked types; `Files.write` LF | FIXED-UNVERIFIED | MIXED | 2 sampled rows → 9 defects |
| W4-3-security-getalgorithms-short-list | `Security.getAlgorithms` answered the empty set | SUPERSEDED | MIXED | Patch E marked DEAD and destructive if applied |
| L8-securerandom-provider | `SecureRandom.getProvider()` null; any algorithm accepted | FIXED-UNVERIFIED | MIXED | headline verified on `ba65f1a19`; residual unbuilt |
| W7-61-sslengine-layout-and-tls-blocking | SSLEngine layout false positive; TLS blocking sites | OPEN | MIXED | **no `Status:` line** — status is a third-pass blockquote. Windows half of item 2 OPEN |

## IO / net

| record | subject | status | prov | notes |
|---|---|---|---|---|
| W2-2-blocked-reader-async-close-wakeup | a blocked reader never woke on `close()` | FIXED-UNVERIFIED | MIXED | surface 3 explicitly unverified |
| W7-53-blocking-close-family | blocking close-awareness: 19 sites fixed, 7 open | OPEN | MIXED | **no `Status:` line** — status is the title plus a bold block |
| W7-57-close-flush-swallow-sweep | 51 close/flush sites dropped the delegated failure | FIXED-UNVERIFIED | MIXED | 213 bound-but-uninspected calls left unclaimed |
| W7-64-printstream-trouble-and-errormanager | PrintStream trouble/ErrorManager fixes measurably inert | OPEN | MEAS | its run banner refutes its own FIXED rows |
| W7-70-printstream-close-noop | `PrintStream.close()` was a no-op for every stream | FIXED-UNVERIFIED | MIXED | contract measured on a recording sink |
| W7-81-write-route-three-way | a delegated write's three outcomes collapsed into one bool | FIXED-UNVERIFIED | MIXED | |
| W7-83-segment-as-backing-array | `ByteBuffer.array()` handed back a MemorySegment | FIXED-UNVERIFIED | MIXED | the mock hid it |
| W7-88-net-channels-dead-registration | dead `ServerSocketChannel.socket()` registration | FIXED-UNVERIFIED | MIXED | live sibling defects flagged |
| W7-24-httpserverloop-and-strict-fallbacks | HttpServerLoop door defect and strict fallbacks | FIXED-UNVERIFIED | MIXED | binary predates the fix |
| W7-50-synthetic-jdk-strict-six | six vectors only the synthetic-jdk strict arm fails | OPEN | MIXED | **its own Status line is stale** — §12 is a real run showing defect A live |
| C6-1-https-urlconnection-session-accessors | https carrier was the abstract class; six accessors added | OPEN | MIXED | partial; the populator is a NOMINATION, so every accessor answers "not open" |
| C12-2-https-session-capture-and-the-cipher-name-it-records | session capture; rustls cipher name rewritten | FIXED-UNVERIFIED | PRED | |
| C12-3-optional-value-slot-holds-an-int-flag | Optional natives write an int flag into the value slot | OPEN | PRED | lane owns neither file; all NOMINATIONS |
| P4A-TOMCAT-20260812 | embedded Tomcat serves HTTPS; four defects | OPEN | MEAS | two of three VM defects reproduce in both modes |

## nio / file

| record | subject | status | prov | notes |
|---|---|---|---|---|
| W7-8-fabricated-success-io-sweep | fabricated success across `java.io` / `java.nio.file` | OPEN | MIXED | partial; §9 is the largest item and is NOT fixed |
| W8-C14-1-default-filesystem-second-door | `theFileSystem()` was a second, unwired door | FIXED-UNVERIFIED | MIXED | |
| W8-C14-2-default-provider-singleton | the default provider was never a singleton anywhere | FIXED-UNVERIFIED | MIXED | |
| W8-C14-3-nio-singleton-audit-and-residuals | `nio_file.rs` split-singleton audit | OPEN | MIXED | corrects a brief: the wrong-typed field read is **not** VM-FATAL |
| W8-C4-3-default-filesystem-two-doors | the boot-loader door is a missing native | OPEN | MIXED | NOMINATION only |

## JIT / typecheck

| record | subject | status | prov | notes |
|---|---|---|---|---|
| W7-38-jit-aastore-never-called-its-own-check | the JIT lowered `aastore` inline, bypassing its check | FIXED-UNVERIFIED | MIXED | after values explicitly PREDICTED |
| W7-101-aastore-interface-component-blanket | `aastore` fails open on every interface-component array | FIXED-UNVERIFIED | MIXED | **renumbered from `W7-39` by C18**; before EXECUTED, after PREDICTED |
| W8-C10-1-typecheck-hatch-audit-and-aastore-precedence | every fail-open hatch in `typecheck.rs`, audited | OPEN | MIXED | partial; all 11 references to the renumbered record rewritten |
| W8-C16-1-serializable-cloneable-are-not-object | `Serializable[]`/`Cloneable[]` are not `Object[]` | FIXED-UNVERIFIED | MIXED | must land as a PAIR with `synthetic_implements` |
| W8-C16-2-synthetic-implements-simple-name | `synthetic_implements` asked whether a CONTAINER's name contains "Collection" | FIXED-UNVERIFIED | MIXED | 410 over-admissions → 50; oracle rows EXECUTED, CratonVM effect PREDICTED; closes `W8-C10-1` §7 N3 |
| W8-C4-1-array-cast-klass-origin | `klass_origin` looked up the array class | FIXED-UNVERIFIED | MIXED | |
| W8-C4-2-map-of-instanceof-collection | `Map.of()` answered `instanceof Collection` true | OPEN | MIXED | NOMINATION, outside the lane's files |
| W7-37-differential-throwable-and-vm | Throwable state machine, array cast-message clause | OPEN | MEAS | partial; 6 of 8 rows verified; the array arm of `klass_origin` never executed |

## Class loading / modules

| record | subject | status | prov | notes |
|---|---|---|---|---|
| W2-3-module-descriptor-answers-empty-sets | `ModuleDescriptor` returned empty sets for every module | OPEN | MEAS | all four out-of-file patch parts deliberately not landed |
| W4-2-unnamed-accessor-bypasses-encapsulation | instantiating a class in a non-exported package | FIXED-UNVERIFIED | MIXED | |
| W5-1-loadlibrary-allowlist-too-wide | the `loadLibrary` allowlist admitted too much | OPEN | MIXED | partial; a green Windows A/B does not measure the Linux road at risk |
| W6-2-module-serviceloader-provider-factory | no `provider()` factory form; `setAccessible` discarded | FIXED-UNVERIFIED | SRC | last row armed in source, unrun |
| W6-6-nativelibraries-load-fabricated-success | `NativeLibraries.load` returned true for everything | OPEN | MIXED | partial; a NEW finding — `java.desktop` admitted on one road only |
| W7-9-minted-interface-abstract-methods | minted interface/abstract receivers | OPEN | SRC | partial; five residual triples, four unfixable in-lane |
| W7-17-vm-internal-door-sweep | 44 minted classes, two gates, four verdicts | FIXED-UNVERIFIED | MIXED | §5's `strict?` column self-falsified |
| W7-31-enable-preview-wiring | the preview gate had no switch | FIXED-UNVERIFIED | MIXED | typed `defineClass` fix absent from every available binary |
| W7-79-loadlibrary-compatible-arm | `load0`/`loadLibrary0` read `args[1]` on the Compatible arm | FIXED-UNVERIFIED | MIXED | RED proof measured pre-fix; post-fix arm not re-run |
| W7-85-serviceloader-stream-validation | `stream()` skipped the provider return-type gate | FIXED-UNVERIFIED | MIXED | |
| W7-87-urlclassloader-namespace-asymmetry | bare `URLClassLoader` resolved classes it never loaded | FIXED-MEASURED | MEAS | |
| W7-97-initphase2-skipped | `initPhase2` skipped; nio is the real blocker | OPEN | MIXED | the skip is correct; the stated reason for it was not |
| L16-classnotfound-vs-noclassdeffound-shapes | absent array element type thrown as the wrong error | FIXED-UNVERIFIED | MIXED | last residual closed in source, not built |

## Reflection / method handles

| record | subject | status | prov | notes |
|---|---|---|---|---|
| W4-1-publiclookup-allowedmodes-never-checked | `allowedModes` never written or enforced | FIXED-UNVERIFIED | MIXED | **its own two status bullets contradict each other** — see contradictions |
| W6-8-method-invoke-exports-gate | `Method.invoke` gave public methods no module check | OPEN | SRC | partial; the gate is unexercised by any vector |
| W7-12-strict-annotation-proxy | strict-mode annotation proxy refusal and `toString` | OPEN | MEAS | headline closed; a NEW live defect found in both arms |
| W7-19-methodhandles-compatible-residuals | `asCollector` carrier, `bindTo` refusal | FIXED-UNVERIFIED | MIXED | one out-of-file line still unapplied |
| W7-26-getannotation-swallowed-exception | `getAnnotation` returned null for a pending VM failure | FIXED-UNVERIFIED | MIXED | partial; 13 delegation sites remain |
| W7-93-stackwalker-option-constants-null | `StackWalker$Option` constants fabricated | FIXED-UNVERIFIED | MIXED | `Thread$State.values()` mints fresh instances |
| L15-nestmate-access-field-and-constructor | field/constructor reflection lacked the caller check | FIXED-UNVERIFIED | MIXED | vector has NEVER been run on CratonVM |

## Threads / concurrency

| record | subject | status | prov | notes |
|---|---|---|---|---|
| W3-4-forkjointask-status-flags-and-the-eager-default | `isCompletedAbnormally` and the eager-fork default | OPEN | MIXED | two vacuous Rust guards — `apps/fjp_probe/` is absent |
| W6-9-complete-erases-the-abnormal-record | `ForkJoinTask.complete` erased the abnormal record | FIXED-UNVERIFIED | SRC | §8's heading still says "not applied" though it is — cost two agent runs |
| W6-12-stampedlock-split-brain | StampedLock refuted; Phaser was the live defect | OPEN | MIXED | both of its prescriptions rejected |
| W7-14-fjp-common-factory-bound-by-name | the common-pool factory was bound by a JDK-21 name | OPEN | MIXED | Compatible half awaits a human decision |
| W7-18-structured-task-scope-jep505 | StructuredTaskScope JEP 505 surface | OPEN | MIXED | never run on CratonVM |
| W7-23-thread-container-registration | container dropped on start, never removed on exit | FIXED-UNVERIFIED | MIXED | a wrong flip signature is a hang, not a FAIL |
| W7-27-thread-exit-java-cleanup | terminating threads never got Java-side cleanup | OPEN | MIXED | partial; the main/primordial call site is unapplied |
| W7-75-continuation-forkjoinpool-alias | Continuation and ForkJoinPool read-side slot aliases | FIXED-UNVERIFIED | MIXED | assert agreement, not the parallelism value |
| W8-C15-1-atomic-array-bounds-had-no-check-at-all | the atomic ARRAY family had no bounds check at all | FIXED-UNVERIFIED | MIXED | a 224-line fixture said that was fine; before EXECUTED, after PREDICTED |

## Process

| record | subject | status | prov | notes |
|---|---|---|---|---|
| W6-10-process-enumeration-syscall-cost | one snapshot per tree node, one `OpenProcess` too many | OPEN | SRC | residual un-appliable as written — its target record left the directory |
| W7-10-processhandle-interface-stub-bodies | `ProcessHandle`/`$Info` stub bodies fabricated | OPEN | SRC | §6's stub-count deltas declared dead — do not quote |
| W7-46-process-cluster | process natives, silently skipped checks | OPEN | MIXED | only the oracle fixture was executed |
| W7-86-static-native-arity | natives indexed args for the wrong receiver shape | OPEN | MIXED | partial; §4.2's four rows still open |
| W7-92-shutdown-hooks-never-run | hooks register and no thread ever starts them | FIXED-UNVERIFIED | MIXED | **banner**: the DEFECT is MEASURED (flatly, no hedge); the FIX is not |
| P4A-SPRING-20260812 | a Spring context constructs; hooks never run | OPEN | MEAS | shutdown-hook finding stated flatly; four nominations open |

## Time / locale / format

| record | subject | status | prov | notes |
|---|---|---|---|---|
| C6-2-simpletimezone-id-resolved-instead-of-rawoffset | `SimpleTimeZone` answers from the ID | SUPERSEDED | MIXED | **corrected**: §D's predicted 10,800,000 measured at 7,200,000 |
| C12-1-simpletimezone-the-trap-and-the-second-site | the unregistration landed; a second site remains | OPEN | MIXED | the authority for 7,200,000 and for "the skew is not a constant" |
| C6-3-a-native-registered-for-the-vms-own-instance-hijacks-the-apps | class-wide registration hijacks app instances | OPEN | **none** | **no provenance statement anywhere near the top** |
| W7-3-format-conversions-and-stringbuilder-bounds | `String.format` float conversions, StringBuilder bounds | FIXED-UNVERIFIED | MIXED | |
| W7-34-formatter-family-residuals | twelve `java.util.Formatter` divergences | OPEN | MIXED | a blocking co-requisite drops its Locale — RJdkHello goes red |
| W7-41-format-exception-subclasses | refusals threw the base type, not the subclass | FIXED-UNVERIFIED | MIXED | evidence unscheduled — `run.sh` never runs `probes/` |
| W7-44-numberformat-enum-and-double-tostring | accounting currency pattern, enum message, last-bit log | FIXED-UNVERIFIED | MIXED | |
| W7-80-locale-data-stage-two | CLDR locale data was reachable all along | FIXED-UNVERIFIED | MIXED | one named breaking site must land in the same change |
| W7-91-format-date-symbols-hardcoded-english | `%t`/`%T` names came from hard-coded English tables | OPEN | MIXED | partial; §5's numeric half is live |

## Logging

| record | subject | status | prov | notes |
|---|---|---|---|---|
| W7-22-shadow-retirement-logging-and-time | shadow retirement for logging and date/time | OPEN | MIXED | blocked on a Linux build and three frozen artefacts |
| W7-25-jul-getlogger-regression | JUL `getLogger` regression; ambient kind hides a shadow | FIXED-MEASURED | MEAS | a Compatible-only console fallback divergence is open |
| W7-35-jul-supplier-and-payload-residuals | JUL supplier overloads and record payload residuals | FIXED-MEASURED | MEAS | 24 rows still ambient `Intrinsic` |
| W7-56-infercaller-strict | the shadow LogRecord ctor dropped `needToInferCaller` | FIXED-MEASURED | MEAS | **no `Status:` line** — status is a table ROW at line 5 |

## GC / memory / object layout

| record | subject | status | prov | notes |
|---|---|---|---|---|
| W4-4-slot-index-species-sweep | natives using synthetic slot indices on real JDK objects | OPEN | SRC | every count is a `javap`+source upper bound, never a run |
| W7-49-slot-index-recensus | census against the un-blinded alias detector | OPEN | SRC | 511 direct allocation sites bypass the detector |
| W7-58-bytebuffer-direct-arm | `bb_state` had no direct-buffer arm | FIXED-UNVERIFIED | MIXED | |
| W7-66-live-over-allocations | 22 live over-allocation sites | OPEN | MIXED | "over is not a defect predicate"; one genuine aliasing defect left |
| W7-68-live-under-allocations | the under-half: no object is actually short | FIXED-UNVERIFIED | MIXED | §3.5's premise refuted in its own banner |
| W7-69-read-side-alias-instrument | the read-side alias instrument and its first census | OPEN | SRC | the census has never been run; all `lib.rs` line numbers stale |
| W7-72-ssc-socket-and-filechannel | ServerSocket cached into `keys`; FileChannel map | FIXED-UNVERIFIED | MIXED | an in-bounds write of the wrong field — invisible to count-based instruments |
| W7-73-short-object-blind-spot | short objects hide in the `Err(_) => ClassId::new(0)` arm | OPEN | SRC | headline self-corrected 16-of-30 → 12-of-28; a cited authority does not exist |
| W7-74-short-object-repairs | two live short Thread mirrors repaired, twelve latent | FIXED-UNVERIFIED | MIXED | §5's "14 of 28" is an arithmetic slip |
| W7-76-bytebuffer-alias-residuals | one parity fix, two refused renumbers | OPEN | MIXED | |
| W7-77-guarded-slot-maps | four guarded slot maps; no map renumbered | FIXED-UNVERIFIED | MIXED | §5.3's disposition stands but BOTH its stated reasons are false |
| W7-84-primitive-in-reference-store | a primitive stored into a declared-reference slot | FIXED-UNVERIFIED | MIXED | four implementations converged on auto-boxing |
| W7-89-memorysession-checkvalidstate | the FFM liveness gate fails open on closed arenas | FIXED-UNVERIFIED | MIXED | |
| W7-90-slot-map-sweep-caller | the declared-slot-map sweep had no caller | OPEN | SRC | three of four doors closed |

---

## Contradictions found and NOT resolved

A contradiction stated is worth more than a contradiction guessed. None of
these was silently decided.

1. **`Math.ulp`'s NaN patterns.** `W7-95` §"`Math.ulp` is EXECUTED-fixed"
   reports the shipped body differing from `Math.abs` on **16,777,212 NaN bit
   patterns** (payload dropped). The later result is that the two forms are
   **equivalent over all 4,294,967,296 patterns**. Both are described as
   measured. They reconcile only if "equivalent" excludes NaN payload — which
   the javadoc permits, since it promises only "is NaN". **Not settled here.**
   Whoever re-runs it should say which of the two claims their number is.
2. **`W7-95a`'s `VM ABORT` rows.** Rows 14 and 38 of the String code-point
   family (`offsetByCodePoints(10,-1)`, `indent(-1)` over an NBSP) are recorded
   as Rust panics. The measured result "no family aborts the VM" is scoped to
   the six `floorDiv`/`floorMod` triples and to the second-generation census.
   **Whether these two String rows still abort is not established by that
   result**, and `W7-95` lists the String family as still open. Do not read
   "no family aborts the VM any more" as covering them.
3. **`W7-62`'s own two statements about itself.** Its `Status:` line (≈:50)
   says *"SOURCE COMPLETE, NOTHING BUILT OR RUN"*; its top banner (≈:3–48) is a
   measured `cargo test` run. One of the two is describing a different scope
   and the record does not say which.
4. **`W4-1`'s two status bullets.** One says the residual is "FULLY CLOSED";
   the next says the deletion-only patch is "genuinely unapplied". Both are in
   the same status block.
5. **`W7-50`'s header vs its §12.** The header says "source landed,
   UNVERIFIED"; §12 is a real run showing defect A still live. The header is
   the stale half, but this lane did not rewrite it — it belongs to the lane
   that took §12's run.
6. **`W6-9` §8's heading** still reads "not applied" for a section that is
   fully applied. Recorded because it has already cost two agent runs.
7. **`README.md`'s record count.** Its headline says 94, with a paragraph
   explaining that both terms of the arithmetic that produced 94 have moved and
   cancelled. The directory now holds **153 `.md` files**. The README's
   exclusion list is what drifts, not the count. Not rewritten here: the README
   is the directory's own front matter and a recount belongs with whoever owns
   its exclusion rules.

## Records with no parseable status

A grep for a line beginning `Status` misses **22 of the 153 files**. Of those
22, all but one carry status prose somewhere else — in the title, in an opening
blockquote, in a bold paragraph, or (in `W7-56`) in a **table row**. Four are
worth naming because they read as status-less on a skim: `W7-53`, `W7-54`,
`W7-56`, `W7-61`. The remaining seventeen are dated deliverables whose whole
body is the status.

**Exactly one record has no provenance statement near the top at all:**
`C6-3-a-native-registered-for-the-vms-own-instance-hijacks-the-apps.md`. Every
other record in this directory says, somewhere in its first screen, whether its
numbers were executed or predicted.

## NOMINATIONS (out of this lane's boundary — `.md` under this directory only)

**N1 — the renamed record is cited from two Rust files.** Both are stale paths
now.

*File:* `vm/src/runtime/interpreter/typecheck.rs`, line ≈782
*exact literal old text:*
`docs/known-issues/jdk-only/W7-39-aastore-interface-component-blanket.md`
*exact literal new text:*
`docs/known-issues/jdk-only/W7-101-aastore-interface-component-blanket.md`

*File:* `vm/src/runtime/interpreter/tests.rs`, line ≈91
*exact literal old text:*
`docs/known-issues/jdk-only/W7-39-aastore-interface-component-blanket.md`
*exact literal new text:*
`docs/known-issues/jdk-only/W7-101-aastore-interface-component-blanket.md`

The five `W7-39` mentions in `native-builtins/src/jca/*.rs` and
`phases_late/ssl_security.rs` refer to the **JCA** record, which kept the
number. **Do not rewrite those.**

**N2 — `register_interface_natives`' own header comment is stale.** It says
"these 23" for a function with 38 registrations
(`native-collections/src/lib.rs`, ≈28773). Two records now correct it from the
outside; the comment itself is what a reader hits first.
