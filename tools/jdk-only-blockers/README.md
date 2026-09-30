# jdk-only-blockers

Generates — and ratchets — the two per-JDK blocker artifacts required by
[`jdk-only-audit.md`](../../audits/jdk-only-audit.md) §6:

```text
target/jdk-only-audit/jdk-<feature>-missing-natives.json
target/jdk-only-audit/jdk-<feature>-synthetic-dependencies.json
```

These are **machine-generated per JDK image**, not written by hand. No static
source review can produce an exhaustive missing-method list across JDK
versions: native surfaces and class-library internals vary by image, so a list
generated against JDK 25 says nothing about JDK 21.

| File | Purpose |
|---|---|
| `blockers.py` | The generator and the ratchet. Standard library only. |
| `selftest.py` | Offline self-test. No VM build, no JDK, no network. |
| `status-ledger.json` | Operator-maintained closure records. |
| `baselines/` | Committed baselines, one pair per JDK feature version. |

---

## What it consumes

Runtime dumps written by `cratonvm`. All five are optional; each one that is
absent degrades the result and is named in the output (see *Partial results*).

> **All dumps must come from the same VM run under the same compatibility
> policy.** A registry census, a class-origin census, an unresolved-native list
> and a violation report describe *one booted VM*. Feeding this tool a
> permissive registry together with a strict class census pairs two different
> worlds; the blocker list that comes out is wrong in a way that looks fine.
> The tool cannot detect the mix — the dumps carry no run identity — so the
> caller is responsible for the pairing. Use `scripts/jdk-only-census.sh`,
> which runs each policy exactly once and names every dump after its policy.

| `blockers.py` flag | `cratonvm` flag | Supplies |
|---|---|---|
| `--native-registry` | `--dump-native-registry` | every registration still classified `synthetic-stub`, with `registered_by`, `overwrote`, `invocations` |
| `--missing-natives-grouped` | `--dump-missing-natives-grouped` | unresolved natives with the VM's own JDK-module attribution |
| `--missing-natives` | `--dump-missing-natives` | the same rows without the module column; redundant when the grouped dump is present |
| `--class-origins` | `--dump-class-origins` | the class-origin census — the compatibility stubs *and* the legitimately generated classes |
| `--jdk-only-report` | `--jdk-only-report` | dispatch-time refusals, refused compatibility classes, and the JDK feature version |

The tool tolerates both census shapes currently in the tree: `vm-cli` and `vm`
each write a native census and a class-origin census, and they differ in their
top-level keys (`mode` / `total` vs. a per-tag `counts` object) and in whether
`real_declaring_method` is `null` or an object. Only the `natives[]`,
`classes[]`, `modules{}`, `missing_natives[]` and `violations[]` payloads are
read, so either emitter works.

## What it emits

Both files are `schema_version: 1`, UTF-8, LF newlines, ASCII-escaped, two-space
indented, with a trailing newline.

### `jdk-<feature>-missing-natives.json`

```jsonc
{
  "schema_version": 1,
  "artifact": "jdk-only-missing-natives",
  "jdk_feature": "25",
  "partial": false,
  "inputs": { "class-origins": "present", "native-registry": "present", … },
  "module_table_source": "vm/src/vm/vm_init.rs",
  "status_vocabulary": ["open","bridge","intrinsic","real-bytecode","generated-class","out-of-scope"],
  "classification_reference": "docs/jdk-only-native-review.md",
  "contract_reference": "docs/feature-designs/jdk-only-mode.md",
  "notes": [],
  "registry_totals": { "bridge": 0, "intrinsic": 0, "synthetic-stub": 0, "total": 0 },
  "counts": {
    "total": 0, "open": 0, "closed": 0,
    "requires_classification": 0,
    "never_invoked": 0, "invocations_unknown": 0,
    "by_category": {}, "by_status": {}, "by_module": {}
  },
  "entries": [
    {
      "class": "javax/management/MBeanServer",
      "name": "queryNames",
      "descriptor": "()Ljava/util/Set;",
      "category": "synthetic-stub-registration",
      "module": "java.management",
      "kind": "synthetic-stub",
      "invocations": 7,
      "invocations_known": true,
      "registered_by": "native-builtins/src/jmx.rs:88",
      "overwrote": null,
      "real_declaring_method": null,
      "sample_call_site": null,
      "sources": ["native-registry"],
      "requires_classification": true,
      "classification_reference": "docs/jdk-only-native-review.md",
      "status": "open",
      "status_note": null
    }
  ]
}
```

Two categories:

