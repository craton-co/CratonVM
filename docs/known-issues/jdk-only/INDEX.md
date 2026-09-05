# INDEX — every record in `docs/known-issues/jdk-only/`

> **STARTING A LANE? The operating rules are not in this directory.**
> [`../../contributing/jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md)
> holds the method, the `owns_slot` and identity traps, the probe-hygiene list,
> the landing protocol and what "done" means. It is permanent; the records here
> are dated. Added 2026-08-29, when `HANDOFF-20260828-SCOPE.md` §3/§5/§6 moved
> there so that page could retire without taking the rules out of circulation.
>
> **It did retire, on 2026-09-01**, to `HANDOFF-20260828-SCOPE.md` in
> the internal tree, all three of its stated blockers discharged: Phase 2
> adjudicated, §4's two owned items closed, §5 rehomed. The eight-lane campaign
> that page scoped is finished; what it measured is a record, and what it taught
> is on the operations page. **Nothing in this directory is an operating page.**

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

> **SECOND PASS — lane F25, 2026-08-13.** The warning above was right. Listing
> re-taken: **227 `.md` files** including this one, and **73 of them had no row
> in this index** — every wave-D, wave-E and wave-F record, the whole
> `W8-D`/`W8-E`/`W8-F` harness line, the two C19 fixture records, and **all
> seven records of the SSL-session chain**. They are added below, in a dated
> block of their own rather than merged into C18's topic tables, so that
> C18's snapshot stays legible as the snapshot it is. **This block will rot the
> same way**: lanes F14–F25 were still landing while it was written.

> **FOURTH PASS — lane G40, 2026-08-17. Start here.** The warning above was
> right again. Listing re-taken: **280 `.md` files** including this one, and
> **36 of them had no row** — the entire wave-G line (`G1-1` … `G34-1`), plus
> `BASELINE-20260817.md` and `HANDOFF-20260814.md`, which are the two most
> current documents in this directory. They are added in the **FOURTH PASS**
> block at the bottom, §A. That block also carries:
>
> * **§B — seven standing claims this session falsified**, each naming the
>   record that asserted it, the record or commit that falsified it, and the
>   class of evidence. Read §B.1 before you trust any "this body is dead"
>   conclusion anywhere in this directory, and §B.2 before you reason about
>   which of a native and real JDK bytecode wins.
> * **§C — the statuses measurement settled**: 88 → 93 → 95 of 99 under
>   `--jdk-only`, and the eight vectors that closed.
> * **§D — what that pass could not settle.** An honest `unknown` is worth more
>   here than a confident label.
>
> **The count 280 will rot too, and faster than 155 or 227 did**: seven lanes
> were editing this tree as it was taken, and five records (`G35-1` … `G39-1`)
> were being written during the pass and have no row. Re-take the listing.

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

# SECOND PASS — waves D, E and F (added by lane F25, 2026-08-13)

**73 records, none of which had a row above.** Subjects are each record's own
H1, condensed; status is read from the record's own status prose, never from
its filename.

**A provenance convention runs through almost all of these and is stated once
here rather than repeated 73 times.** Waves D/E/F worked on a shared branch
whose lanes were forbidden to build or run CratonVM, so the standard shape is:
**HotSpot 25.0.3+9-LTS column MEASURED on the lane's own host, tree and
`jdk25src` citations READ, CratonVM "after" column PREDICTED** — i.e. `MIXED`
in this index's vocabulary, and `FIXED-UNVERIFIED` where a fix landed. Rows
below carry `MIXED` unless the record itself says otherwise; the exceptions
worth knowing are the `R11` baseline line (E32/E37/E41-R11 ran `cargo test` and
say so) and the `W8-E*` harness records (which ran the suite).

> **Do not read `FIXED-UNVERIFIED` here as "nearly done".** In this directory it
> means *no binary carrying the change has ever executed*. Several of these
> records also say, in their own residuals, that they were never type-checked.

## The SSL / TLS session chain — read in this order

Seven records, not six, and each one corrects or extends the one before it. A
reader who opens any single one of them will get a picture that a later record
in the chain has already moved.

| # | record | what it established | status | prov |
|---|---|---|---|---|
| 1 | E12-1-the-null-session-and-the-fabricated-cipher | a session that negotiated nothing answered `TLS_AES_256_GCM_SHA384`, `"UNKNOWN"`, a 32-byte pseudo-random id, and a **null** `SSLSocket.getSession()`; the eleven accessors are NOT uniform (some refuse, some answer null). Created `RSslNullSession` | FIXED-UNVERIFIED | MIXED |
| 2 | E22-1-the-null-session-in-the-registrar-that-actually-answers | the E12-1 fix went into a registrar that does not answer; the live one is `t27_tls.rs`. **`--dump-native-registry` is the instrument**, not grep | FIXED-UNVERIFIED | MIXED |
| 3 | E31-1-the-unregistered-door-and-the-slot-that-resurrects-a-fabrication | `getHandshakeSession` had no registration at all, so `RSslNullSession` ran **1 of its 47 checks**; and slot 3 is the peer host on the wide shapes and the **attribute map** on the 4-field one, which Jetty arms on every SSL request | FIXED-UNVERIFIED | MIXED |
| 4 | E42-1-the-slot-that-was-never-there-and-the-predicate-that-was-its-own-negation | the attribute slot the previous record reached for did not exist at that width, and a predicate was its own negation. Blocks the obvious "widen NEW-13" repair | FIXED-UNVERIFIED | MIXED |
| 5 | F6-1-the-arm-that-had-to-move-and-the-two-minters-it-keeps-wrong | the width arm moved; two minters are **deliberately** left wrong, with the reason recorded | FIXED-UNVERIFIED | MIXED |
| 6 | F10-1-the-two-minters-that-told-a-completed-handshake-it-never-happened | a COMPLETED handshake reported as never negotiated; `HTTPS_CLIENT_SESSION_MARKER` is not free to choose — it must not collide with any real socket id | FIXED-UNVERIFIED | MIXED (HTTPS arm measured on a loopback `HttpsServer`) |
| 7 | F18-1-four-session-doors-with-no-registration-and-the-twin-that-read-another-table | `invalidate`/`getPeerHost`/`getPeerPort`/`getSessionContext` had **no registration** (⇒ `AbstractMethodError`), and `getPeerPrincipal` read a different table from its twin `getPeerCertificates`. **Corrects "invalidate moves isValid and nothing else"** — it moves `getSessionContext` too | FIXED-UNVERIFIED | MIXED |

Adjacent, same family, not part of the chain proper:
`E3-1-the-cipher-name-helper-and-its-real-denominator` (the helper had 1 caller
of 8) and `D3-3-rustls-cipher-names-reaching-jsse` (five of seven sites need
the rustls spelling).

**Fixture state:** `regression-suite/src/RSslNullSession.java` was **47 checks**
through records 1–7 and asserted nothing about any of record 7's four doors.
Lane F25 extended it to **89 checks**; see
`F25-1-the-four-doors-the-null-session-never-knocked-on-and-two-argument-kind-vectors-20260813.md`.

## Wave D

| record | subject | status | prov |
|---|---|---|---|
| D1-R11-SERVICELOADER-DOUBLE-SOURCE | the duplicate `junit-jupiter` engine is a modular jar on `-cp` promoted into the boot layer; **not intermittent**, and `getResources` is innocent | OPEN | MIXED |
| D3-1-simpledateformat-format-zone-arm | the zone-arm patch is right and **the fixture meant to prove it cannot** | OPEN | MIXED |
| D3-2-http2-optional-reference-layout | eleven `Optional` sites, not nine — and every one is dead outside synthetic-JDK mode | OPEN | MIXED |
| D3-3-rustls-cipher-names-reaching-jsse | five of seven sites need the rustls spelling; one is a provable no-op | OPEN | MIXED |
| W8-D2-1-two-summary-lines-and-the-suite-denominator | two REGRESSION SUITE lines in one log, neither with a denominator | META (instrument audit, no VM defect) | MEAS |

## Wave E — defect and census records

| record | subject | status | prov |
|---|---|---|---|
| E1-1-simpledateformat-format-zone-arm-landed | the `format` zone arm LANDED; the fixture the brief named cannot gate it | FIXED-UNVERIFIED | MIXED |
| E2-1-optional-reference-layout-landed | eleven `Optional` sites fixed, one left alone; the builder family keeps four fixture rows red | FIXED-UNVERIFIED | MIXED |
| E5-1-base64-null-contract-and-the-fourth-site | the fourth Base64 null site, and a whole-surface audit behind it | FIXED-UNVERIFIED | MIXED |
| E7-1-character-int-code-point-contracts | `Character.charCount` lost the sign; the above-BMP case tables had never been looked at | FIXED-UNVERIFIED | MIXED |
| E8-1-string-null-contracts-and-the-third-copy-of-six-constants | `String`'s reference arguments swallowed every null; six version-skew constants needed a third copy | FIXED | MIXED |
| E10-1-intrinsic-census-round-3 | 292 of the last 420 `Intrinsic` triples, and a `SimpleDateFormat` memo collision | OPEN | MIXED |
| E13-1-the-six-builders-decode-a-reference-as-an-int | six builders decoded a reference as an `Int`; four more sites the idiom did not mark; one row that still cannot go green | FIXED-UNVERIFIED | MIXED |
| E14-1-base64-the-fabricated-receiver-and-the-seven-methods-that-read-it | the fabricated Base64 receiver and the seven methods reading it | OPEN (NOMINATION §7) | MIXED |
| E17-1-character-digit-int-and-the-fifteen-unregistered | `Character.digit(int,int)` stays unregistered; the "15 deliberately unregistered" methods are **57** | OPEN — **no code change** | SRC |
| E18-1-the-jit-facing-string-doors-and-the-fourth-copy-of-one-search-rule | the JIT-facing `String` doors, and one JVMS search rule written four times | FIXED | MIXED |
| E21-1-getstatic-has-no-native-path | `GETSTATIC` has no native path; the family is **148, not 5** | OPEN | PRED |
| E23-1-synthetic-jdk-nosuchmethoderror-census | 7,700 methods over 845 classes; the one that costs 88 fixtures | META (measurement + one behaviour-neutral deletion) | MEAS |
| E26-1-the-reach-audit-what-eleven-green-families-were-not-asking | eleven GREEN families audited for what they were **not** asking | OPEN (mostly NOMINATION §8) | MIXED |
| E27-1-the-jit-indexof-int-intrinsic-was-the-fifth-copy | closes E18-1's three JIT doors; the fifth copy of the search rule; one E18-1 claim was wrong | FIXED | MIXED |
| E34-1-the-throwable-ctor-table-and-the-descriptor-javac-actually-emits | one fixed descriptor list; 62 classes, 97 constructors that do not exist and 15 that do | FIXED (registrar data) | PRED |
| E36-1-inverted-enum-fallbacks-and-the-field-shaped-rows-that-cannot-fire | inverted `name`/`ordinal` fallbacks; a `values()` returning nine nulls with the difficulty stated as its excuse; 21 field-shaped rows classified | OPEN | PRED |
| E38-1-biginteger-shifts-ctors-and-the-stringbuilder-repeat-twin | the shift that allocates, the constructor that validated nothing, the `repeat` twin that won | LANDED (unbuilt) | MIXED |
| E39-1-four-pending-nominations-applied-on-top-of-the-new-denominators | four pending nominations APPLIED on denominators that had just moved | META / APPLIED | MIXED |
| E40-1-the-test-that-pinned-a-wrong-type-and-the-36-sites-a-getstatic-cannot-reach | a test pinning a wrong type; a withdrawn class three tests still describe; 36 call sites no `getstatic` can reach | OPEN | PRED |
| E41-GATHERER-SLOT-MAP-AND-JUL-LOGGER-CONVENTION | one slot map per class for `Gatherer`; the **second** `java/util/logging/Logger` convention | FIXED-UNVERIFIED | MIXED |
| E43-1-collapsing-the-blanket-throwable-ctor-rule-onto-one-table | the blanket throwable-`<init>` rule was at **five** sites, not four | FIXED (registrar side) | PRED |

## Wave E / R11 — the JDK-baseline and unfalsifiable-guard line

The one part of waves D–F that **ran tests**. Read E32 → E37 → E41-R11 in order.

| record | subject | status | prov |
|---|---|---|---|
| E4-R11-CLASS-PATH-MODULE-BOOT-LAYER-FIX | the fix for the class-path module promoted into the boot layer, and the one jar shape it does NOT close | FIXED-UNVERIFIED | PRED |
| E16-R11-P59-MODULE-LAYER-TWIN | NOM E-7's "dormant twin" was **not dormant, and not for the stated reason** | OPEN | MIXED |
| E20-R11-INTERSECTION-BLIND-GUARD | a guard that could not fail for the case it was written for, and a census built from its own answer | OPEN | MIXED |
| E25-R11-GUARD-POPULATION-SWEEP | the seventeenth `Mac` method, and every other guard whose population was its own answer | FIXED-UNVERIFIED | MIXED |
| E28-R11-P59-MODULE-WIDTHS-AND-CATALOG | three widths, and a catalog side effect whose real blocker is a field **NAME** | OPEN | MIXED |
| E32-R11-JDK-BASELINE-CAPABILITY | checked-in JDK surface baselines + the generator that writes them — **the missing capability, built** | FIXED-MEASURED (generator self-verified); consumer pending | MEAS |
| E33-R11-FOUR-UNFALSIFIABLE-GUARDS | four guards that could not go red, repaired and **mutation-checked** | FIXED-UNVERIFIED-BY-CARGO | MEAS (mutation) |
| E35-R11-SYNTHETIC-WIDTH-SWEEP | every synthetic allocation in `native-builtins/src/lib.rs` against its declaration and its twins | OPEN | MIXED |
| E37-R11-JDK-BASELINE-CONSUMER-AND-RATCHET | a self-testing parser, a four-kind two-way ratchet, the worked rewrite | FIXED-MEASURED (17/17) | MEAS |
| E41-R11-TWELVE-GUARDS-CONVERTED | twelve guards converted; three class names JDK 25 does not have | FIXED-MEASURED (25/25 in module) | MEAS |

## Wave W8-E — harness, oracles and the `aastore` atomic set

The `W8-E` line is where the **suite's own instruments** were audited. Six
fixtures had a broken HotSpot oracle; that is the standing reason a green
family is not evidence.

| record | subject | status | prov |
|---|---|---|---|
| W8-E6-1-aastore-ase-names-the-component-not-the-array | `aastore`'s ArrayStoreException named the element's COMPONENT class; the helper that fixed this for `checkcast` had one caller | APPLIED | MIXED |
| W8-E9-1-three-broken-oracles-and-the-suite-denominator | three fixtures whose HotSpot oracle was broken, and the suite's denominator. **The authority for the `checks=`/`fails=` one-value-per-line rule** | FIXED-MEASURED | MEAS |
| W8-E11-1-jit-aastore-third-twin-and-the-check-only-helper | the third `aastore` twin, and a check-only helper that lets the inline store come back | APPLIED + NOMINATION SET | MIXED |
| W8-E15-1-the-fourth-broken-oracle-the-unscheduled-vector-and-the-reach-ratchet | the fourth broken oracle, a vector registered nowhere, and G5 — a ratchet for **reach** | FIXED-MEASURED (for two fixtures) | MEAS |
| W8-E19-1-the-void-guard-sweep-and-the-aastore-atomic-set | undefined-RAX guard sweep; the `aastore` ATOMIC SET, 2 of 5 applied — **the tree does not build until §3 lands** | OPEN (partial) | MIXED |
| W8-E24-1-the-aastore-abi-slot-applied-and-seven-literals-the-nomination-missed | the ABI slot APPLIED 5 of 5; seven count literals W8-E19-1 did not carry | FIXED-UNVERIFIED | MIXED |
| W8-E29-1-bridge-census-round-1 | the first `Bridge` census — the largest `NativeKind` had never been measured | OPEN | MIXED |
| W8-E30-1-broken-oracles-five-and-six-and-a-lint | oracles five and six, shared launch hooks, and G6 — a lint making the reporting dialect self-enforcing | FIXED-MEASURED (for three fixtures) | MEAS |

## Wave F

| record | subject | status | prov |
|---|---|---|---|
| F1-1-the-boxing-caches-were-three-of-six-and-the-bounds-all-differ | three of six boxing caches, and no two of the six share a bound | FIXED-UNVERIFIED | MIXED |
| F2-1-the-charbuffer-that-read-empty-and-the-two-shifts-that-allocated | a `CharBuffer` that read empty, a shift that allocated 256 MB, a constructor that invented a zero | LANDED (unbuilt) | MIXED |
| F3-1-RANDOM-NULL-CONTRACT-AND-THE-SECOND-JUL-LOGGER-CONVENTION | `java.util.Random`'s null contract lives in a file that lane could not edit; the second `Logger` convention in `lib.rs` | OPEN (NOMINATION) | MIXED |
| F5-1-charbuffer-accessible-array-three-way-split | `hasArray`/`array`/`arrayOffset` had ONE branch where the JDK has THREE | FIXED-UNVERIFIED | MIXED |
| F6-1 … | see the SSL chain above | | |
| F9-1-the-eighteen-that-could-not-fail-and-the-descriptor-that-cannot-be-corrected | eighteen tests an `if let` could skip; two struct layouts HotSpot refuses to build; a descriptor that cannot be corrected in the test | OPEN | PRED |
| F10-1 … | see the SSL chain above | | |
| F11-1-reflective-boxing-is-canonical-everywhere-except-array-get | reflective boxing is canonical everywhere except `Array.get`; the caller that forbade the obvious fix was never a caller | FIXED-UNVERIFIED | MIXED |
| F12-1-NULL-CONTRACT-APPLIED-AND-THE-GETINSTANCE-ARGUMENT-ORDER | NOM F3-1 applied, plus a seventh divergence: `getInstance`'s argument **ORDER**. **Five of the seven had no check anywhere in the tree** | FIXED-UNVERIFIED | MIXED |
| F14-1-the-mutable-alias-a-hasarray-check-handed-out | the mutable alias `hasArray()` handed out, and the supertype that makes half the family's assertions ornamental | FIXED-UNVERIFIED | MIXED |
| F15-1-two-registrars-that-no-bytecode-can-name | two registrars no bytecode on this JDK can name, and the mode question that shortens every reachability argument | OPEN | MIXED |
| F16-1-one-layout-encoding-and-the-padding-the-jdk-never-inserts | one layout encoding, padding the JDK never inserts, and the union that discarded its members | FIXED-UNVERIFIED | PRED |
| F17-1-cds-sharedsecrets-fabrications | eleven CDS natives for classes JDK 25 does not have; two real natives a public-only baseline could not see; a factory-name guard punishing the correct spelling | FIXED-UNVERIFIED | MIXED |
| F18-1 … | see the SSL chain above | | |
| F19-1-the-sixteen-that-allocated-and-the-boolean-that-was-not-TRUE | sixteen allocating sites, a `Boolean` that was not `TRUE`, and the fifth boxing implementation that is the only correct one | FIXED-UNVERIFIED | MIXED |
| F20-1-the-three-unguarded-rescales-and-the-scale-that-negates-into-a-panic | three unguarded rescales, a scale that negates into a panic, two twins that shadow their own fix | **PARTIAL** | MIXED |
| F21-1-read-only-is-contagious-and-the-registrar-that-shadows-the-fix | read-only is contagious through `duplicate`/`slice`/`slice(int,int)`; three copies converge; a registrar shadows the repair | FIXED-UNVERIFIED | MIXED (916-row seven-family sweep MEASURED) |
| F22-1-formatter-utf16-units-and-t-zone | `java.util.Formatter` carried its output as a Rust `String`; `%t` had no time zone | FIXED-UNVERIFIED | MIXED |
| W8-F4-1-the-formatter-conversion-table-swept-against-the-spec | `%h` was an alias for `%s`; the null argument had never taken the JDK's printer | OPEN (edits landed, unbuilt) | MIXED |
| W8-F7-1-the-argument-driven-allocation-sweep-of-biginteger-and-bigdecimal | one bit that cost 256 MB, and the sweep for everything else sized by an argument | LANDED (unbuilt) | MIXED |
| W8-F13-1-formattable-never-dispatched-and-the-upper-caser-ran-after-the-justifier | `%s` of a `Formattable` never dispatched; the upper-caser ran AFTER the width justifier | OPEN (edits landed, unbuilt) | MIXED |
| F25-1-the-four-doors-the-null-session-never-knocked-on-and-two-argument-kind-vectors | fixture-only: F21-1's GAP 3c landed (19 rows), `RSslNullSession` 47 → 89, `RJdkSecurity` 80 → 123, this index brought current | FIXED-MEASURED **on the oracle only** — no CratonVM run | MEAS (HotSpot) |
| F23-1-the-guard-that-could-not-see-a-private-native | the surface baseline kept public+protected only, so **27.7% of the surface (528 rows) was invisible** and visible `native` methods went 3 → 13; 3 of 5 prior off-surface CDS verdicts were artifacts; **no registration had been wrongly deleted** | FIXED (32 baselines regenerated, v1 → v2) | MEAS (jrt image); the Rust half checked by a **transcribed proxy oracle**, 56 assertions |
| F24-1-sharedsecrets-spellings-and-the-list-that-checked-itself | `getJavaSecurityAccess` went with JEP 486; `getJavaUtilJarAccess` → `javaUtilJarAccess`; the two lists diverged **in both directions at length 15**, so every count-based check passed | FIXED-UNVERIFIED | MEAS (`javap -p` = 98 members; plain `javap` = 65) |
| F26-1-a-copying-slice-is-a-wrong-capability | `slice`/`duplicate`/`wrap` copied where HotSpot aliases; `bb.slice(2,4).duplicate().get(0)` read the wrong element **on dev** | PARTIAL — `ByteBuffer` landed; the six typed families are **specified, not landed** (three different aliasing mechanisms) | MEAS |
| F27-1-the-reader-that-answered-eight-and-the-length-it-called-an-address | `read_layout_kind` matched none of the real `ValueLayouts$Of*Impl` carriers; `alloc_return_slot` sized every aggregate return at 8 bytes — a live **out-of-bounds heap write**, since `ffi_call` writes `rtype->size` | FIXED-UNVERIFIED | MEAS |
| F28-1-the-t-family-answered-27-fields-it-must-refuse-and-a-null-locale-is-not-the-default | 186-cell sweep (31 `%t` fields × 6 `java.time` types): CratonVM answered **all 31 for all six** because `invoke_i32` returns 0 for a missing method; a null locale means **three** different things and the JDK means all three | FIXED-UNVERIFIED — **79 cells now refuse**; real blast radius, stated not buried | MEAS (forced `ru_RU`, `tr_TR`, `ar-EG`, `Asia/Kolkata`) |
| F29-1-the-wrapper-class-comes-from-the-call-site-not-the-methodtype | one handle whose `MethodType` says `Object` yields **six** wrapper classes — the class comes from the call-site static type, which never reaches natives; refutes F19-1 §7 N2's stated mechanism | PARTIAL — the `Object[]` collector needs a descriptor threaded across three crates | MEAS (3 runs, `-Xint`, `-XX:-UseCompressedOops`) |
| F30-1-the-registrar-call-graph-and-the-drifted-arm | **three** registration arms serving **four** configurations; the feature-build's real-JDK arm was missing four passes — incl. `register_random_and_securerandom_natives`, so a seeded `Random` returned **0 from every `nextInt`/`nextLong`/`nextDouble`** | FIXED-UNVERIFIED, gated by a source-witness that compares the arms element-for-element (mutation 8/8, control green) | READ (source census, line-numbered) |
| F31-1-three-roads-out-of-one-scale-and-the-zero-operand-that-is-exempt | one receiver, three answers — `Underflow` / `0` / `Overflow`, because `setScale` reaches the clamping instance `checkScale` and `toPlainString` the casting static `checkScaleNonZero`; a **zero raised operand is exempt**, so F20-1 N1(a)'s text would have refused four rows HotSpot answers | FIXED-UNVERIFIED | MEAS |
| F32-1-the-proxy-route-and-the-drifted-twin | the deciding line is an **inlined one-name copy** in a third dispatch door (`dispatch_virtual.rs`), not either site read before; since `real_proxy_super()` defaults true the guard has been **inert for the shipping configuration** since the real-super gate landed | PARTIAL — the route switch is staged and must land **after** the shim's `C B S I J` boxing arms | READ (line-numbered) + MEAS (a real `$Proxy0` dumped and disassembled) |
| F33-1-a-factory-and-its-owner-must-share-one-kind | `register()` re-tags by **receiver class**, so one ambient `Bridge` block left factories surviving strict mode while their carriers were dropped; **2 of 4 carriers cannot service a single `invokeinterface` in any mode**, which refutes "make them work" | FIXED-UNVERIFIED — the factory now carries the kind of the owner it hands out (derived rule, no second list) | MEAS (`javap -p`, with negative controls) |

## Wave C stragglers — the two C19 fixture records

| record | subject | status | prov |
|---|---|---|---|
| C19-1-optional-shape-fixture | `RJdkOptionalShape` — the executable form of C12-3, with an honest account of which rows reach it | FIXED-UNVERIFIED | MIXED |
| C19-2-simpledateformat-zone-fixture | `RSimpleDateFormatZone` — the `format`-shaped vector C12-1 §5 asked for, vacuity trap closed mechanically | FIXED-UNVERIFIED | MIXED |

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

---

# THIRD PASS — the measurement phase, 2026-08-14

**Why this block exists.** Waves A–F were written by lanes that could not
build or run the VM: every "after" in those records is **PREDICTED**. A
measurement phase then built the binary and ran the fixtures against the
HotSpot 25.0.3+9-LTS oracle. This block records what the binary said, adds the
seven records that had no row, and corrects the rows measurement falsified.

**Read `F41-1` first.** It is the summary of that phase and the only record in

**Starting fresh? Read `HANDOFF-20260814.md` first instead.** It has the build
and measurement loop, the traps that cost this session real time, and the
ordered list of what to pick up next.
this directory whose claims were verified by running the VM.

## Records added (had no row in either earlier pass)

| record | subject | status | prov |
|---|---|---|---|
| F34-1-the-synthetic-only-registrar-population-and-its-gate | 284 registrars reachable only via `register_synthetic_overrides`; **0 of 127 exclusive classes declare a `native` method**, so no capability gap at class granularity — the exposure is 2,412 triples registered by BOTH a synthetic-only family and a shipping pass (drift, ungated) | GATE LANDED, and **RUN**: `cargo test --test registrar_reachability` = 4 passed | READ + the gate EXECUTED |
| F35-1-the-segment-that-could-not-read-its-own-array-and-the-gate-that-was-inverted | heap `MemorySegment` access implemented; `asSlice` stamped `0 + offset` as an address, so a sliced heap segment dereferenced the literal offset — F27-1 **moved** that fault rather than closing it | FIXED-UNVERIFIED | MEAS (oracle) |
| F36-1-the-half-the-null-session-cannot-reach-and-the-verifier-that-was-never-invoked | `RSslLiveSession`, 95 checks, loopback TLS; **F18-1 §8.3(4) is WRONG** — its verifier claim was measured against a verifier that never ran | FIXED-MEASURED on the oracle | MEAS (121/121 mutants dead) |
| F37-1-the-typed-families-alias-and-a-read-only-put-that-succeeded | `ByteBuffer.allocate(8).asReadOnlyBuffer().put(0,(byte)1)` **SUCCEEDED**; F21-1 had CLEARED that cell against a body registration order shadows | FIXED-UNVERIFIED | MEAS |
| F38-1-the-formatter-locale-slot-and-two-year-fields-with-one-localized-minus | `Formatter()` wrote null into the locale slot, collapsing "no locale" and "the default" into one value; the deletion route fails on slot 0 | FIXED-UNVERIFIED | MEAS |
| F41-1-what-the-first-real-measurement-of-wave-f-found | **the measurement phase itself** — four defects, three of them created or left behind by wave F, none visible to the lane that owned the file | FIXED-MEASURED | MEAS (both VMs) |
| W8-E30-1-broken-oracles-five-and-six-and-a-lint-that-makes-the-dialect-self-enforcing | harness dialect lint | (as stated in the record) | — |

## Rows measurement CONFIRMED (PRED → MEASURED)

These predicted a denominator or a flip, and the binary agreed **exactly**:

| record | predicted | measured |
|---|---|---|
| F2-1 | `hex` 73 → up, checks 32–49/54–57/69/73 flip | **`hex=77`**, family green |
| F14-1 | `hex` 73 → **77** | **77** |
| F21-1 | `bounds` 102 → **121** | **121** |
| F25-1 | `RJdkIntrinsics2` **1022**, `RJdkSecurity` **149** | **1022 PASS**, **149 PASS** |
| F11-1 / F19-1 / F29-1 | reflective boxing identity | **`RJdkReflBox` PASS, 107 checks** |
| F40-1 | the route switch would not redden the proxy arms | proxy family green |

## Rows measurement FALSIFIED or narrowed

| record | what it claimed | what the binary showed |
|---|---|---|
| **F14-1 §N1** | fixed `native_tb_array`'s refusals | that body **never ran**. `--dump-native-registry`: `java/nio/IntBuffer.array()[I` is owned by `servlet.rs` (`owns_slot=true, overwrote=null`), and four sibling families were registered under the **wrong descriptor** `()[I` — phantom rows nothing can dispatch to. Fixed in `fdacf3a01`. |
| **F2-1** | `parseHex` fixed by registering the ranged overloads | correct, but resting on a reader that was wrong one layer down: `CharBuffer.toString()` applied the length and dropped the position **on the reflective route only**. Fixed in `fdacf3a01`. |
| **F18-1 §8.3(4)** | the `HostnameVerifier`'s session is a different object | **same object** — measured against a verifier that was never invoked. Correction note inlined in F18-1 by `871ae0a25`. |
| **F19-1 §7 N2** | the collector fix needs the handle's `MethodType` | the wrapper class comes from the **call-site static type**; one handle whose `MethodType` says `Object` yields six wrapper classes (F29-1). |
| **F5-1 §1** | `ReadOnlyBufferException` and `UnsupportedOperationException` share no supertype below `RuntimeException` | ROBE **extends** UOE; the conclusion survives on check *ordering* (F14-1). |
| **a `phases_early.rs` comment** | the `lib.rs` LogRecord ctor registration "is refused and this one silently owns the slot" | the reverse: `lib.rs owns=true inv=2`, `phases_early owns=false inv=0`. A fix landed in the dead body and changed nothing. Corrected in place by `8c72d23ca`. |

## Still open, measured but deliberately NOT fixed

- **`java/util/Hashtable`** diverges on **7 of 9** rows of the null axis. A
  separate slot from `Properties`, served partly by generic Map natives shared
  with `HashMap` (which legitimately accepts null keys), so it needs
  receiver-routing, not a copied guard.
- **`ConcurrentHashMap`**'s null-key helper carries an invented message
  (`"ConcurrentHashMap does not permit null keys"`) never checked against the
  oracle.
- **`InheritableThreadLocal`** captures at `start()` where the JDK captures at
  construction — see F41-1 §6. The current behaviour is a *documented
  workaround* protecting every `Executors` path; moving it needs
  interpreter-level tracing.

---

# FOURTH PASS — the listing re-taken, and seven claims this directory got wrong

**Lane G40, 2026-08-17.** Listing re-taken at `ls docs/known-issues/jdk-only/*.md
| wc -l` = **280 files**, including `INDEX.md` and `README.md`, on branch
`claude/jdk-only-mode-completion-1351c0` at `HEAD = 9ae371468`. Every file is
`.md`; there are no non-`.md` entries in this directory.

**Thirty-six of those 280 had no row anywhere in this index** — the whole wave-G
line (`G1-1` … `G34-1`, 34 records), plus `BASELINE-20260817.md` and
`HANDOFF-20260814.md`, both of which are the most current documents here and
neither of which was listed. They are added in §A below. The eight SSL-chain
records that a first-cell scan reports as missing are **not** missing: they are
rowed in "The SSL / TLS session chain", whose first column is a sequence number.

> **This block will rot too, and faster than its predecessors.** Seven lanes were
> editing this tree while it was written and five further records (`G35-1`,
> `G36-1`, `G37-1`, `G38-1`, `G39-1`) were being written *as* the count was
> taken. If you find a file with no row, it is newer than this pass, not a stray.
> **Re-take the listing before quoting 280.** The number has been 155, then 227,
> then 280 in four days.

**How completeness was checked**, so the next lane can repeat it rather than
trust it: extract every table row's first cell from `INDEX.md`, strip backticks
and bold, and test each filename's first 18 characters against that set. That
method has one known false-positive mode (the SSL chain, above) and one known
false-negative mode (a record named only in prose, never in a row) — both were
resolved by hand here.

---

## A. Records added — the wave-G line, the baseline and the handoff

Status is read from **each record's own status prose**, as everywhere else in
this index. Where the record's own prose and a later measurement disagree, the
row carries both and the Notes column says which is which.

| record | subject | status | prov | notes |
|---|---|---|---|---|
| BASELINE-20260817 | the measured state of the suite after `dev` merged: 88 → 93 → 95 of 99 under `--jdk-only`, plus this session's running corrections | META | MEAS | **the most current document in this directory.** Every number in it was executed |
| HANDOFF-20260814 | the previous session's handoff: build loop, traps, ordered next steps | META | MIXED | **carries a banner now** — §1's "two remain red", §3's `jdk25src` path and §4's `--dump-native-registry` recommendation are each falsified or narrowed. §2 stands, and is the standard this directory holds itself to |
| G1-1-hashtable-null-axis-and-the-chm-message-that-was-invented | `Hashtable`'s null axis; the `ConcurrentHashMap` message that was invented | OPEN | MIXED (oracle MEAS, CratonVM PRED) | bannered: its "no JDK source was read" premise was wrong — `src.zip` was there |
| G2-1-the-formatter-conversion-surface-measured-against-the-oracle | 2,932 oracle cells over the `Formatter` conversion surface; four disagreements | OPEN | MIXED (2,932 HotSpot cells MEAS; every CratonVM value PRED) | its §0 is the clearest provenance statement in this directory; copy it |
| G3-1-the-triple-level-mode-drift-census-and-its-gate | F34-1's 2,412 drifting triples re-counted at triple granularity: **1,540, not 2,412**; gate written | META (census + gate) | SRC | its own §0 says "NOTHING HERE WAS MEASURED ON A BINARY" |
| G4-1-the-io-and-nio-fabricated-success-sweep-measured | the `java.io` / `java.nio.file` fabricated-success sweep | OPEN | MIXED (oracle MEAS, CratonVM arm not) | bannered: "no JDK source was read" |
| G5-1-inheritable-threadlocal-captures-at-construction | `InheritableThreadLocal` captures at construction; the note that named the wrong cause | OPEN | MIXED | partial — the `--jdk-only` half is a NOMINATION, not a fix. Still red as `tlocal` inside `RJdkIntrinsics3` |
| G6-1-the-ffm-surface-measured-and-the-merge-questions-settled | the FFM surface; the merge's three questions | FIXED-UNVERIFIED (own prose: "FIXED-ON-ORACLE, NOT MEASURED ON A CRATONVM BINARY") | MIXED | **`RJdkForeign` has since gone green, MEASURED at `783685c34`** — see §C |
| G7-1-the-sslsession-surface-measured-and-the-merge-questions-settled | the `SSLSession` surface end to end | FIXED-UNVERIFIED | MIXED | bannered: "no JDK source was read". `RSslLiveSession` is still red |
| G8-1-the-collections-view-families-and-the-null-function-axis | the collections view families; the null-function axis | OPEN | MIXED (oracle MEAS, CratonVM PRED) | bannered: "no JDK source was read" |
| G9-1-the-intrinsic-semantics-census-settled | the `Intrinsic` semantics census, settled where it could be | FIXED-UNVERIFIED | MIXED | bannered: it names `jdk25src`'s absence as the reason a question stayed open |
| G10-1-the-bignum-surface-measured-and-the-shipping-twin | the bignum surface; the twin that is not compiled at all | FIXED-UNVERIFIED (own prose: "CODE LANDED, BEHAVIOUR UNVERIFIED ON CRATONVM") | MIXED | flags its own `invocations = 0` risk; that risk is now **larger**, not smaller (§B.1) |
| G11-1-shutdown-hooks-and-the-process-cluster | the shutdown-hook contract measured end to end; the process cluster re-read | FIXED-UNVERIFIED | MIXED | bannered: "no JDK source was read" |
| G12-1-the-proxy-that-any-interface-array-accepted | the `$Proxy` substring blanket in `typecheck.rs`, scoped | FIXED-UNVERIFIED per the record | MIXED (before MEASURED) | **`RArrayStoreInterfaces` went RED → GREEN, MEASURED**, at `d2e127930` |
| G13-1-the-abstract-declaration-that-was-invoked-directly | three vectors, **two** mechanisms, and the discriminator is a single field | OPEN (partial) | MEAS | the record that corrected `BASELINE-20260817`'s own owner column in place |
| G14-1-the-uri-value-surface-and-how-far-RJdkBridge1-got | the URI value surface | OPEN (own prose: PARTIAL) | MIXED (before MEAS) | `RJdkBridge1` is one of the four still red at `9964ca733` |
| G15-1-the-jul-null-axis-and-how-far-RJdkIntrinsics3-got | the `java.util.logging` null axis | OPEN | MIXED | **bannered** — its "the registry dump is taken at registration time" is falsified (§B.1) |
| G16-1-the-server-socket-impl-and-how-far-RSslLiveSession-got | the `ServerSocket` impl that was never there | OPEN | MIXED (before MEAS) | **bannered** — §8 concludes `plain_socket.rs` is off the path from `invocations = 0` (§B.1) |
| G17-1-the-dst-family-and-the-fixture-that-compared-two-empty-strings | the DST family answered "no zone has ever observed daylight saving"; the fixture that would have caught it compared two empty strings | OPEN in its own prose; **its vector is now GREEN, MEASURED** | MIXED | **bannered** for one `invocations = 0` inference (§B.1). `RSimpleTimeZoneRaw` green at `9964ca733` |
| G18-1-the-proxy-invocation-contract-and-two-vectors | the proxy invocation contract, and the two vectors that ride on it | OPEN (own prose: PART-FIXED-MEASURED) | MEAS | **bannered** — its `MethodHandle.asType` "not the live body either" was falsified by `G31-1` (§B.1) |
| G19-1-the-layout-step-and-the-scope-that-was-not-stable | the layout step; the scope that was not stable | FIXED-UNVERIFIED per the record | MEAS (before) | **`RJdkForeign` and `RForeignLayoutJdkInterfaces` both green, MEASURED**, at `783685c34` |
| G20-1-the-first-performance-profile-of-this-branch | the first performance profile: startup 3.2x, JIT 239x on matrix, the native boundary at ~141 ns | META | MEAS, **with two falsified findings** | **bannered.** Its GC headline is wrong (`G27-1`, `3765fad76`), its `invocations` §8 claim is not reproducible (`G33-1`), and its binary is `target-fcheck` — the build the tree now says to ignore |
| G21-1-the-handler-setlevel-store-and-how-far-RJdkIntrinsics3-got | the `Handler.setLevel` store | OPEN | MIXED (before MEAS, both modes) | the record that **falsified `G15-1`'s registration-time premise**, and that found `--only=<family>` (§B.6) |
| G22-1-the-slot-collision-and-the-matcher-with-no-method | the field-slot collision behind `Map.isEmpty()`; the matcher with no method | FIXED-UNVERIFIED per the record ("AFTER NOT MEASURED ON A VECTOR") | MEAS (before) | **`RJdkMapViews` green, MEASURED**, at `783685c34`. First record here to read `src.zip` (§B.3) |
| G23-1-the-nominations-that-needed-lib-rs | the nominations that needed `lib.rs`; the three that turned out to be wrong | OPEN | MIXED (vector rewrite FIXED-MEASURED) | states the inverse of the `invocations` trap correctly, before `G33-1` had the mechanism |
| G24-1-the-proxy-return-coercion-on-the-live-path | the proxy return coercion, on the path that actually runs | FIXED per the record; after PENDING-A-BINARY | MEAS (before) | **`RJdkProxy` green, MEASURED**, at `783685c34`. **bannered** for §7.3's "`invocations=0` confirms it" |
| G25-1-the-int-written-into-a-reference-slot | the int written into a reference slot, and the null it actually wrote | OPEN (SOURCE-FIXED / BEFORE-MEASURED) | MEAS | **the authority for the W7-84 correction** (§B.5) |
| G26-1-four-families-of-RJdkIntrinsics3 | four families of `RJdkIntrinsics3`, and the one that turned out to be its own | OPEN | MIXED (before MEAS on both VMs) | |
| G27-1-the-young-collection-that-never-runs | `gen_heap.rs` is not the collector; ZGC is, and has been since 2026-08-10 | META | MEAS (96 runs, every checksum 68332206) | **the record that falsified `G20-1`'s headline** (§B.4) |
| G28-1-the-dst-rule-layer-rebuilt | the DST rule layer rebuilt from the JDK's own arithmetic; 632 zones × 9,480 rows | FIXED — **`RSimpleTimeZoneRaw` GREEN, MEASURED** at `9964ca733` | MEAS | **bannered** for restating `G17-1`'s `invocations = 0` inference. Its "74 divergences → 0" prediction held exactly |
| G29-1-the-fabricated-http-request-and-its-missing-accessors | Mechanism A, instances 4 and 5: `HttpRequest` minted with 3 of 7 accessors | FIXED — **`RJdkOptionalShape` GREEN, MEASURED** at `9964ca733` | MEAS (before) | its §6 worry about the force list is answered by `G34-1`, and was unfounded |
| G30-1-the-silent-reference-slot-coercion | the silent reference-slot coercion, made visible without moving it | OPEN (instrumented) | MEAS (census + runtime population) | with `G25-1`, the authority for §B.5 |
| G31-1-astype-and-the-verifier-that-was-never-asked | `asType` had no convertibility check; a lambda verifier that was invisible | FIXED-UNVERIFIED (own prose: "FIXED-UNRUN") | MEAS (before) | **falsifies `G18-1`'s reading of `MethodHandle.asType`.** `RJdkProxyIface` was still red at `9964ca733`; the fix landed in `2944095fe`, after |
| G32-1-the-four-families-at-their-owners | `fmtobj`, `inet`, `bufslice`, `misc` fixed at their real owners | FIXED-UNVERIFIED (own prose: "before MEASURED, after PREDICTED") | MIXED | landed in `1eb5f8346`, **after** the `9964ca733` binary — not represented in 95/99 |
| G33-1-the-instrument-that-under-reported | `invocations` is a FLOOR; the configuration in which it is exact | META (tooling) | MEAS (causal) + SRC (mechanism) | **the authority for §B.1.** Supersedes two earlier corrections in `BASELINE-20260817` |
| G34-1-who-wins-native-or-bytecode | registering a `Bridge` is by itself the gate under `--jdk-only`; the force list is a later cache-shape override | META (rule) + FIXED-UNVERIFIED (§5 hazard fix) | MEAS, both directions, cold and warm | **the authority for §B.2** |
| G40-1-the-index-reconciled-20260817 | this pass: the listing re-taken at 280, 36 rows added, seven claims reconciled, sixteen banners placed | META | — (documentation only; no `.rs`, no `cargo`, no VM run) | **written after the listing was taken, so it makes the directory 281.** That is the rot, in one row |

---

## B. RECONCILED 2026-08-17 (lane G40) — seven standing claims this session falsified

House convention: each row names the record that asserted the claim, the record
or commit that falsified it, and the class of evidence that did the falsifying.
Where a record's headline is affected it now carries a `RECONCILED 2026-08-17
(lane G40)` banner of its own, in the style the five `aastore` records already
use. **Histories are not rewritten — the banner goes on top and the record's own
account is left exactly as written.**

### B.1 `invocations` in `--dump-native-registry` is a FLOOR. Zero proves nothing.

| | |
|---|---|
| **the claim** | `invocations == 0` shows the body is dead, off the path, or has no constituency |
| **asserted in** | `HANDOFF-20260814` §4 (recommends the tool with no caveat); `G15-1` §"one measurement that outlived its purpose" (*"the registry dump is taken at registration time"*); `G16-1` §8 (`plain_socket.rs` "not on the path"); `G17-1` §1.3 and §4 (*"the base's rows read `invocations=0` … so the narrow registration loses nothing measured"*), restated in `G28-1`; `G18-1` §1 and its `MethodHandle.asType` paragraph (*"so that native is not the live body either"*); `G24-1` §7.3 (*"`invocations=0` confirms it"*) |
| **falsified by** | `G33-1`, and `G31-1` for the `asType` instance specifically |
| **evidence class** | MEASURED, causally isolated on the `783685c34` binary, plus source reading of `record_invocation` |
| **what is true now** | `invocations > 0` proves the body **ran** — unchanged. `invocations == 0` proves **nothing**: the counter is an exact count of *registry-resolved* dispatches and a lower bound on Java-level calls, because `CachedInvokeTarget::Intrinsic` and the JIT's thin direct-call helpers dispatch without ever holding a `NativeMethodId`. `Math.abs` reads **1** for 100,000 calls under `--nojit`, and **100,000** with `CRATONVM_DISABLE_INTRINSICS=1`. The magnitude was never usable. `owns_slot`, `kind`, `registered_by`, `overwrote`, `kind_stated`, `kind_chosen` and the `counts` block are untouched |
| **the exact configuration** | `--nojit` **and** `CRATONVM_DISABLE_INTRINSICS=1`. Every native probed counted 1:1 there |
| **what a reader must do** | any conclusion of the form "this body is dead because the dump says zero" must be re-derived from `owns_slot` plus a behavioural probe. `G31-1` is the worked example of that re-derivation returning the opposite answer |

### B.2 `force_native_over_real_jdk_bytecode` is not the gate

| | |
|---|---|
| **the claim** | a class must be on the force list for its natives to preempt real JDK bytecode; a class absent from the list answers only where the resolved method has no `Code` |
| **asserted in** | `F5-1` §"`resolve_dispatch` step 3" (`CharBuffer` "is in neither … so in real-JDK mode these natives answer for exactly the receivers whose resolved method has **no `Code`**"); `F14-1` §, in the same form; and the doc banner on the Rust function itself, which `G34-1` §5.2 corrects in place |
| **falsified by** | `G34-1` (`9ae371468`) |
| **evidence class** | MEASURED on a real binary against a real oracle, in both directions, cold and warm, with a second, differently-registered binary as a control |
| **what is true now** | under `--jdk-only`, registering a `Bridge` for a triple is **by itself sufficient** to preempt real JDK bytecode. The decision is taken at the first dispatch site that answers, and for nearly every call that site is `try_stackless_invoke` step 1 → `resolve_step1_native`, which runs *before* method resolution and so passes `bytecode_available: false` unless `CRATONVM_ENFORCE_NATIVE_SHADOW` is armed. The force list is a **second, later, cache-shape-only** override, consulted by the vtable inline cache and the JIT — the sites that resolved a bytecode `Method` without asking the registry. Two rows of `G34-1`'s decision table fire in the same run for the same triple from different sites: the answer is **site**-dependent, not triple-dependent |
| **what this lane could not settle** | whether `F5-1`'s and `F14-1`'s *specific* conclusions survive. Their general reachability argument is void; re-deriving each family's answer needs a dump and a probe, which is a Rust-owning lane's work. Their banners say that and no more |
| **still correct** | `C13-2` §, `W4-4` §, and `P2-COLLECTIONS-SHADOWS` §2.3 already described the force list as *reinstating* a default on the warm/cached/reflective/JIT paths rather than as the mode's policy. Those readings are confirmed, not falsified |

### B.3 The JDK's sources are readable on this machine

| | |
|---|---|
| **the claim** | JDK 25 sources cannot be read locally; work from the oracle's behaviour |
| **asserted in** | `G1-1` §provenance, `G4-1` §, `G7-1` §, `G8-1` §provenance, `G9-1` §, `G11-1` §provenance — each in the form "`C:\craton\jdk25src` is absent … so no JDK source was read". `HANDOFF-20260814`'s preamble asserts the *opposite* error: "JDK 25 source is checked out at `C:\craton\jdk25src`" |
| **falsified by** | `BASELINE-20260817` §"CORRECTION: the JDK's sources ARE readable"; first used in practice by `G22-1`, then `G14-1`, `G23-1`, `G28-1`, `G31-1`, `G34-1` |
| **evidence class** | FILESYSTEM, re-verified by this lane: `C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot\lib\src.zip`, **52,462,198 bytes**, dated 2026-04-27. `C:\craton\jdk25src` does not exist — that half of the claim was always right |
| **what is true now** | `unzip -p "$JAVA_HOME/lib/src.zip" java.base/java/net/URI.java`. It is the source of the exact build being used as the oracle. It does not replace the oracle — a source reading can be wrong about what the shipped build does — but it is the difference between transcribing a contract and guessing at one. Two results this session were not derivable from black-box probing in reasonable time and were two minutes of reading: `URI.parseAuthority`'s demotion rule, and `ZoneInfoFile`'s `dstSavings` arithmetic |

### B.4 `moving_young: cycles=0` did not mean a broken young path

| | |
|---|---|
| **the claim** | the generational moving-young collection was requested and never ran, making every collection a whole-heap non-moving pass — the session's headline performance finding |
| **asserted in** | `G20-1` §0 and §4 |
| **falsified by** | `G27-1`, landed as `3765fad76` |
| **evidence class** | MEASURED — 96 interleaved runs, order rotated per round, every checksum `68332206` — plus source: `vm/src/config.rs:769` has defaulted to `GcAlgorithm::Zgc` since 2026-08-10, and `grep -c moving_young gc/src/zgc.rs` returns **0** |
| **what is true now** | `gen_heap.rs` is not the collector in a default run, so its gating predicate at `:5719` is never reached. All three diagnostics the finding rested on mislead in the same direction: `moving_young_requested=true` is a **JIT-codegen capability flag**, not a request for a collection; `"no collection has run yet"` fires because ZGC never calls `record_collector_decision`; the whole `[GC] cards:` block is generational-only and structurally zero. The real result is that **ZGC is 4.37x–6.35x slower than the generational backend** at every heap size, with the ranges not touching. The predicate itself works: 8/8 cycles MOVING when the generational backend is selected |
| **also wrong in that record** | its binary is `C:/craton/target-fcheck/release/cratonvm.exe`, which `G34-1` §provenance now says to ignore outright ("that build partly failed; its timestamp misrepresents its contents"). `G20-1` itself flagged the attribution as only "partly" sound |

### B.5 The `gc::guard` W7-84 warning is not a reference-slot census

| | |
|---|---|
| **the claim** | the ~16 `gc::guard` W7-84 warnings per VM start are a census of native reference-slot violations, and an `Int` written into a reference slot is "silently dropped by the field-layout guard" |
| **asserted in** | the orchestrator's own lane briefs and its `RSslLiveSession` write-up, quoted verbatim in `G25-1` §1 |
| **falsified by** | `G25-1`, re-measured and widened by `G30-1` §2.2 |
| **evidence class** | MEASURED with `CRATONVM_DBG_LAYOUT=1` over a full `--jdk-only` run, then re-measured across six vectors, plus a line-numbered source trace |
| **what is true now** | the int is **not dropped, it is actively written as null** — `NativeContextImpl::set_field` → `VmHeap::set_field_as(.., b'L')` → `coerce_field_value_by_descriptor` (`gc/src/heap.rs:1674`), which maps `Value::Int` / `Value::Long` to `Value::Object(None)`. That coercion is deliberate and documented (tag `S111r29`) and it is **bidirectional**. It is silent unless `CRATONVM_DBG_OVERLAY` is set. W7-84 is a different path entirely: **every** warning in every run is `class_id=ClassId(12) index=0` — one class, one slot, the VM's own class-mirror populator writing over `java.lang.Class.cachedConstructor`. Native reference-slot writes produce **no warning at all**. A W7-84 count is a census of one line of `vm_object.rs` |
| **the real number** | 271 sites, by source scan — a **lower bound** (single-line allocations only). `phases_early.rs` 68, `servlet.rs` 22, `http2.rs` 19, `net_channels.rs` 19, `tls.rs` 18; by class, `ArrayList.elementData` 56, `SocketChannel` 22, `HashMap.table` 19. The systemic fix is a layout-aware field writer, not 271 individual edits |

### B.6 `--only=<family>`, `--list` and `--jdk-only-report` exist and are under-used

| | |
|---|---|
| **the claim** | not a false claim so much as an absent one: every lane so far has worked from "the first failing assertion", because an `AssertionError` aborts the run |
| **reframes** | every record that describes a vector as one assertion from green — most sharply `HANDOFF-20260814` §6.1, which says of `RJdkBridge1` and `RJdkIntrinsics3` that "each cycle is roughly one build. Nothing clever is needed" |
| **established by** | lane G21, recorded in `BASELINE-20260817` §"Process" |
| **evidence class** | MEASURED, on a binary that already existed, before a line was written |
| **what is true now** | the vectors take a family selector: `cratonvm.exe --java-home "$JAVA_HOME" --jdk-only -cp regression-suite/build RJdkIntrinsics3 --only=logrec`. Used on `RJdkIntrinsics3` it showed the vector is **not** one assertion from green: it reaches **800 of 1011** and stops at `tlocal`, with `fmtobj`, `inet`, `misc` and `bufslice` red for four unrelated reasons and `regex` (42) and `mathexact` (57) green. `--list` enumerates the families; `--jdk-only-report` is a complete census that `JDK-ONLY-REPORT-CENSUS-20260812` recorded as unused two waves ago and which is still unused. Any lane blocked at "assertion X, and I cannot see past it" should reach for these first |

### B.7 `SUITE` defaults to `core`, and `SUITE=all` is still not `--jdk-only`

| | |
|---|---|
| **the claim** | a green suite run says something about the `RJdk*` corpus, or about the `--jdk-only` policy |
| **asserted in** | implicitly by every record quoting a bare suite pass count; `HANDOFF-20260814` §3's one-vector recipe does not mention the arms at all |
| **falsified by** | `BASELINE-20260817` §"Three things this measurement corrects", items 2 and 3 |
| **evidence class** | MEASURED, three separate runs, plus `run.sh:330` and `:585` read directly |
| **what is true now** | **three distinct runs, and they disagree.** `bash run.sh` runs `core` only — 61 vectors — and the 38-vector `RJdk*` corpus this whole effort is named after does **not** run. `SUITE=all` runs all 99 but in **Compatible** mode, because `run.sh:330` sets `JDK_ONLY=1` only when `CRATONVM_ARGS` names the flag. The policy arm is a third run: `CRATONVM_ARGS="--jdk-only" bash regression-suite/run.sh`. At the merge the three read 53/61, 84/99 and **88/99** — the policy arm was the *best* of the three, which inverts the framing used everywhere else in this directory that `--jdk-only` is the harder mode |

---

## C. Statuses this session settled — MEASURED, not predicted

The suite under `--jdk-only` went **88 → 93 → 95 of 99** across three attributable
binaries. This is the first sustained sequence in this directory where predictions
made by lanes that could not build were checked against a binary that could.

| | `d2e127930` | `783685c34` | `9964ca733` |
|---|---|---|---|
| passing of 99 | 88 | 93 | **95** |
| failing | 11 | 6 | **4** |

**Vectors closed and MEASURED this session**, with the record each closes out. A
green vector proves the vector's own assertions pass; it does **not**
retroactively promote every claim in the named record from PREDICTED to
MEASURED, and these rows must not be read that way.

| vector | closed at | the record it closes out | note |
|---|---|---|---|
| `RArrayStoreInterfaces` | `d2e127930` | `G12-1` | the `$Proxy` substring blanket in `typecheck.rs`, scoped |
| `RJdkProxy` | `783685c34` | `G24-1` | the return coercion, one level in from where the nomination pointed |
| `RJdkMapViews` | `783685c34` | `G22-1`, `G13-1` | a **field-slot collision**, not the interface doors — in Compatible mode the substitution *succeeded* and reported a three-entry view as empty, so the `AbstractMethodError` was the better outcome |
| `RCrypto` | `783685c34` | `d378eee51` | went GREEN → RED first, for a correct reason: it had only ever been green because `Files.newDirectoryStream`'s filter was never called |
| `RJdkForeign` | `783685c34` | `G19-1`, `G6-1` | |
| `RForeignLayoutJdkInterfaces` | `783685c34` | `G19-1` | |
| `RSimpleTimeZoneRaw` | `9964ca733` | `G28-1`, `G17-1`, `G23-1` | three pieces, in three commits, by three lanes; the fixture had to be rewritten first because the old one compared `104` against `104` and **could not fail**. The rule layer's author predicted 74 divergences → 0 without ever building, and it held exactly |
| `RJdkOptionalShape` | `9964ca733` | `G29-1`, `G13-1` | |

**Still red at `9964ca733` (4):** `RJdkIntrinsics3`, `RJdkBridge1`,
`RSslLiveSession`, `RJdkProxyIface`. None is untouched — each has a fix
committed after that binary was built (`1eb5f8346`, `c703cff68`, `2944095fe`) or
in flight. **No binary has yet measured any of those four fixes.**

---

## D. What this lane could NOT settle

Stated rather than guessed, in this index's own convention.

1. **Whether `F5-1`'s and `F14-1`'s `CharBuffer` reachability conclusions
   survive `G34-1`.** The *argument* they rest on is void. The *answer* needs a
   registry dump and a behavioural probe on the current binary, from a lane that
   may edit Rust. Their banners say the argument is void and stop there.
2. **How many of this directory's "dead body" conclusions rest on a zero.** The
   seven sites in §B.1 are what a targeted grep found; the phrasing varies too
   much for a grep to be a census. Treat §B.1's list as a floor — exactly like
   the instrument it is about.
3. **Which of `G20-1`'s remaining numbers are affected by its binary.** Its
   provenance names `target-fcheck`, which `G34-1` says to ignore. `G27-1`
   re-measured the GC claims on a good binary; the startup, throughput and
   native-boundary numbers were not re-taken. They are not marked wrong here —
   they are marked **unre-measured**, which is not the same thing.
4. **The five records being written as this pass ran** (`G35-1`, `G36-1`,
   `G37-1`, `G38-1`, `G39-1`). They did not exist when the listing was taken and
   have no row. That is not an oversight; it is the rot this block's header
   warns about, observed in the act.
5. **`README.md`'s per-record tables.** Its headline count is corrected; the ~86
   per-record rows below it were not audited row by row and may name records
   that have since moved.
6. **Whether any pre-wave-G record's status prose is now stale for a reason this
   session did not touch.** This pass reconciled the seven claims it was given
   plus what the suite measured. It did not re-read 244 older records, and does
   not claim their statuses are current.

---

# FIFTH PASS — in-session, 2026-08-17. Status only; the tables are NOT rebuilt.

## §E.1 The one number that matters has moved

The FOURTH PASS's §C reads **88 → 93 → 95 of 99** under `--jdk-only`. It is
stale. MEASURED since, same three-arm method, same oracle:

| binary | `--jdk-only` |
|---|---|
| `9ae371468` | 97 of 99 |
| `e7e840264` | 97 of 99 |
| `3fcc8d90f` | **98 of 99** |

**`RSslLiveSession` is green** — 104 checks, empty diff against HotSpot, on a
vector that had never passed in its life. It closed in two steps, `G57-1` (the
carrier had to CARRY the dialled endpoint, because the session is minted long
after the request returned) and `G58-1` (the whole `BaisEvent` mechanism
existed and nothing had ever installed a consumer).

**`RJdkBridge1` is the only red vector left.** It stops in `surrog`, and the
step it stops at is the useful number — its check count is not comparable
across binaries because the vector aborts at its first failure. See
`BASELINE-20260817.md` for both, and for the two harness traps that cost a run
each today (`run.sh`'s default `JDK` does not exist on this host; its
per-vector `timeout` means a parallel build can manufacture a failure).

## §E.2 The listing is 307, and I am not publishing a coverage number

`ls docs/known-issues/jdk-only/*.md` — **307 files**, against the FOURTH PASS's
280. So this index has rotted again, exactly as its own four warnings predicted.

**How much has rotted is not established here, deliberately.** Two scripted
attempts disagreed — 98 unindexed by one measure, 174 by another — because
records are cited in this file three different ways: by full slug
(`G33-1-the-instrument-that-under-reported-20260817`), by bare id (`G1-1`), and
by id-plus-prose. No grep separates "has a row" from "is mentioned in passing
in someone else's row", and both of my numbers conflate them. A number I cannot
stand behind is worth less than nothing in this directory, so there is not one
here.

What IS spot-verified: **`G50-1` and `G60-1` have zero mentions anywhere in this
file**, and the whole `G35-1`…`G60-1` run postdates the FOURTH PASS. A real
sixth pass needs to read each record's status prose, which is what passes one
through four each did and why they took a lane apiece.

## §E.3 Records added since the FOURTH PASS, by id

`G35-1` `G36-1` `G36-2` `G37-1` `G38-1` `G39-1` `G41-1` `G42-1` `G43-1` `G44-1`
`G45-1` `G46-1` `G47-1` `G48-1` `G49-1` `G50-1` `G51-1` `G52-1` `G53-1` `G54-1`
`G55-1` `G56-1` `G57-1` `G58-1` `G59-1` `G60-1` `G61-1` `G62-1` `G63-1`

Three of those are worth reading before acting anywhere in this tree:

* **`G56-1`** — the allocator's "reference types are already `Object(None)` from
  zero memory" comment stopped being true when `Value::Object` gained a
  `NonNull` niche. 1,111 of 1,122 coercion events were that one expired premise.
* **`G59-1`** — with the noise gone, the residue was readable, and it named a
  defect no vector points at: `URL.openConnection()` writing a synthetic slot
  map into a real JDK class. **Two of its four wrong writes were invisible to
  the guard**, because a well-typed value in the wrong slot is not a descriptor
  mismatch. Read this before treating a quiet coercion log as a clean one.
* **`G63-1`** — `map.values().iterator()` is **not fail-fast**, in BOTH modes.
  The view is real (`HashMap$Values`); its iterator is a snapshot `ArrayList$Itr`
  handed over by the `java/util/Collection.iterator` interface door, so a
  structural modification mid-iteration throws `ConcurrentModificationException`
  on HotSpot and nothing here. `keySet()`/`entrySet()` are correct, which is what
  localises it to the door. Found by disbelieving a measurement in `G60-1` — that
  record had attributed the same exception to `java/util/ArrayList.iterator` and
  held four retirable triples back on it.
* **`G60-1`** — **RESOLVED 2026-08-17, moved to
  `G60-1-what-jdk-only-still-overrides-RESOLVED-20260817.md`.**
  `--jdk-only` counts its own violations and nobody had read the count. 0 classes
  fabricated, 0 synthetic stubs run. Read the resolved record before quoting any
  of its numbers: **the "81 natives that won over real JDK bytecode" was 58.**
  The other 23 rows are recorded on the YIELD path — §1.4 enforced, the bridge
  losing to real bytes — and the report gave them a `summary` saying the
  opposite, which is now fixed and carries an explicit `outcome` field. A real
  application (embedded Tomcat, booting and serving under `--jdk-only`) puts the
  population at **521 native-won triples**, so one vector was about a ninth of
  it, and the report now says out loud when its own list is truncated.
* **`G69-1`** — 82 rows where **the exception type and the precedence were both
  already exact and the sentence was wrong on every one**. A reflective field
  refusal is diagnosed by reading its message, and ours named neither the field
  nor the value (`Can not set static final field via Field.set: Field typed
  setter`). Five grammars, three of them deliberately non-uniform — the
  bad-receiver form carries `final`, the conversion form drops the modifiers and
  quotes the name, and the generic `set` names a bad RECEIVER where every other
  row names the value. Also the one thing nobody was looking for: **an array's
  `class_id_of_object` is the COMPONENT's id**, so `int[]` printed as
  `java.lang.Object` and `String[][]` lost a dimension. `Object.getClass()` had
  always computed the descriptor itself; that computation is now shared. Read §8
  N1 before trusting any of the other 306 sites that name an object by its class
  id, and §7 before believing a `0 of 100` suite run.
* **`G70-1`** — the `native-collections` units refactor, **done whole**.
  Seventeen `toString` families all bottomed out in one function that returned
  a Rust `String`, which cannot hold an unpaired UTF-16 surrogate: **21 of 29
  probe rows diverged**, where `G63-1` had sampled three. Two corrections to
  that record's plan — it needs TWO trait methods (a lossless read written back
  through a lossy constructor is still lossy), and `init_string_from_units`
  cannot stand in for the second because it assumes the `char[]` layout a real
  JDK String does not use. Then the part worth reading: **two rows survived the
  whole-family fix**, because real JDK `AbstractCollection.toString` is a
  `sb.append(e)` loop and `StringBuilder.append(Object)` was still calling the
  lossy reader — whose units-exact twin was built by `G26` and sits directly
  above it, unused. A family fix that stops at the crate boundary is not whole.
* **`G71-1`** — surrogate sweep 8, and the first sweep whose NEGATIVE result is
  the bigger half: **38 of 42 rows were already exact**, including every
  `StringBuilder`/`StringBuffer` `append`/`insert` overload, `Base64`,
  `URLEncoder`, `Collator`, `MessageDigest`, `java.time` and
  `chars()`/`codePoints()`. `G70-1` N2 supposed those siblings were suspect;
  they were not. The four that failed share one shape — **the value never IS a
  String**, so the units path that already carries a String argument was never
  on: a boxed `Character` (which rendered `?`, not U+FFFD), `%s` of any
  non-String object, and `CharBuffer.wrap(CharSequence)`, which lost the units
  twice in one branch. `Scanner.nextLine()` is the fourth and is **left
  unfixed and unrowed** — its buffer is host text end to end. Also read §
  "toolchain" before trusting a build failure here: `-C lto=fat` crashes rustc
  on this tree AND on unmodified HEAD, so a red build is not evidence about a
  change.
* **`G72-1`** — sweep 9 on the exception-contract axis `G68-1` N3 and `G69-1` N3
  both nominated, and it **could not finish**: `Object.wait(-5)` on a held
  monitor **blocks forever** where HotSpot throws `IllegalArgumentException`, so
  29 of 33 rows have never been measured on CratonVM. A negative timeout is not
  a long wait, it is an error — and `Thread.sleep(-1)` silently succeeds for the
  same missing check while `Thread.join(-1)` right beside it is correct. A hang
  is worse than a wrong value: it presents as "the application stopped", far
  from the call. NOT FIXED. The oracle column for all 29 unmeasured rows is in
  §4 so the next pass is a diff, not a measurement exercise.
* **`G73-1`** — sweep 10 closes the exception-contract axis `G68-1` opened:
  `Method.invoke` and `Constructor.newInstance`, 31 rows, **23 already exact**
  — every `InvocationTargetException` wrapping, the access checks, argument
  widening/narrowing, and the whole `java.lang.reflect.Array` family. Four
  message defects fixed, and the one that is not a message: **`Constructor`
  dropped the CAUSE that `Method` attaches**, so the same bad argument produced
  different exceptions through the two doors and Spring's
  `getCause() instanceof NPE` branch took the wrong arm. The logic was inline
  in one and absent in the other; it is a shared helper now. Two residues are
  left deliberately unfaked — their text names JDK internals
  (`sun.invoke.util.ValueConversions`, a module/loader-qualified
  `ClassCastException`), and inventing that is not transcription.
* **`G74-1`** — two nominations CLOSED by measuring instead of assuming, and a
  retraction. **`G69-1` N1** (306 sites naming an object by its class id, which
  on an array is the COMPONENT's) is a mostly-NEGATIVE result: sweep 11, 51
  rows, **50 already exact** — names across dimensions, component types,
  assignability, `forName` round trips, `reflect.Array`, clone, arrays inside
  collections. One live site, `Class.cast`, fixed. The premise was right and
  the scale was wrong; do not re-audit 306 sites expecting a harvest.
  **`G71-1`'s Scanner residue is DECIDED, not deferred**: its source is an
  `Arc<str>` and the tokenizer runs a Rust regex engine over `&str`, so units
  would need a hybrid representation — the cost is the regex boundary, not the
  buffer. **RETRACTED: the fat-LTO build was never broken.** Four commits say a
  release binary could not be produced on this host; that was `G72-1`'s own
  leaked `cratonvm.exe`, and `-C lto=fat` builds clean once it is gone.
* **`G75-1`** — `URI`/`URL` components and unpaired surrogates, measured on a
  26-row probe and split into two defects that looked like one. **N2 is FIXED**:
  `URL.toString()`/`toExternalForm()` rebuilt the external form as host text,
  while `getPath()`/`getFile()` were exact — under `--jdk-only` the real JDK
  constructor fills the components and only the reconstruction is ours. **N1 is
  measured, costed and deliberately NOT started**: the URI parse loses the unit
  three steps upstream of `url_parse`, in ten callers that read their arguments
  with `read_string`, and a half-converted parse mis-slices EVERY URI rather
  than only the ones with surrogates. Read §3 before reaching for the
  positional-substitution trick — it works and it is a trick. Read the N2 entry
  before editing any `URL`/`URI` native: I fixed the wrong twin first, and the
  registry dump would have said so in one command.
* **`TIMEOUT=420` — I ran a whole session without it and got away with it until
  I did not.** `HANDOFF-20260812` says it is not optional on this host
  (`RMapGcStress` needs ~4m55s against a 120 s default) and a retired record
  calls the vector "a load flake [that] read as a result in both directions".
  On a non-LTO binary it fit under 120 s and every arm was green; on the
  fat-LTO binary it straddled the line and failed 2 runs of 4, which reads
  exactly like a regression from whatever you just changed. It is not. Set
  `TIMEOUT=420` on every arm, and when a GC-stress vector fails intermittently,
  check the timeout before the diff — the documented answer was already written
  down and the cost of not reading it was an hour.
* **`G76-1`** — sweep 13, a fresh axis: what a failed PARSE says and what a
  closed or out-of-range STREAM does. 48 rows, **42 already exact** (every
  integral/radix/BigInteger/BigDecimal message, read-after-close on four stream
  types, mark/reset). Six defects, two of them not messages: **`new
  ByteArrayOutputStream(-1)` succeeded** — the guard `if v > 0` made "negative"
  and "unspecified" the same case — and a bad read range raised
  `ArrayIndexOutOfBoundsException` where HotSpot raises the BASE
  `IndexOutOfBoundsException`, which survived precisely because `AIOOBE extends
  IOOBE` and every `catch` still matched. The float parsers say `empty String`
  where the integral ones say `For input string: ""`; both are vector rows, so
  the difference cannot be simplified away. `EOFException` carries a null
  message — an empty message is not the empty string, the same distinction
  `G72-1` needed.
* **`G77-1`** — two sweeps that found **nothing**, recorded because the
  alternative is somebody probing them again. String bounds messages, case
  mapping (including `ß`→`SS`, the `ﬁ` ligature, final sigma, Turkish `i`/`I`,
  surrogate pairs and lone surrogates) and comparison: **44 rows exact**.
  Serialization round trips, back-reference identity, `transient`, every
  refusal, and the stream header: **20 rows exact**. Six preceding
  contract sweeps found a defect within the first ten rows every time; these
  two are unrelated to each other and to those six. The cheap message-shaped
  hunt has reached diminishing returns — see §4 N2 for the three question
  SHAPES never asked (concurrency, GC pressure, scale), which is where a next
  sweep should go rather than at another subject.
- [G78-1](G78-1-the-read-string-audit-and-the-file-that-merged-20260818.md) — the `read_string` caller audit: 2904 grep hits narrowed to 18 by dataflow and registry measurement; `java.io.File` lost the unit three times over and MERGED two distinct paths in equals/hashCode/compareTo. Closes G70-1 N1. 18 rows -> 2.
- G79-1 (retired: `G79-1-the-census-that-was-said-not-to-exist-20260818`) — the P0 over-tagging census EXISTS today (`--dump-native-registry` + a class-loading probe). `native-awt` measured: 187 registrations not 122, 22 genuine bridges not 10, and the "names absent methods" count is inflated ~4x by inheritance and placement. The suite has ZERO AWT coverage, so the arms cannot adjudicate a retag.
- G80-1 (retired: `G80-1-the-first-awt-measurement-20260818`) — the first AWT differential measurement: 10 of 25 headless rows diverged, 7 fixed (opaque-image alpha lost on read, an invented AIOOBE message, three unregistered `Graphics` methods raising AbstractMethodError). The 3 left are one design decision: `BufferedImage.getRaster()`/`getColorModel()` return null. Changes the P0 retag order.
- G81-1 (retired: `G81-1-the-first-closed-row-20260819`) — **the first CLOSED row in the table**: P2 "Headful AWT" → CLOSED(5), test named. 13 of 14 headful operations were ALREADY conformant while the row sat OPEN; the fourteenth threw an NPE, which fails rule 5 too (it demands a specification-consistent error, not any error). Route was written in the row's own required-resolution column all along.
- G82-1 (retired: `G82-1-the-run-that-closed-a-p0-row-20260819`) — **the first CLOSED P0 row**: *Real boot-image requirement* → CLOSED(5). It was waiting on nothing but execution — its own provenance note admitted "no cargo command was run this session". Four legs run and recorded; both cited tests now actually pass. Also corrects the row: there are TWO refusal messages, and the commoner user mistake gets the less informative one.
- G83-1 (retired: `G83-1-the-ratchet-was-already-red-20260819`) — the stub ratchet a P0 row cites as evidence has been FAILING (1308 vs a frozen 1277, SLACK 0), pre-existing and unnoticed because nobody ran it. The row's `157` matches neither baseline nor either measured population (1330 default / 0 strict). The strict zero is a DROP, not a resolution — and retagging a fake to Bridge improves both metrics while making the VM less correct.
- G84-1 (retired: `G84-1-what-the-strict-report-actually-says-20260819`) — `--jdk-only-report` counts exactly what three P0 rows argue about, and none of them cites it. One strict run: `compatibility_classes: 0`, ONE compatibility-class request in total (`cratonvm/stream/LazyOp`), `native-shadows-bytecode: 104`, `interpreter_shadow_unenforced: 77`. Explains why I did NOT close the fallback-policy row despite the evidence.
- G85-1 (retired: `G85-1-six-retags-that-were-no-ops-20260819`) — **CORRECTION**: six native-collections retags were NO-OPS (an inner explicit `set_category(Bridge)` overrode the call-site wrapper), so the session's real over-tagging figure is 54, not 433. Green arms and 127,920-check spot-checks cannot distinguish a safe retag from an inert one; the missing test was re-dumping the registry. Keeps the NotDeclaredSplit findings, which stand independently.
- [G86-1](G86-1-the-two-lists-are-one-and-already-dead-20260819.md) — the P0 *Duplicate dispatch* row's "two lists that disagree" were centralised into ONE predicate and the StringJoiner exception retired on 2026-08-04; and under `--jdk-only` the list cannot fire at all (its guard needs a SyntheticStub, of which strict mode registers zero). Does NOT close the row — the JIT is a third location and is untouched.
- [G87-1](G87-1-the-jit-third-location-measured-20260819.md) — the P0 *Duplicate dispatch* row's JIT "third location", measured under strict: 0 direct native binds, 0 inline-cache natives, and 38 fast-path admissions REFUSED (the field name reads as the opposite of what it counts). Live in `--real-jdk`, neutralised in `--jdk-only`. Does NOT close the row.
- G88-1 (retired: `G88-1-the-state-map-that-gates-the-row-20260819`) — retagged eight collection registrars at once to find out which classes carry REAL state: **all five collection vectors broke**. VM-owned state is pervasive, not a LinkedHashMap quirk. The safe-retag rule in one line: stateless surfaces or real-state objects retag; VM-owned containers do not. Shows four P0 rows are facets of ONE project — the VM owns state belonging to real JDK objects.
- G89-1 (retired: `G89-1-the-red-ratchet-named-and-re-lit-20260819`) — the stub ratchet had been failing in BLOCKING CI since 2026-08-14, so for five days it adjudicated nothing. All 109 excess stubs named at three commits. **The finding is a second column**: a relabel keeps its row and moves only its kind, a new fake adds one, and a single frozen count cannot separate two opposite-signed events. 78 relabels (registry unchanged at 12792 rows), 31 inherited and enumerated. Plus a scope defect — the file freezes two constants and CI ran one, the unrun one being the SHIPPING resolve.
- G90-1 (retired: `G90-1-the-dial-and-the-arm-227-shadows-retired-20260819`) — **227 §1.4 shadows retired.** The P0 over-tagging row's `104` is one vector's and counts `bytecode-won` successes with `native-won` failures; the defect over 36 vectors is 980, itself a floor. `CRATONVM_ENFORCE_NATIVE_SHADOW` takes a PREFIX list and nobody had swept it — the documented whole-VM catastrophe (1/36) is real and **not evenly distributed** (seven prefixes at 36/36). The finding worth more than the 227: the 36-vector screen passed two prefixes the 102-vector arm rejected, because the screen corpus asked those subsystems nothing.
- [HANDOFF-20260819](HANDOFF-20260819.md) — where the goal stands, the four-step method that produced G90-1, the instruments and what each lies about, how to run the arms without losing an hour, and the standing traps.

## FIFTH PASS — wave H, 2026-08-20

Five records, added on the day they landed rather than waiting for a rebuild
pass. The four previous passes all discovered that the wave then in flight had
no rows; this block exists so wave H does not repeat it. **It will rot the same
way** — wave H is not finished.

**Read `H1-1` before quoting any shadow count in this directory.** Every
`native-shadows-bytecode` figure published before 2026-08-20 came from a sink
that capped at 256 and announced saturation only through a boolean nothing read.
Measured with the cap lifted, 104 vectors, `saturation: none`: **1403
`native-won`**, not the `943` the P0 row and `HANDOFF-20260819` §6 carry. That
is not a regression and not 460 new shadows — it is a censored measurement
replaced by an uncensored one, which also means `G90-1`'s `980 → 943`
improvement is a difference of two numbers neither of which knew its population.

- [H0-1](H0-1-the-jmx-pin-and-a-jdk-that-was-not-there-20260820.md) — `FIXED-UNVERIFIED` · MEASURED (§1 only). The oracle JDK is **not** at the Adoptium path this directory names in 66 records; that directory does not exist on this host. Both occurrences in `HANDOFF-20260819` corrected; the rest left as dated snapshots, with the rule *resolve a JDK path, never copy one*. Plus the JMX P0 row's step 1 (the two ambient registrars pinned) and the "convert" half of step 2 (`ObjectName`'s canonical-name slot resolved by NAME, not index 0). **Corrects the row twice**: `register_as` has never existed in this tree, and "any write past slot 0 is silently discarded" is stale — the allocator already widens to the real field count.
- [H0-2](H0-2-the-carrier-that-advertised-a-class-it-does-not-have-20260820.md) — `OPEN` · **MEASURED**, no source change. `Map.of(k,v)` and `Collections.unmodifiableMap(…)` are **the same carrier class**, told apart only by a marker field that `getClass()`'s display rule reads and the `INSTANCEOF` opcode does not. So the published remedy — give the stamp `AbstractMap` as a superclass — **cannot work**: it would fix one and break the other, which HotSpot answers `false`. **The 219-check vector asks 1 of 12 divergent cells**; the 11 it misses include `instanceof RandomAccess` false on `List.of(…)`, which `Collections.binarySearch`/`reverse`/`shuffle` branch on. Specification for the wave-2 cluster retag, not a complaint.
- [H1-1](H1-1-the-sink-that-capped-every-count-20260820.md) — `FIXED-UNVERIFIED` at writing, **since MEASURED** (census live, strict arm 104/104). Two truncations, not one: the producer and the consumer were gated on the same `FULL` flag, so once the sink filled the VM stopped *walking* for shadows and `interpreter_shadow_unenforced` froze — a drop counter alone would have counted nothing. Cap 256→4096, drop counter, `truncated`, `CRATONVM_NATIVE_SHADOW_SINK_CAP`, and the census now prints on every strict arm. **Also: the P0 *Native-first dispatch* row's premise expired 2026-08-04** — `COPY 1 OF 2` is gone (`b90f9a614`), there is ONE predicate with twelve entries, and `real_protected_stub_class` is not in the file the row names. The two-column table the row asks for would have had two provably identical columns; source-witness tests freeze the one-predicate property instead.
- [H2-1](H2-1-the-filetime-epoch-and-the-queue-lock-20260820.md) — `FIXED-UNVERIFIED` at writing, **retirement since verified live** (all eight triples `[JDK-ONLY-REFUSED]`, `RFileTimes` PASS 68 checks). `WindowsFileAttributes` stores **Windows FILETIME**; CratonVM wrote Unix millis into those fields and read them back the same way — self-consistent, agreeing with nothing. 1609459200000 ticks *is* `1601-01-02T20:42:25.920Z`. Eight `sun/nio/fs/` shadows retired. **`java/lang/ref/` deliberately NOT retired, against the brief**: every `discover_reference` call lives in a subclass CONSTRUCTOR native and nothing in `gc/` scans the heap for references, so retiring the prefix disables weak/soft/phantom discovery outright — which is the `false` `RClassUnloadSweep` reported. A negative gate now fails if anyone acts against that.
- [H3-1](H3-1-the-ratchet-that-did-not-compile-20260820.md) — `FIXED-UNVERIFIED`. **`native-builtins/tests/stub_ratchet.rs` has not PARSED since merge `26e4b5db4`**, which spliced two versions of one failure message and kept both argument lists. Independently reverified: both parents parse, the merge has 32 parse errors, and **`origin/dev` still carried the break at `d8b40ff8f`**. That gate is blocking CI in both configurations and is the cited evidence for the P0 *Residual synthetic native set* row. Also: seven `java.util.function` default-method stubs deleted (reachability **measured**, not argued), the bridge ratchet given the two-column rule and a mode-qualified baseline key — the old key let a strict census overwrite the compatible baseline, so the strict registry could not be given a baseline at all. **Corrects its own brief**: the six `Runtime.exec` overloads are not in `native-io/src/process.rs`; `javap` shows none is `ACC_NATIVE`; all six kept, so the delta is −7 not −13.
- [H4-1](H4-1-the-cluster-that-is-not-a-tag-20260820.md) — `OPEN` · **MEASURED, analysis only, zero behaviour change.** **`NativeKind` cannot move the map/set cluster at all**, so `G88-1` §5's "the unit is the cluster, not the registrar" stops one level short. `SyntheticStub` acts in exactly ONE place — `register_inner`'s `JdkOnly` arm — and removes a *registration*, not a Rust function. Three populations bypass it: **168 direct Rust calls** into these natives from 18 files in two other crates, **45 `try_alloc_concurrent_synthetic("java/util/HashMap")` sites** that allocate under the REAL class id and then fill through those calls, and **6 JIT direct helpers**. After a retag those objects meet real bytecode over a real, empty `table` and answer **absent** — a silently empty map, no error. §5's three green vectors construct no map through any of the 168 sites. **Also: contract §8 was never the blocker for Properties/Hashtable** — `native-builtins/src/lib.rs` holds ZERO such registrations (re-verified: 2 occurrences, neither a registration); the 67 are in `properties_sidetable.rs` (35), `deprecated_util.rs` (12), `deprecated_io_util.rs` (7), `wildfly_naming.rs` (3). `G88-1` §6 conflated the crate with the file. Its falsifiable prediction was tested and HELD (see `H0-2`'s correction banner).
- [H5-1](H5-1-the-abstract-registrations-are-fabricated-receivers-20260820.md) — `FIXED-UNVERIFIED` (one deletion) · **MEASURED** (the census and the family map). **The P1 *NIO, files, networking* row's remedy is wrong**: the natives on `java.nio.channels.*` **cannot** move to the `sun.nio.ch.*Impl` classes, because the VM fabricates instances whose class name **is** the abstract class — 13 named allocation sites — so moving them strands every receiver. The defect is one layer up: the VM instantiates abstract classes. `Pipe`, the row's own example of misplacement, already registers on BOTH. Row totals re-derived: **106 / 656 / 222 / 367 / 18**, against the published `86 / 307 / 105 / 91 / 54`. The duplicate `java/io/FileInputStream.read([BII)I` registration is deleted — and the comment defending it was false, claiming two different callbacks where both named `native_fis_read_bytes`. Census of 1,113 call sites: 9 same-function duplicate groups, 27 cross-function, of which **two are real defects** (`sun/nio/ch/UnixDispatcher.close0`'s callback is dead — a 10-line comment describes a body that never runs; and `CRATONVM_REAL_RAF` cannot reach `RandomAccessFile.getFilePointer` because the registrar runs six lines after the gated block). H5-C's premise was stale: all 29 registrars already pin their own category, and two of the five "confirmed stubs" (`scanner`, `data_stream`) are `Bridge` and strict-mode LIVE.
- [H6-1](H6-1-the-canonical-name-slot-holds-a-different-string-20260820.md) — `FIXED-UNVERIFIED`. **Live heap corruption fixed**: `init_runtime_mxbean_fields` wrote slot 0 ← `Object` and slot 1 ← `Int` into a real `java.util.ArrayList`, whose JDK 25 layout is `0 modCount(int) | 1 elementData(ref) | 2 size(int)` — an `Int` in a reference slot the collector marks and moves. The file's other two `ArrayList` allocations already wrote by name, so the exception was invisible to a whole-file grep. **Settles the `ObjectName` question with an oracle run rather than by effort**: `javap` gives **zero** native methods on that class, `ObjectName$Property` holds INDICES into `_canonicalName` rather than data, and HotSpot's `_canonicalName` is the **sorted** string where `jmx.rs` stores the **source** string — so hand-filling the three fields is *unsound*, not merely laborious, and would turn today's NPE into a silently wrong substring. Declined the half-migration; landed an oracle-verified real bug instead (`getSerializedNameString` returned canonical where HotSpot returns source). **Corrects `H0-1` N2**: `ThreadInfo`/`LockInfo`/`MonitorInfo` are already 100% by-name and the "~40 sites" figure is an order of magnitude off — of 141 field accesses only four clusters address a real layout, the rest are interface-stamped receivers with no real fields. **Does NOT close the P0 row**, and the binding item is independent of all code: closure rule 1 needs a *reviewed* bridge, and no `jdk-only-native-review.md` §4 review has been run on the 203 JMX bridges.
- [H0-3](H0-3-the-collection-cluster-is-not-a-leaf-20260820.md) — `OPEN` · **MEASURED.** Arming `CRATONVM_ENFORCE_NATIVE_SHADOW` on **one** prefix, `java/util/concurrent/ConcurrentHashMap`, takes the strict arm **104/104 → 93/104**. The finding is WHICH eleven: only three are collection vectors; the rest are `RCrypto`, `RJdkSecurity`, `RJdkX509Intercept`, `RJdkLogging`, `RJdkModule`, `RJdkProxyIface`, `RJdkEnumerations`, `RServiceLoaderDoubleSource`. Crypto provider chains, the logger registry, the module graph, the proxy cache and service loading are all built on a map whose contents the VM owns. **Superseded in degree by `H0-4`: `HashMap` is the floor, CHM is one storey up.**
- [H0-4](H0-4-the-blast-radius-table-20260820.md) — `OPEN` · **MEASURED, six runs.** **The blast-radius table**, which prices a retirement before anyone attempts it, for one env var and no build: `HashSet` 103/104, `Hashtable` 101, `LinkedHashMap` 97, `TreeMap` 97, `ConcurrentHashMap` 93, `HashMap` **81**. **The families are not equally entangled and nobody knew that** — a spread of 23×, where every prior argument treated "the collection cluster" as one body. §4 then diagnoses the one row common to four families: `RMapGcStress` is **one defect with four faces** (`iterated 1 != 3000`; a lost value; an NPE from `Iterator.next()`), real bytecode iterating a table the VM never populated. Net of it the costs are **0 / 2 / 6 / 7 / 10 / 22**, giving a migration order — `HashSet` first, `HashMap` last — that **inverts** where the P0 row currently queues them.
- [H7-1](H7-1-the-second-door-into-the-map-and-the-guard-that-named-the-wrong-class-20260820.md) — `FIXED-UNVERIFIED` · arms since verdict-neutral. **Corrects `H4-1` O1 twice, in opposite directions**: none of the six JIT helpers reimplements anything (five *call* the registered function; the sixth is a one-line re-export), so this is not the `E18-1`/`E27-1` duplicate-rule species; and they are not past the kind check — four are refused at **bind** time one crate away, which a call-site grep cannot see. Three rows did disagree: a CHM get answering `null` for an unrecognised key address where its own sibling defers; a non-leaf native called with no funnel (the defending comment claimed the native's own pinning made the wrapper redundant — pinning *is* a service of that wrapper, which also supplies the `NativeRunning` transition the STW census waits on); and a put whose out-of-contract arm re-dispatched **after** the overlay insert, returning the value it had just written as the previous mapping. **The finding that changes the plan**: two ladders hand the guard the CALL SITE's interface while running the IMPLEMENTATION's native — correct today only because all four rows are tagged `bridge`, and retagging them is exactly what `H4-1`/`H0-3` propose.
- [H8-1](H8-1-three-declines-that-were-not-declines-20260820.md) — `FIXED-UNVERIFIED` · arms since verdict-neutral, census byte-identical as predicted. Three `native-io` defects `H5-1` localised. The dead `UnixDispatcher.close0` was **not** a harmless duplicate: it closes an `fd_table()` entry where the winner closes a `net_sockets()` entry, and `next_net_fd()` starts at `0x4000_0000` *specifically so the id spaces cannot collide* — so the "fallback" the comment defended would have left every OS socket open while zeroing `fd`/`handle`. `CRATONVM_SYNTHETIC_RAF=1` made **`getFilePointer()` return a constant `0`** for every `RandomAccessFile` while seek/length/read worked, because exactly one method escaped the gate — structurally, being the only member of "public API" ∩ `ACC_NATIVE`. And `native_scanner_close` claimed a void call it did not perform; **no receiver reaches it today**, so that one is a corrected backstop rather than a repaired leak, and the record leads with that. **Seven in-tree comments found wrong**, one with its gate polarity contradicting the doc comment eight lines above it.
- [H0-5](H0-5-two-mechanisms-under-the-hashmap-blast-radius-20260820.md) — `OPEN` · **MEASURED**, with a control that changed what two rows mean. `H0-4` left open whether the 22 `HashMap` failures were one defect or several. **They are not one.** A bare unarmed/armed pair per vector shows **20 of 22 are cleanly dial-caused**; the other two are not diagnosable bare — `RJdkModule` fails bare with or without the dial (it needs the module-path args `run.sh` supplies) and `RJdkLogging` passes bare in both. **I nearly recorded a wrong cause for `RJdkModule` from a reproduction that was not about the dial at all.** Two dominant mechanisms: **A**, four vectors dying on `Cannot read field "modCount" because "this.this$0" is null`; **B**, four naming a fabricated `cratonvm.synthetic.AnonymousObject$4` at a real array store or cast. §6 measures A's reach — it hits `LinkedHashMap` and `Hashtable` but **not `TreeMap`**, so `TreeMap` does not ride along on that repair. **§3 carries a correction against itself**: I called A "the single most actionable finding" as though it were an oversight, then grepped and found the tree documents the null `this$0` as *deliberate containment* in three places, one added by `H4-1` the same day. The wrong sentence is left quoted rather than deleted. **Superseded on the mechanism question by `H0-6` §7: A and B are one root.**
- [H0-6](H0-6-the-fabrication-surface-is-growing-20260820.md) — `OPEN` · **MEASURED.** Answers `H0-5` N2 and then overturns `H0-5`'s own framing. **`AnonymousObject$N` is never allocated — it is SUBSTITUTED**, at one site in `alloc_object`: a native calls `alloc_object(ClassId::new(0), N)`, having *resolved a class and failed*, and the VM swaps in a synthetic class of exactly `N` fields, which the caller then hands out as an instance of the class it named. **`AnonymousObject$4` IS the `HashMap.Node`** — `{hash, key, value, next}` — proven by the failure landing in real `HashMap.resize()` storing into a `Node[]`. **So `H0-5`'s two mechanisms are one root with two faces**: the VM owns `HashMap`'s internal representation, and real bytecode trips on the *view* (null `this$0`) or on the *node* (wrong class at an array store). One repair, not two. **The substitution is why no census ever counted it**: it makes width and class_id agree, so a width census sees perfect health — third instance in two days of an instrument reporting health because of the defect's SHAPE. **The surface is growing: 49 → 84 production sites in eight days**, 24 of them in `util_concurrent_ext.rs` and only **4** in `native-collections`, the crate the migration plan is organised around. Also: strict mode does **not** refuse the substitution (contract §1 item 6 permits it in both modes — my own N4 guess, measured wrong); and the one instrument that could attribute these allocations, `CRATONVM_DBG_ANONALLOC`, **attributes 97 of 2204 events — 4.4%** — with `--nojit` giving an identical 97, so the obvious tier-up explanation is **disproved and the cause unknown**.
- [H0-7](H0-7-the-two-vectors-bare-runs-could-not-see-20260820.md) — `OPEN` · **MEASURED, seven runs.** Answers `H0-5` N4: the two vectors bare runs cannot diagnose, done through `ONLY=` so `run.sh` supplies their launch args, **with an unarmed control every time** (both PASS unarmed). Armed on `java/util/HashMap`, `RJdkLogging` survives to diff and **76 of 79 checks are right**; the headline diff is `zoneAgrees=threw:ZoneRulesException`. **§7 then corrects §3 against itself**: arming `java/time/` and `java/time/zone/` directly leaves the vector GREEN, so `java.time` is a **CONSUMER of the `HashMap` defect, not a new family** — evidence of reach into a package with no natives in the picture, rather than another migration target. Two further findings: **CHM is worse than `HashMap` for these two vectors** while being cheaper in aggregate, so `H0-4`'s table orders FAMILIES and **must not be read as a per-vector priority**; and the two `HARNESS ERROR [G2]/[G3]` lines the armed run prints, which read literally as *a vector in the strict 104 that asserts nothing*, are **crash artifacts** — the guard runs on every invocation and is silent unarmed. Checked precisely because the literal reading was the more interesting one.
- [H9-1](H9-1-hashset-owns-no-state-20260820.md) — `FIXED-UNVERIFIED` · **the first migration attempt of the wave, and it inverts the published order.** `HashSet` has ONE instance field (`transient HashMap map`, slot 0) and every method is a one-line forward onto state the VM still owns — so `H0-4`'s measured cost of **net 0** is structural, not luck, and **`HashSet` must move BEFORE `HashMap`**, not after it as the P0 row queues it. The one thing `HashSet` does own is the membership marker and it was wrong: real `remove` is `map.remove(o) == PRESENT`, an **identity** test, and the VM wrote three non-`PRESENT` markers, two of them null. **INDEPENDENTLY VERIFIED by lane H0 on the pristine binary before the fix was built** — armed, `new HashSet<>(asList(…)).remove("b")` and `Collectors.toSet()` both **remove the element and return `false`** where HotSpot returns `true`; 2 of 5 population shapes, not all six sites, and **invisible unarmed** because the VM's own reader is self-consistent with its own wrong marker. Also corrects the population/consumer counts it was briefed with (27 producers not 19; 16 consumers not 168, 15 of them through one helper; **zero** JIT helpers touch the set surface, so `H4-1` O1 does not gate this family). Three out-of-file edits remain blocking, incl. a **second** `HashSet.spliterator()` registration and `canonical_concrete_for_interface` minting `HashSet` receivers for `Set`/`Collection`.
- [H10-1](H10-1-three-instruments-and-a-parse-verdict-of-its-own-20260820.md) — `FIXED-UNVERIFIED` · three instruments, and **a blocking CI gate that was structurally unable to report the thing it was blamed for not reporting.** `rustfmt --check` exits 1 both for a formatting diff (**stdout**) and for a parse error (**stderr**), and `ci.yml`'s `fmt` job reads only the exit code — so `H3-1`'s 32-error unparseable merge looked exactly like the 59% of the tree that merely prints a diff. **VERIFIED by lane H0 independently**: the broken `stub_ratchet.rs` at `26e4b5db4` gives exit 1, **0 bytes of stdout, 32 `^error` lines on stderr**, and 32 is `H3-1`'s number reached by a second route. The new `merge-parse` job reads **stderr only**, runs **in place** (both parents report bogus `failed to resolve mod` out of tree), and is a separate JOB not a step — because `fmt`-first is what kept clippy off `dev` for weeks. Also `RJitMapTierDiff` (25 map shapes, each read at its first interpreted invocation and again after tier-up, **made to fail on purpose in both directions**) and the blast-radius matrix as a weekly non-blocking job whose adjudicated cell is **the failing SET, not the pass count**, netted against an unarmed control arm, printing no total ever because the prefixes are not disjoint. **The corpus is now 105** — `harness-selfcheck.sh SUITE=all` re-run by lane H0: *105 vectors sound, 0 flagged*. Every published `104`/`64` denominator shifts.
- H11-1 (retired: `H11-1-native-dispatch-keys-on-the-receiver-not-the-call-site-20260820`) — `OPEN` · **MEASURED — answers the question `HANDOFF-20260820` §7 item 6 calls the highest-value probe in the effort, and one answer settles ~200 abstract-class registrations.** **Native dispatch keys on the RECEIVER's class, not the constant-pool class named at the call site.** Taken twice independently (source through `invoke_class` → `resolve_step1_native`, and four discriminating runs byte-identical to HotSpot): a user `AutoCloseable`'s own `close()` runs; a `MyFile extends File` override beats the registered `java/io/File.length()J`; a user `DataInput` implementor returns its own value. `H5-1` N1's exact probe run: a `Pipe` driven through `SourceChannel`/`SinkChannel`-typed locals gives the `sun/nio/ch/*Impl` rows `invocations: 1` and the abstract CP-named rows **0**. The fallback walk follows **`superclass` links only and never visits an interface** — first run-evidence behind a conclusion `H8-1` had reached from the unrelated step-6 interface-default gate. Mode-independent.
- [H11-2](H11-2-two-hundred-thirty-seven-of-the-abstract-rows-are-fabrications-and-two-sites-hid-from-the-grep-20260820.md) — `OPEN` · **MEASURED, full census from a live registry dump: 36 abstract/interface classes, 334 `native-io` rows.** **237 rows across 14 classes CANNOT MOVE** — the VM mints the receiver, so relocating the registration strands it; **at most 15 rows are genuinely movable**, and two of those classes are already `sun.nio.ch`. This is the measured form of `H5-1` §3, and it prices the P1 row's remedy at roughly 4% of what that remedy assumes. **Disproves `H5-1` N7**: both classes it called *"no fabrication site found — candidate, unproven"* are fabricated **inside `native-io` itself**, at multi-line `alloc_obj`/`try_alloc_synthetic` calls four lines from the grep that missed them — the `a-50-line-window-is-not-an-absence-proof` failure, again. **The trap worth carrying:** for **55 of the 334** rows, deleting the `native-io` line does not remove a native — it hands the slot to **another crate's incompatible body**. `Pipe`'s six rows are the sharp case.
- [H11-3](H11-3-four-rows-retired-and-a-unit-test-that-blocks-the-next-two-20260820.md) — `FIXED-UNVERIFIED` · four live registrations retired (`java/io/DataInput.{readInt,readLong}`, `java/io/DataOutput.{writeInt,writeLong}`), each measured at **0 invocations across 15 corpus vectors** while `DataInputStream.readInt` took **540** in the same runs — the positive control that makes the zero mean something (`G33-1`: a zero on its own proves nothing). The named falsifier is stated: the `recv_is_bare_object` rescue path has no witness. **The blocked pair is the more useful half**: `Closeable`/`AutoCloseable` are equally dead, but `vm/src/vm/tests.rs::auto_closeable_close_p70` **calls the slot directly and its helper panics on an unregistered triple**, so retiring them is a two-file commit this lane could not make — a unit test pinning a fabrication in place is now a named blocker, not a surprise. The lane also records nearly publishing three of `H5-1`'s nominations as live when all three had been closed in its own 58-commit merge gap.
- [H12-1](H12-1-the-osr-door-binds-five-bridge-natives-the-method-entry-door-refuses-20260820.md) — `FIXED-UNVERIFIED` · **the tier-dependent hole `HANDOFF-20260820` §7 item 4 predicted, at the door neither previous audit looked at.** CratonVM has three compile doors. The handoff blamed six helpers in `vm/src/jit/helpers.rs`; `H7-1` correctly showed those six are not the problem; **both were auditing their own door and the hole is in the third one.** **REPRODUCED INDEPENDENTLY by lane H0** on the pristine binary with a fresh probe (a `static` method called ONCE containing a 300,000-iteration loop, so only OSR can tier it): under `--jdk-only` the MethodEntry door examined **7 sites and refused all 7** — that guard is real and fired — while the **OSR door bound**, and compiled code made **298,000 calls through a `bridge` native in strict mode**. `--real-jdk` reports an **identical** OSR column: the door does not consult the mode. Five `bridge` rows bind there unguarded. **Trap 1 paid again**: an in-tree comment says these helpers are *"gated twice"*, and gate 1 was **deleted 2026-08-06** — `jit/src/lib.rs` says the addresses are now registered *"unconditionally"* while its own note still lists the deletion as an open ask. **No wrong VALUE was witnessed** and the record leads with that: `native_hashmap_get_exact` walks the real `HashMap.table`, so `H4-1`'s "silently empty map" does not transfer to this door. It is an open door, and a retag is what walks through it.
- H12-2 (retired: `H12-2-the-three-door-direct-bind-matrix-and-how-to-re-measure-it-20260820`) — `OPEN` · the three-door direct-bind matrix and the procedure to re-measure it, written so the next lane does not have to rediscover which door it is looking at. Carries the concrete specification `H12-C` owed lane `H10`: **`RJitMapTierDiff` as originally specced would have passed**, and four changes make it discriminating — hot region in a once-invoked method (else it goes to the guarded door), receiver declared `HashMap` not `Map` (else the OSR ladder never matches), force the overlay, and assert `put`'s **return value** (since `H7-1` §2c is a wrong return with a correct map). Top nomination: `compile_gate.rs` already solved this exact problem for the backend entry with a type-level `CompileAdmission` token; applying it to `JitDirectCall` would make the fix **compiler-enforced instead of reviewer-remembered** — smaller than the audit that found the bug.
- [H14-1](H14-1-the-1402-shadows-are-149-registrars-and-none-were-adjudicated-20260820.md) — `OPEN` · **MEASURED — the first classification of the whole defect population, and it had never been done.** All **1402 / 1402 attributed, 0 unattributed**, every one `owns_slot: true`, across **149 registrars** joined by registrar FUNCTION rather than by file. Three things the census report alone could not show. **(a) `kind_stated` is `false` on all 1402** — not one native in the defect population was ever deliberately classified; every one inherits an ambient `set_category`, so **there is no adjudicated sub-population for a retag to separate out.** *Lane H0 checked this from the opposite direction and found the contrast is the sharper half:* on a registry dump, shadow-shaped rows are **90.2% `kind_stated: false` (2504/2775)** while legitimate `ACC_NATIVE` bridges are **99.5% `kind_stated: true` (205/206)** — a near-perfect inverse, which makes `kind_stated` a usable triage filter and a candidate birth-time gate. **(b) The image verdict splits the 1402 into three verbs**: 1244 *retire*, 156 registered on a class that does not declare the method, and **2 that MUST NOT be touched** — `java/nio/file/Path.toString()` and `.equals()`, verified independently as `public abstract` on an **interface** with no `Code`, so the registration is the only implementation and a row-count-driven plan would delete it and leave nothing. **(c) 162 triples are registered more than once** and only the `owns_slot` one is reachable, so a retirement aimed at the loser measures as "no effect". Also corrects `cluster-map.py`'s `fn` rule, which admits NESTED functions and had put a local helper at the top of the ranking.
- WORKER-3-NOTE-5 (retired: `WORKER-3-NOTE-5-the-census-exempts-398-shadows-20260822`) — `OPEN` · **CORRECTS THE DENOMINATOR EVERY ROW ABOVE IS A FRACTION OF.** `H14-1` calls its 1402 “the whole defect population”. It is not: the census records a `native-shadows-bytecode` row only when `kind != NativeKind::Intrinsic` (`vm_exec.rs:1405`), and `resolve_step1_native` returns every `Intrinsic` at step 2, BEFORE the step-3 arm that records — so the population is **`Bridge` + `SyntheticStub` by construction**. MEASURED on one `--jdk-only --dump-native-registry`: 10832 registrations, **629 `intrinsic`**, 595 of them `owns_slot: true`, and **398 where the real JDK 25 class declares the method WITH CODE** — i.e. shadows by §1.4's own definition, exempt from the count. That is **+29% on the denominator**, **305 of it `java/lang`**, and it appears in no figure in this directory. The exemption is defensible design (every JVM intrinsifies `Math.sqrt`); its SIZE being unpublished is not. **Re-read every percentage on this page as a fraction of ~1800, not ~1400.**
- [H14-2](H14-2-the-plan-is-aimed-at-fourteen-percent-of-the-defect-20260820.md) — `OPEN` · **MEASURED — the plan of record is aimed at 14% of the defect.** Distribution over 149 registrars: top 10 = **34.3%**, top 25 = **55.8%**, top 50 = **76.9%**; 95 clusters, 94 of them small. **`H0-4`'s six priced collection prefixes account for 200 rows — 14.3% — and 135 of the 149 registrars have ZERO rows under any of them.** **445 rows (31.7%) are claimed by no P0/P1/P2 row at all**: `java.lang` core 168, `java.io` streams 99, `StringBuilder`/`StringBuffer` 57, `java.lang.invoke` 56. So the effort's entire published queue addresses a seventh of the measured defect, and nearly a third of it belongs to nobody — a conclusion no hand-picked example could have reached, and the reason this lane existed. Positive control that the instrument can see work landing: the `java/util/logging` and `sun/nio/fs` retirement waves show as **exact zeros**, while `java/util/ArrayList` still carries 38 rows after 12 triples were retired.
- [H14-3](H14-3-five-retirements-are-free-and-properties-is-worse-than-hashmap-20260821.md) — `OPEN` · **MEASURED, thirteen arms, each verified uncontaminated — and it names work that is free.** **Five registrars cost ZERO vectors**: `register_throwable_subclass_natives` at a clean 104/104, plus `StringBuilder`/`StringBuffer`, `ArrayDeque`, `Optional`, `HexFormat` — **174 rows, 12.4% of the entire defect, for nothing.** Four more cost one vector each. At the other end **`java/util/Properties` is 65/104 — WORSE than `HashMap`'s 81** (`RJdkHello` fails), which inverts the cost ordering `H0-4` established and that three lanes have planned against; the 41-class monolith arm is 28/104. **Two cautions that bit this lane and are worth more than the numbers.** `RMapGcStress` failed in **12 of 13 arms** and would have been netted out as a shared defect — it is **`rc=124`, a TIMEOUT**: the control shows it needs **233 s unarmed against a 120 s budget** and PASSES armed at 600 s. It is the clock, and it is explicitly **not** the same `RMapGcStress` finding `H0-4` §4 netted out, which were real assertion failures. And **three arms were discarded and re-run**: a "killed" background sweep left its children alive, two sweeps drove `run.sh` concurrently, starved the HotSpot oracle, and moved `register_uri_natives` from **83/104 to 102/104** — the fixed-`.guard-tmp` collision hazard, demonstrated rather than theorised. The sweep now takes a lock and self-quarantines. Minor instrument findings: `CRATONVM_NATIVE_SHADOW_SINK_CAP=200000` is **silently discarded** (ceiling 65,536, no warning); the `jit_compile` sub-sink reports `truncated: null`, which `run.sh`'s saturation grep can never match; and `run.sh`'s `477 bytecode-won` is **453 distinct triples**, 24 double-counted.
- [H18-1](H18-1-the-opcode-and-the-message-read-different-classes-20260821.md) — `FIXED-UNVERIFIED` · **the opcode, the reflection and the exception message read THREE different classes off one object.** `Class.isInstance` consults a `getClass()` display alias; `op_instanceof`/`op_checkcast` never learned to; and the CCE message is a third reader again. Measured on the pristine binary, **seven of seven receivers diverge** — `Map.of()`/`Map.copyOf` on `AbstractMap`, `List.of()` on `AbstractCollection` and `RandomAccess`, `Set.of()` on `AbstractCollection`, `Collections.unmodifiableList` on `RandomAccess` — every one `instanceof=false` with `isInstance=true` **on the same object at the same instant**, where HotSpot says true for both. Fixed as **one table with three callers** rather than a fourth copy of the rule, which is the species this directory has recorded four times. Retires `H0-2` §5's objection to the fix: the copy already exists in `native-builtins` and `vm/Cargo.toml` already depends on it, so the patch is a **second caller, not a third copy**.
- [H18-2](H18-2-the-third-door-is-the-exception-message-20260821.md) — `FIXED-UNVERIFIED` (the leak) · `OPEN` (the `entrySet` display name) · **the third door is the exception message, and the fix written to close it was applied to one stamp of eleven.** `cce_display_class_name` exists by its own doc comment to stop a VM-generated `ClassCastException` exposing CratonVM's private stamp — it broke Spring's `LambdaSafe`, which identifies an erased-generic mismatch by comparing the exception prefix against `argument.getClass().getName()`. Its second statement is `if raw_name != "cratonvm/internal/UnmodifiableMap" { return raw_name }` — **one of the ELEVEN stamps `vm_init.rs:1746-1820` registers.** The other ten fell through and printed the private name. A guard written for a real, diagnosed bug, applied to 9% of its own population.
- [H18-3](H18-3-the-price-of-the-randomaccess-row-and-the-tier-that-did-not-get-the-fix-20260821.md) — `MEASURED` (§2) · `OPEN` (§3, patch written out but not applied — outside the lane's ownership) · **the `RandomAccess` row costs 886x, and the tier that did not get the fix is the tier that runs the loop.** Two prior records called this row "the one that costs something" and both left the cost **ARGUED**, because `Collections.binarySearch`/`reverse`/`shuffle`/`fill`/`copy`/`swap` branch on `list instanceof RandomAccess` and fall back to a `ListIterator` walk **while still returning correct answers** — so no value-diffing vector can see it. The instrument is an **A/B inside ONE VM and ONE run**: an `ArrayList` against `Collections.unmodifiableList` **of that same ArrayList**, which is what makes it survive a shared host where wall-clock numbers are otherwise worthless. `n=60000`: Compatible **37 ms vs 33,585 ms = 885.7x**, reproduced at **911.4x**; `--jdk-only` and HotSpot both **1.0x**. Scrupulous about what it does not claim — not a claim about `binarySearch`'s own complexity, and not a measurement of the other five methods.
- H19-1 (retired: `H19-1-the-combinators-computed-the-right-answer-in-the-wrong-class-20260821`) — `FIXED-UNVERIFIED`, **and the lane died before writing this record — reconstructed by H0 from its in-source measurements.** `Function.andThen`/`compose` fabricate `Function$AndThen`/`Function$Compose` in Compatible mode where HotSpot and `--jdk-only` both return a real `Function$$Lambda`. **The computed values were RIGHT in all three arms** (`andThen` 30, `compose` 21), which is exactly why this survived: only a **class-name screen** or a **null-contract check** can see it, and the corpus has neither. `f.andThen(null)` fails to NPE. Fourth instance of the standing shape *a native that returns the right value through the wrong object*. **`identity()` is deliberately left unguarded** because `register_function_identity_natives` registers it again later and only the second registration is reachable — `H14-1`'s duplicate-registration trap (162 triples, only the `owns_slot` one live) turning up mid-fix. Also builds a MEASURED type-conversion matrix for `asInterfaceInstance`, including that the whole `void` COLUMN of the return matrix accepts, spelled out rather than folded into `primitive_widens_to` where a `V` entry would wrongly claim `void` is a widening of `int` **in both directions**.
- [H19-2](H19-2-the-empty-container-was-the-only-one-still-fabricating-20260821.md) — `FIXED-UNVERIFIED`, reconstructed by H0 from the dead lane's own doc comments. **The empty container was the only one still fabricating, and it diverged three ways at one site.** `new Hashtable<>().keys()`: HotSpot gives `Collections$EmptyEnumeration`, not an `Iterator`, throwing `NoSuchElementException` past the end; Compatible mode gives **`Enumeration$Impl`**, which **IS** an `Iterator` and **returns** past the end. **The populated table was already correct in both arms**, because real `Hashtable.getEnumeration(int)` short-circuits to `Collections.emptyEnumeration()` at `count == 0` — so the defect is **EMPTY-CONDITIONED and every probe filled the table**. Third instance in two days of *a narrow probe reports its own reach*. The premise was already written down in `hashtable_has_entry`'s doc block; the VM had no way to honour it. Fixed by resolving the JDK's own carrier, returning `None` — never a fabrication — when absent, and propagating with `?` rather than swallowing, because the sibling it mirrors propagates and a swallow would **discard a live `Throwable`**.
- [H20-1](H20-1-the-direct-call-plan-is-a-second-thing-every-door-builds-20260821.md) — `FIXED-UNVERIFIED`, reconstructed by H0 — the lane wrote ~950 lines and died one step before running its probe. **Asked to transplant `compile_gate.rs`'s `CompileAdmission` token onto `JitDirectCall`; it found the analogy does not hold, and the reason is worth more than the fix.** `CompileAdmission` works at the backend entry because "do not compile" is a fallback every caller already has. **A direct-bind refusal has no downstream point at all**: `reserve_stack_floor` defines a raw self-call as *an `invokestatic` pc with neither an invoke-info entry nor a direct-call plan*, and every ladder pushes its row then `continue`s past the `invoke_info` construction — so a row dropped after the door leaves the pc with no metadata and **the backend compiles `Thread.currentThread()` as a call to the enclosing method.** *"Filtering downstream would trade an open door for a wild jump."* That is the obvious fix, it is what a reviewer would propose, and it would have converted a latent policy hole into a live miscompile. Built instead: a `DirectCallPolicy` witness with **no `Default`** and **three** states so "never asked" stays representable, a rule stated once, a **counter** that makes the bypass a number instead of a silence, and — the part that answers the brief — `CompileDoor::builds_direct_calls` as an **exhaustive `match`**, so a fourth door cannot be added without answering the question the third one silently skipped.
- H21-1 (retired: `H21-1-the-vm-was-handing-out-an-instance-of-an-abstract-class-20260821`) — `FIXED-UNVERIFIED`, reconstructed by H0 — the lane died narrating a change it had already written. **`Pipe.open()` returned an instance of an ABSTRACT class, in both modes.** `Pipe.open().getClass()` answered `java.nio.channels.Pipe` with `Modifier.isAbstract == true`, against `sun.nio.ch.PipeImpl` on the oracle — **a receiver the `new` opcode cannot legally produce** (JVMS §6.5 makes it an `InstantiationError`). One line: `ensure_class_initialized("java/nio/channels/Pipe")` resolves to the real, abstract class and `alloc_object` mints it. **Only the wrapper was wrong** — both channels were already `SourceChannelImpl`/`SinkChannelImpl`, so the object graph was right everywhere except its root. **This is the P1 row's real defect**: `H5-1`/`H11-2` disproved the published remedy by measuring that 237 of 334 `native-io` rows cannot move *because the VM mints abstract receivers*; the registration was never the problem. Slot compatibility measured with `javap`, not assumed — `PipeImpl` declares `source`/`sink` at slots 0 and 1, the same two the VM already writes. Both halves of `H11-1`'s dispatch answer are load-bearing here in opposite directions at one call site: `source`/`sink` are registered on `PipeImpl` too because dispatch keys on the **receiver**, while static `Pipe.open()` stays on the abstract class because there the **constant-pool** class is the key.
- [H13-1](H13-1-the-map-that-answers-get-and-reports-empty-20260820.md) — `OPEN` · **MEASURED — a THIRD mechanism, and the nastiest of the three.** Arming CHM does not empty the map and does not disconnect it: it makes one instance **internally inconsistent**. **REPRODUCED by lane H0** with an independent probe (`regression-suite/probes/ChmConsistencyProbe.java`), unarmed control clean: four back-to-back String `put`s give **`size()=1` with an EMPTY `keySet()`** — the counter and the table contradicting each other *through a single door* — where HotSpot gives 4; **interposing `Math.abs(1)` between the puts makes all four persist**; `Integer` keys are correct throughout; and the same map read through a `Map`-typed local answers `containsKey=false, keySet=[], entrySet().size()=0` while the `ConcurrentHashMap`-typed local answers `true, [only], 1`. **Not the JIT** (`--nojit` identical), so the deferral lives below tier-up. Unlike mechanisms A and B, which fail loudly with an NPE or an `ArrayStoreException`, **this one returns wrong answers quietly and its trigger is the instruction sequence around the put, not the data.** Traced end to end: `CryptoPermissions.isEmpty()` true for a map whose `size()` is 1 → `JceSecurity.<clinit>` throws → **the whole JCE is dead for the process** (RCrypto); `KnownOIDs.name2enum` holds **1 entry against HotSpot's 590**, with the JDK's own duplicate guard unable to fire because a dropped `put` returns the success value (RJdkX509Intercept).
- [H13-2](H13-2-all-four-assigned-defects-were-closed-and-the-probe-found-two-more-20260820.md) — `FIXED-UNVERIFIED` · **all four assigned defects were already closed** — verified against the tree rather than read, one of them by a byte-exact 33/33-line HotSpot diff of the repo's own probe. The value is what the verification turned up instead. **`engine_delegate_shape` names one engine and every other falls off `_ => None`**: `MessageDigest$Delegate`'s only constructor is `(Spi, String, Provider)`, a different **arity**, so a provider written to the documented JCA contract (a bare `MessageDigestSpi`) got `NoSuchAlgorithmException` where HotSpot returns a delegate — **only BouncyCastle's shape ever got through**. That is the `a-defaulting-helper-with-no-reporting-caller` species. And `MessageDigest.getInstance` checked provider **existence** and never **ownership**, the last engine of six that did not — kept as its own commit because it is the only narrowing in the lane and must be revertible alone. Two more measured and deliberately NOT fixed (files the lane does not own): a `KeyPairGeneratorSpi` constructed and thrown away, and `getProvider()` returning a fabricated Provider across every engine — one defect with at least three faces, **now deferred twice for the same ownership reason**.
- [H15-1](H15-1-the-five-gate-failures-are-compatible-mode-defects-20260820.md) — `OPEN` · **MEASURED, five of five — and it reframes the acceptance bar every lane in this wave worked under.** **All five standing `SUITE=all` failures PASS under `--jdk-only` and fail only in Compatible mode**, with HotSpot agreeing with the strict arm every time. **CONFIRMED by lane H0 from `run.sh` itself**: the strict arm schedules `CORE_CLASSES + JDKONLY_CLASSES` and `SUITE=all` schedules `CORE_CLASSES + JDKONLY_CLASSES` — **the identical vector set**, differing only in the flag. So `104/104` strict against `99/104` compatible has always meant *these five are compatible-mode defects that strict mode already fixes*, and it has been sitting in plain sight in every baseline block for weeks, quoted repeatedly and read by nobody. Consequences: **no strict-mode change can move this set**, and a change that turns one green is the fix rather than a violation. N1 proposes splitting the bar into "`--jdk-only` stays green" plus "the `SUITE=all` set must not grow or change membership except by removal".
- [H15-2](H15-2-the-opcode-does-not-read-the-alias-the-reflection-does-20260820.md) — `OPEN` · **`op_instanceof` and `op_checkcast` never consult the `getClass()` display alias that `Class.isInstance` has consulted since the Spring `GenericConversionService` fix.** All twelve of `H0-2` §4's cells re-measured divergent, every one with **`instanceof=false` and `isInstance=true` on the same receiver** whose `getClass()` name is identical to HotSpot's — the `a-reflective-native-and-its-bytecode-opcode-are-twins-that-drift` species, caught in the act. **§5.1 retires `H0-2` §5's reason for declining the fix**: the "second copy of one rule" objection is spent, because the copy already exists in `native-builtins` and `vm/Cargo.toml` already depends on that crate — so the patch is a **second caller, not a third copy**. It avoids the one real hazard (`getclass_display_class_id` runs `invoke_virtual("size")` and can move the heap mid-opcode) with a family-level exit that needs no size.
- [H15-3](H15-3-three-stand-ins-compatible-mode-does-not-need-20260820.md) — `OPEN` · three `SyntheticStub` stand-ins that strict mode drops and Compatible keeps, **where the real bytecode demonstrably works** — so the stand-in is the entire defect. `asInterfaceInstance` returns its own `MethodHandle` argument; `Function.andThen` mints a `Function$AndThen`; and an **empty** `Hashtable.keys()` mints an `Enumeration$Impl` — the non-empty case is already correct, so the defect is **empty-conditioned**, and that same site also fails to throw `NoSuchElementException` past the end, a second divergence no vector reaches. Also: **`D1-R11`'s fix landed and its diagnosis expired** — the boot-layer door is closed, but a `-cp`-only module's `provides` still reaches `ServiceLoader` because the guard in `service_providers_from_modules` **asks about the loader where the rule is about the module** (`descriptors=0 providers=2`). `RJdkFunctionCombinators` is diagnosed to its FIRST failure only; eleven stand-in names remain across three registrars and the lane says plainly it cannot tell whether that is one fix or five without a build.

### 2026-08-28 — lane L1 (`Unsafe`)

- [l1-unsafe-516-rows-24-defects](l1-unsafe-516-rows-24-defects-and-the-sub-word-atomics-that-never-returned-20260828.md) — `FIXED` (24 defects, plus 4 more found on the residual sweep) · `RESIDUALS CLOSED 2026-08-30/09-01` (§19–§24) · **MEASURED, 516 rows against HotSpot 25.0.4+7 in BOTH modes, 0 mode drift.** The `jdk/internal/misc/Unsafe` (120) + `sun/misc/Unsafe` (102) families of the Phase-2 retirement surface. **Headline: the JDK implements the whole byte/short/char/boolean atomic family in BYTECODE, by masking the 32-bit word at `offset & ~3` — which is meaningless when `objectFieldOffset` returns a SLOT INDEX. So `compareAndSetByte` answered `false` with the right witness, `compareAndExchangeByte` answered 0, and `getAndSetByte` / `getAndBitwiseOrByte` NEVER RETURNED.** Closed by four registrations (`compareAndSet{Byte,Short}`, `compareAndExchange{Byte,Short}`) whose bodies are the existing `int` ones unchanged — what was missing was a registration, not an implementation. The control that identifies the mechanism is `getAndAddByte`: same family, already registered as a native, already correct. **These four are unretirable by construction, and a retirement pass reasoning only from "the real method has Code" will propose them.** **Second: a Java caller could ABORT the VM** — `allocateMemory(Long.MAX_VALUE)` reached `vec![0u8; size]`, which is infallible, so Rust's allocation-error hook killed the process where HotSpot throws `IllegalArgumentException`; the IAE/OOME boundary is now measured (`probes/AllocBoundary.java`) rather than guessed. Also: the two off-heap doors named DIFFERENT STORAGE for float and double only (the null-base arm read a private side map, so it round-tripped perfectly within one door and shared nothing with the arena); `copySwapMemory` off-heap was a silent no-op; and a `copyMemory` length guard sat BELOW the dispatch it was meant to cover, so the heap arm never reached it. **The generalisation worth carrying: the retirement surface IS the JDK's argument-validation layer** — 19 of the 24 defects are a check that lives in a bytecode wrapper the shadow replaced, and the `0`-suffixed twins already point at the same Rust functions. **And the two spellings do not share one contract**: `sun.misc.Unsafe.objectFieldOffset` refuses a record component and a hidden class, `jdk.internal.misc.Unsafe.objectFieldOffset` answers an offset — so the shared native has to ask which door it came through, and a stage-1 fix that applied the refusal to both replaced one wrong answer with another. **The residuals are now CLOSED, and closing them found the bigger defect they stood in front of: every public constant on the LEGACY `sun.misc.Unsafe` spelling was ZERO** — all 18 `ARRAY_*` plus `ADDRESS_SIZE` — because `<clinit>` computes them through natives not registered that early in boot and an unregistered native returns its return type's zero instead of throwing, so any consumer following the documented `ARRAY_<T>_BASE_OFFSET + index` protocol through that spelling read bytes 16 short, SILENTLY. `RUnsafeArrayBase` — the core vector named for this exact surface — was green throughout, because it called the METHOD and never read the CONSTANT; it now guards both and is proven to fail when they disagree. The memory-access warning latch was the same defect (`staticFieldBase`/`staticFieldOffset` returned `null`/`0`), and repairing it took R5's 513+ null-base rescues per H2 vector to **0**. **The null-base CAS that returned `true` having written nowhere now throws**, and `objectFieldOffset(Class,String)` refuses an absent field as HotSpot does — both after counting the population §4.5 asked for: 0 hits across 120 regression vectors, 136 Netty buffer/util classes, both DoD workloads and both H2 vectors, each with a firing positive control. Two methodology results worth more than the counts: **an oracle configured unlike the VM under test invents permanent residual rows** (three of them here were `-XX:-UseCompressedOops` on HotSpot and nothing else), and **`capture_stack_trace` and `frame_class_ids` order their frames OPPOSITELY**, so reading the wrong end gives the stack floor — which looks exactly like an answer.
- [L4-the-io-and-nio-worklist](L4-the-io-and-nio-worklist-49-defects-and-a-bounds-check-that-killed-the-vm-20260828.md) — `FIXED` (67 defects, 8 shadows retired) · `OPEN` (one residual, §4.3; three unclaimed, §P3.10) · **MEASURED, 2304 rows against HotSpot 25.0.4+7 in BOTH modes, 2303 identical.** The `java/io` + `java/nio` families of the Phase-2 retirement surface: 199 native-won triples, mined from a `--jdk-only-report` over `apps/probes/L4Reach.java` rather than guessed. **Headline: `PrintStream.write(byte[], 0, -1)` PANICKED THE VM** (`capacity overflow` — `-1 as usize` in a bounds check that was not there at all), and `sun.nio.ch.FileChannelImpl.truncate(-1)` clamped to zero and DELETED the file. Also: `Files.copy`/`move` raising `IllegalStateException`, which is not an `IOException` at all, so `catch (IOException)` could not see it; `Path.startsWith` implemented as a STRING prefix, so `/appsecret` starts with `/app`; a `SimpleFileVisitor` whose `visitFileFailed` answered `CONTINUE` and swallowed every walk error; and a backslash treated as a path separator on Unix — 47 rows in one EXISTING probe, from a predicate whose own comment claimed it was platform-independent. **The generalisation: a native that shadows bytecode inherits the bytecode's ARGUMENT VALIDATION, and that is where most of these live.** Part two answered the completeness question — 395 static rows, of which 142 had never been reached by any probe in the tree — and part three asked those. **The covariant bridge and its target disagreed**: `javac` emits `reset()Ljava/nio/Buffer;` only when the reference is typed `Buffer`, so ~30 bridge descriptors had never been dispatched to, and `Buffer b = bb; b.reset()` raised `IllegalStateException` where `bb.reset()` raised `InvalidMarkException` — one method, two answers, decided by a local variable's declared type. **And the setter was not the liar**: fixing `File.setReadable` did not close its row, because the chmod had been correct all along and `canRead` was answering "does it exist?" — the three `can*` methods asked about the FILE where the JDK asks `access(2)` about THIS PROCESS. Reading the state back through a second method is what separated them. Two of the lane's own mistakes are recorded because they are the reusable part: a null check put into `write([BII)`, an overload neither failing row calls, and a US-ASCII decoder given REPLACE where `Files.readString` REPORTs — refuted by the oracle refusing a row this VM answered. **Four probes this record itself cited had been deleted by a repo reorg and were reporting `DIFF strict=0` on runs that produced ZERO LINES**; they are restored and tracked, and the row count printed beside every diff is the only reason it was noticed.

---

## L2, 2026-08-30 — Phase 2 adjudicated

- [Phase 2 adjudicated](phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md) — `OPEN` (the retirement is; the adjudication is not) · **The `--jdk-only` shadow surface is 1477 triples over 270 classes. A per-class `CRATONVM_ENFORCE_NATIVE_SHADOW` sweep calls 34 load-bearing and 236 retire-safe — and that green column cannot be used as a retirement list.** Arming all 236 at once fails **54 of 118 corpus vectors** (14 123 530 dispatches reached the dial, 14 055 769 yielded, **0 leaks** — the retirement was complete and it still failed) and breaks **35 of 78 probe families, killing 20**, several of which were byte-identical to HotSpot unarmed. Three reasons the sweep over-reports, each cheaper to check than the sweep: **146 of the 236 greens were never asked anything** (the smoke set dispatches a shadowed native on only 120 of 270 classes — check `enforcement_dial.reached > 0`, not a passing vector); the scope is `starts_with`, so **a row measures a prefix, not the class it names**, and a safe prefix cannot exclude a load-bearing class beneath it — **a retirement is per-TRIPLE, the dial is per-PREFIX**; and it would have **re-retired six of the eleven triples `retired_shadow.rs` deliberately holds back**. **The sharp case:** the corpus passes `ConcurrentHashMap` 14/14 and its OWN content probe is 0-diff over 39 357 yields — but `MapViewsShadowSweep` dies at row 261 of 302, because JDK 25's `Properties` delegates to an internal `ConcurrentHashMap`, so **`Properties.keySet()` comes back `[]` on a three-entry table**. A retirement's blast radius is its class's USERS, and the family's own probe being clean is the trap rather than the reassurance. Guarded in code: `the_held_collection_families_are_not_retired` now carries the measurement and holds the CHM view/iterator classes. **Five families got BETTER armed** (`AbstractReceiverSweep` 12→2 vs HotSpot, `L6MsgProbe` 8→2, `L4Diag` 4→0, `DequeListShadowSweep` and `PropsOrderSweep` 2→0) — those are the only positive retirement leads the adjudication produced.

---

## L5 residual round, 2026-08-30 — the `java.lang.invoke` argument surface

- [The cast that `asType` performs, and this VM did not](the-cast-that-asType-performs-and-this-vm-did-not-20260830.md) — `FIXED` · **`MethodHandle.invoke` is `asType(callSiteType)` then an exact invocation, and `asType` CASTS every reference argument to the handle's declared parameter type. This VM passed them through untouched**, so `cat.invoke("ab", (Object) Integer.valueOf(3))` answered `NoSuchMethodError: 'boolean java.lang.Integer.isEmpty()'` where HotSpot answers `ClassCastException` — the `Integer` reached the callee and the callee's own body dispatched on it, so the error names a method the caller never wrote. `NoSuchMethodError` extends `Error`, so the `catch (ClassCastException)` a framework wraps a reflective dispatch in does not see it. The RECEIVER was uncast too, and `bindTo` accepted the mismatch outright and returned a handle that looks well-typed. **Five independent doors**: `invoke`, `bindTo`, both `invokeWithArguments` overloads, and `Method.invoke`'s interface formals. Closes L5's last recorded residual and the `bindTo` half W7-19 §5.1 declined — that deferral's reason still governs the SHAPE (a naive `is_subclass` throws FALSE `ClassCastException`s on the Groovy-indy / SpEL / log4j paths), so `reference_arg_admitted` refuses only on a POSITIVE reading and **14 of `InvokeCastSweep`'s 29 rows exist only to assert the check does NOT fire**. §5.1 also asked for "a lane that can run those workloads": `probes/CodegenFrameworkSmoke.java` boots Groovy, ByteBuddy, Mockito, ASM, Javassist and Objenesis **as themselves**, 16 rows 0-diff both modes. **The interface case was not the limit it was first recorded as** — `ClassManager::is_assignable_to_name` was already what the `aastore` path used and had no `NativeContext` door; it has one now. **And exposing it immediately refused working code**: `Linker.downcallHandle(MemorySegment, …)` with `argument type mismatch`, because the `strlen` address is a `cratonvm.internal.foreign.MemorySegmentImpl` that declares no interfaces and is known only to `synthetic_implements_declared`. Neither new probe caught that; `P1RemainingSweep`, from a lane closed the day before, did.
- [The fix that changed nothing: a shadowed registrar on the `Lookup.define*` doors](the-fix-that-changed-nothing-a-shadowed-registrar-on-the-lookup-define-doors-20260830.md) — `FIXED` · **Eleven refusal-type defects across the three `MethodHandles$Lookup.define*Class*` doors**: bad magic, empty and truncated bytes must be `ClassFormatError`, null bytes `NullPointerException`, and a null `ClassOption[]` an NPE rather than silently accepted. The type is the contract — `ClassFormatError` is an `Error`, so a generator guarding its emit with `catch (ClassFormatError)` never sees an `IllegalArgumentException` and the malformed class escapes to fail elsewhere. **The first fix changed NOTHING**: it went into `classloader.rs`, and `lookup_define.rs` re-registers all three triples afterwards from BOTH registrars, so those callbacks are never dispatched — a 34-minute build to learn what one `--dump-native-registry` column says. **One dump: 12 792 registrations, 1 060 shadowed, 568 CROSS-FILE** (109 `lang_misc.rs`→`lib.rs`, 58 `native-io/lib.rs`→`phases_late/nio_file.rs`, …), so the prior odds that a native picked by name is the loser are ~4%. Zero rows have a `bridge` losing to a `synthetic-stub`, so that historical species is closed by `register`'s downgrade rule. **Measuring every door before fixing any changed the fix twice**: `ClassLoader.defineClass` was already correct at both its doors, and extending the probe 6 rows → 14 found the truncated-body case (which fails in the BACKEND, not in the magic check) and the silently-accepted null varargs. The `define.lookupWrongPackage` control row holds the JDK's split contract — format problems are `ClassFormatError`, package problems stay `IllegalArgumentException` — and a blanket replacement would have broken it.
- [`VarHandle` checks neither its receiver nor its value](varhandle-checks-neither-its-receiver-nor-its-value-20260830.md) — `OPEN` · **`vs.set(box, Integer.valueOf(3))` leaves a `String`-declared field holding an `Integer` with nothing failing at the store**, so the next ordinary read of that field is where it surfaces — at a site that did nothing wrong. **`vs.get("not-a-box")` reads through a receiver of the wrong class and RETURNS a value**: the field index was resolved from the VarHandle's own class and applied to a `String`. Five rows, both modes, so an ordinary defect and not a mode defect. The same shape repeats across six natives, each with its own `VH_KIND_INSTANCE` arm. **Not a one-line fix**: these are the CAS-dominated paths the file has been tuned for twice (a thread-local plan memo moved a scaling probe 0.07x → 0.68x at 24 threads), so a per-operation `class_id_by_name` + `is_subclass` would undo that. `VarHandleMeta` already carries `class_id` and `field_desc`, so the receiver check is an integer compare and only a mismatch pays the full predicate. The record carries the design. **What the same 65-row sweep says is already RIGHT is what makes this specific**: `Method.invoke`, `Field.set`/`get`, `Constructor.newInstance` and `Array.set` all refuse correctly across wrong references, wrong receivers, arity, and primitive widening/narrowing/null — `Array.set` because it was made to ask the same question as the `aastore` bytecode, and `VarHandle` because it was never asked to ask anything.