- **`unresolved-native`** — a native the run needed and could not resolve.
  `invocations_known` is `false`: the unresolved log dedupes by triple and
  records a first call site, not a count. Emitting `0` there would be a
  fabrication, so the count is explicitly marked unknown.
- **`synthetic-stub-registration`** — a registration still classified
  `synthetic-stub`. These carry `"requires_classification": true`.

> **`synthetic-stub` is not a verdict.** The native kind is *ambient*: it comes
> from `NativeMethodRegistry::set_category` / `with_category`, not from a
> `register()` argument, and it defaults to `SyntheticStub`. A `register()` call
> outside a `with_category(...)` block is a stub *by omission*. Some
> `synthetic-stub` rows are therefore genuinely mis-tagged permanent bridges.
> This file never asserts they are defects; it asserts they need a
> classification decision, per
> [`docs/jdk-only-native-review.md`](../../docs/jdk-only-native-review.md).

`registry_totals` is copied verbatim from the census's own `counts` and nothing
is re-derived from it. The stub ratchet in
`native-builtins/tests/stub_ratchet.rs` asserts the **157** `SyntheticStub`
baseline exactly and asserts only `total >= 8000`; the total is not a pinned
measurement, so do not quote one.

### `jdk-<feature>-synthetic-dependencies.json`

```jsonc
{
  "schema_version": 1,
  "artifact": "jdk-only-synthetic-dependencies",
  "jdk_feature": "25",
  "partial": false,
  "inputs": { … },
  "status_vocabulary": [ … ],
  "allowed_generated_origins": ["vm-array","hidden-class","generated-lambda","generated-proxy","reflection-accessor"],
  "blocker_origin": "compatibility-stub",
  "contract_reference": "docs/feature-designs/jdk-only-mode.md",
  "notes": [],
  "counts": {
    "blockers_total": 0, "blockers_open": 0, "blockers_closed": 0,
    "allowed_generated_total": 0,
    "by_category": {}, "by_status": {}, "by_origin": {}
  },
  "blockers": [
    {
      "name": "java/util/function/Function$Identity",
      "category": "compatibility-stub-class",
      "origin": "compatibility-stub",
      "reason": "Function.identity() stand-in",
      "real_bytes_found": false,
      "requested_by": "java/util/function/Function.identity()Ljava/util/function/Function;",
      "loader_ids": [0],
      "occurrences": 1,
      "sources": ["class-origins"],
      "status": "open",
      "status_note": null
    }
  ],
  "allowed": [
    { "origin": "generated-lambda", "disposition": "allowed", "count": 1, "classes": ["Main$$Lambda$1"] }
  ]
}
```

Two blocker categories:

- **`compatibility-stub-class`** — a class in the origin census with origin
  `compatibility-stub`.
- **`compatibility-class-refused`** — a `compatibility-class-requested`
  violation. Strict mode refused to fabricate it, so it never reached the class
  census; the report is the only evidence it left behind.

**`allowed` is not a blocker list.** `vm-array`, `hidden-class`,
`generated-lambda`, `generated-proxy` and `reflection-accessor` classes are
permitted under `--jdk-only` (contract §1 item 6). They are reported separately
so a reviewer can tell legitimate runtime generation apart from fabrication.
Conflating the two is what makes an origin census useless. Classes loaded from
real bytes (`boot-image`, `application-class-path`, `user-defined`) and
`vm-internal` bookkeeping appear only in `counts.by_origin`. An origin tag the
tool does not recognise is **named in `notes` and the result is marked partial**
— it is never silently treated as allowed.

### Closure vocabulary

`status` is `open` unless the status ledger closes it. An entry is closed only
when it is:

| `status` | Meaning |
|---|---|
| `bridge` | implemented as a reviewed `NativeKind::Bridge` |
| `intrinsic` | implemented as a reviewed `NativeKind::Intrinsic` |
| `real-bytecode` | executed from real class bytes; the substitution is gone |
| `generated-class` | implemented as legitimate generated class bytes |
| `out-of-scope` | explicitly declared out of scope, with a specification-consistent error |

Anything else is `open`. There is no `wontfix` and no `deferred`. An
off-vocabulary status in the ledger is a hard error (exit 2), so a typo can
never silently close a blocker.

---

## Running it

### The supported driver

```bash
export JAVA_HOME=/path/to/jdk25
sh scripts/jdk-only-census.sh                 # generate
BLOCKERS=check  sh scripts/jdk-only-census.sh # generate + ratchet
BLOCKERS=update sh scripts/jdk-only-census.sh # generate + refresh the baseline
BLOCKERS=off    sh scripts/jdk-only-census.sh # dumps only
```

The census script boots the VM once per policy — each run emitting all four
dumps — and then calls this tool once per coherent set:

| dump set | policy | artifacts land in |
|---|---|---|
| `registry-real.json`, `missing-real-by-module.json`, `classes-real.json`, `report-real.json` | `--real-jdk` | `target/jdk-only-audit/` |
| `registry-strict.json`, `missing-strict-by-module.json`, `classes-strict.json`, `report-strict.json` | `--jdk-only` | `target/jdk-only-audit/strict/` |

The `real` pair is the canonical §6 artifact and the only one ratcheted:
`baselines/` holds one pair per JDK feature version, so ratcheting a second
policy against it would compare two different worlds. The script always passes
`--jdk-feature` explicitly, read from `$JAVA_HOME/release`, rather than letting
it be inferred from a report a failing strict run may never have written.

### By hand

```bash
export JAVA_HOME=/path/to/jdk25
mkdir -p target/jdk-only-audit

cargo run -p cratonvm-cli --bin cratonvm -- \
  --jdk-only \
  --java-home "$JAVA_HOME" \
  --jdk-only-report              target/jdk-only-audit/report-strict.json \
  --dump-native-registry         target/jdk-only-audit/registry-strict.json \
  --dump-class-origins           target/jdk-only-audit/classes-strict.json \
  --dump-missing-natives-grouped target/jdk-only-audit/missing-strict.json \
  -cp vm-cli/tests/resources HelloWorld

python tools/jdk-only-blockers/blockers.py \
  --native-registry         target/jdk-only-audit/registry-strict.json \
  --missing-natives-grouped target/jdk-only-audit/missing-strict.json \
  --class-origins           target/jdk-only-audit/classes-strict.json \
  --jdk-only-report         target/jdk-only-audit/report-strict.json \
  --out-dir                 target/jdk-only-audit
```

The JDK feature version is taken from the report's `jdk_feature`. Pass
`--jdk-feature N` when there is no report, or to override it. If it cannot be
determined the tool **fails** (exit 2) rather than guessing: the version is part
of the file name and an artifact keyed to the wrong image is worse than none.

The same generation works under `--real-jdk` and is the more useful baseline
today — wave 1 is measurement, not deletion, so a `Compatible` run's census is
exactly the measure of how far `--jdk-only` still has to go.

### Regenerating a baseline

```bash
python tools/jdk-only-blockers/blockers.py \
  --native-registry         target/jdk-only-audit/registry-strict.json \
  --missing-natives-grouped target/jdk-only-audit/missing-strict.json \
  --class-origins           target/jdk-only-audit/classes-strict.json \
  --jdk-only-report         target/jdk-only-audit/report-strict.json \
  --out-dir                 target/jdk-only-audit \
  --update-baseline

git add tools/jdk-only-blockers/baselines/
```

Baselines land in `tools/jdk-only-blockers/baselines/` (override with
`--baseline-dir`) under the same names. They are byte-stable: fixed sort order,
no timestamps, no absolute paths, LF newlines. Two runs over the same dumps
produce identical bytes, so a diff is always a real change.

A baseline is refused from a partial result. Freezing a partial run bakes in a
false-clean ratchet, which is the exact failure this tool exists to prevent.
`--allow-partial` overrides that, deliberately.

### Ratcheting

```bash
python tools/jdk-only-blockers/blockers.py … --check
```

`--check` regenerates, then compares the **open** key set against the committed
baseline and exits non-zero if it grew. Keys are
`category|class.name(descriptor)` and `category|class-name` — invocation counts
are deliberately *not* part of the key, so a brand-new stub with
`invocations: 0` fails the ratchet exactly like an invoked one. **An uninvoked
stub is still a stub.** An entry that regresses from a closed status back to
`open` also fails.

Closed blockers that disappear are reported but do not fail; refresh the
baseline with `--update-baseline` to lock the improvement in.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | ratchet failure — the open blocker set grew |
| 2 | usage / configuration error: bad arguments, bad status ledger, undeterminable JDK feature version, or `--check` with no committed baseline |
| 3 | a partial result where a complete one was required |

### Partial results

A missing or malformed dump produces a **partial** result, never a fabricated
or silently-zero one:

- `"partial": true` at the top of both files;
- `inputs` records `present` / `missing` / `malformed` / `not-provided` per role;
- `notes` carries an explicit sentence saying the dump is *not* evidence of a
  clean run;
- a warning is printed to stderr regardless of `--quiet`;
- `--check` and `--update-baseline` refuse (exit 3) unless `--allow-partial`.

If **no** input can be read at all, nothing is written and the tool exits 3. An
empty blocker file on disk would read as "nothing is broken", which is the one
outcome that must never be possible.

---

## CI

The tool needs only `python3` and the dump files; it does not build anything.
It fits either as a step in an existing job that already provisions a JDK, or as
its own job.

The `jdk-only-audit` job already runs `sh scripts/jdk-only-census.sh`, which
generates both artifact pairs. Turning that step into a ratchet is one
environment variable — `BLOCKERS: check` — and needs no new step; the script
exits 5 and names the reason when the open set grows. Until a baseline pair is
committed for the image CI pins, `check` correctly refuses (there is nothing to
ratchet against), so commit `baselines/jdk-25-*.json` first. The equivalent
standalone step is:

```yaml
      - name: JDK-only blocker artifacts
        run: |
          mkdir -p target/jdk-only-audit
          ./target/release/cratonvm \
            --real-jdk --java-home "$JAVA_HOME" \
            --jdk-only-report              target/jdk-only-audit/report.json \
            --dump-native-registry         target/jdk-only-audit/registry.json \
            --dump-class-origins           target/jdk-only-audit/classes.json \
            --dump-missing-natives-grouped target/jdk-only-audit/missing.json \
            -cp vm-cli/tests/resources HelloWorld
          python tools/jdk-only-blockers/blockers.py \
            --native-registry         target/jdk-only-audit/registry.json \
            --missing-natives-grouped target/jdk-only-audit/missing.json \
            --class-origins           target/jdk-only-audit/classes.json \
            --jdk-only-report         target/jdk-only-audit/report.json \
            --out-dir                 target/jdk-only-audit \
            --check

      - name: Upload JDK-only blocker artifacts
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: jdk-only-blockers
          path: target/jdk-only-audit/jdk-*.json
```

Notes for whoever wires this up (this directory does not own `.github/`):

- **CI pins JDK 25 everywhere today.** Every `actions/setup-java` step in
  `.github/workflows/ci.yml` uses `java-version: '25'`, including
  `real-path-coverage` and `difftest-gate`. A 17/21/25 matrix is a proposal, not
  current coverage — and the artifacts are only valid for the image that
  produced them, so each matrix leg needs its own committed baseline pair.
- Run `--check` **without** `continue-on-error` to make it blocking, and upload
  the artifacts with `if: always()` so a failure ships its own evidence.
- Keep `--allow-partial` out of CI. A partial run in a gate is a green light
  for a dump that did not get written.

## Self-test

```bash
python tools/jdk-only-blockers/selftest.py
# or, from the same entry point as the census itself:
sh scripts/jdk-only-census.sh --selftest
```

`--selftest` is handled before every prerequisite check in the census script,
so it needs no `cratonvm` binary and no `JAVA_HOME` — it is safe in a lint or
docs job that provisions neither.

51 assertions over synthetic dumps in both emitter shapes: generation,
determinism (byte-identical reruns, no CRLF, no absolute paths), the ratchet
(including that a never-invoked stub fails it and an extra allowed generated
class does not), the status ledger, and every degraded-input path. No VM build,
no JDK, no network. Safe to run in the docs/lint job.

## Maintenance

`blockers.py` parses the JDK module table out of `classify_jdk_module` in
`vm/src/vm/vm_init.rs` at run time, so the two cannot drift; the embedded copy
is only a fallback, and using it is recorded in `module_table_source` and in
`notes`. It is used solely for rows that arrive without a module — the grouped
dump's own attribution always wins.

New `ClassOrigin` variants must be added to `ALLOWED_GENERATED_ORIGINS` or
`REAL_BYTES_ORIGINS` in `blockers.py`. Until then an unrecognised tag marks the
run partial and names itself in `notes`, which is the intended failure mode: a
new origin must be classified by a human, not defaulted into "allowed".

## See also

- [`docs/feature-designs/jdk-only-mode.md`](../../docs/feature-designs/jdk-only-mode.md) — the normative contract
- [`jdk-only-audit.md`](../../audits/jdk-only-audit.md) — the reproducible audit, §6 defines these artifacts
- [`docs/jdk-only-native-review.md`](../../docs/jdk-only-native-review.md) — the per-native promotion checklist
- [`docs/known-issues/jdk-only/runtime-services-blocker-inventory.md`](../../docs/known-issues/jdk-only/runtime-services-blocker-inventory.md) — the blocker inventory
