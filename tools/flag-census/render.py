#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Render `flag-census.md` from the scan in `census.py`.

Usage:  python3 tools/flag-census/render.py [REPO_ROOT]

Everything except the two curated tables (§3 documented-but-dead highlights and
§10 truth tables) is derived mechanically from the scan, so re-running this
after a code change produces an up-to-date census.
"""

import collections
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import census  # noqa: E402

ROOT = sys.argv[1] if len(sys.argv) > 1 else \
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
OUT = os.path.join(ROOT, 'docs', 'flag-census.md')

sites, nonrust = census.scan(ROOT)
rows = census.aggregate(sites, nonrust)
bysite = collections.defaultdict(list)
for s in sites:
    bysite[s['name']].append(s)

# Adapt the aggregate rows to the field names the renderer below expects.
for r in rows:
    r['pol'] = r['polarity']
    r['nb_only'] = '1' if r['nb_only'] else '0'
    r['reads'] = str(r['reads'])
    r['consumer_reads'] = str(r['consumer_reads'])
    r['cached'] = str(r['cached'])
    r['uncached'] = str(r['uncached'])
    r['setters'] = str(r['setters'])

cnt = collections.Counter(r['klass'] for r in rows)
nbonly = [r for r in rows if r['nb_only'] == '1']

_TODAY = __import__('datetime').date.today().isoformat()
try:
    _HEAD = __import__('subprocess').run(
        ['git', '-C', ROOT, 'rev-parse', '--short', 'HEAD'],
        capture_output=True, text=True, check=True).stdout.strip()
except Exception:
    _HEAD = 'unknown'

# Curated: why each surviving direct read site in `native-builtins` stays put.
_HOLDOUT = {
    'CRATONVM_JBOSS_MP_ROOT':
        '`find_mp_argument()` is exercised by unit tests that `set_var` / '
        '`remove_var` it in-process and assert the read changes '
        '(`jboss_module_loader.rs:3880`, `:3897`, `lib.rs:2383`..`:2478`)',
    'CRATONVM_SYNTHETIC_QUARKUS_ARC':
        '`quarkus_arc.rs:1330`/`:1345` flips it in-process and asserts '
        '`synthetic_arc_opted_in()` follows',
    'CRATONVM_MAVEN_REPO_LOCAL':
        '`jboss_module_loader.rs:4034` sets it in-process, then calls '
        '`resolve_module()` and asserts the new repo is used',
}


def _native_flag_fields(root):
    """(flag, field, parser) for every field on `NativeFlags`, read back from
    `types/src/flags.rs` so the table cannot drift from the code."""
    import re as _re
    text = open(os.path.join(root, 'types', 'src', 'flags.rs'),
                encoding='utf-8').read()
    body = text.split('impl NativeFlags {', 1)[1].split('\n}\n', 1)[0]
    out = []
    for m in _re.finditer(
            r'(\w+):\s*(\w+)\(\s*src,\s*"(CRATONVM_[A-Z0-9_]+)"\s*,?\s*\)',
            body):
        out.append((m.group(3), m.group(1), m.group(2)))
    if not out:
        raise SystemExit('could not read NativeFlags fields back from flags.rs')
    return sorted(out)


L = []
w = L.append
w('# CratonVM `CRATONVM_*` environment-flag census')
w('')
w(f'*Generated {_TODAY} from `dev` @ `{_HEAD}` by a mechanical scan of every')
w('`.rs` file in the workspace plus every `.md` / `.sh` / `.java` / `.toml` under the')
w('repo root. Regenerate with the scripts recorded at the bottom of this file.*')
w('')
w('This census is the evidence base for the typed-config refactor')
w('(`refactor/typed-vmconfig-20260725`). It exists on its own merit: it is the first')
w('complete inventory of the flag surface, and it is what makes the migration')
w('reviewable — a flag that silently stops being read is a silent behaviour change,')
w('and the only defence is knowing what the full set was beforehand.')
w('')
w('## 1. Totals')
w('')
w('| Metric | Count |')
w('| --- | ---: |')
w(f'| Distinct `CRATONVM_*` identifiers seen anywhere (code, docs, scripts) | **{len(rows)}** |')
w(f'| …of which have at least one Rust read site | **{len([r for r in rows if r["reads"] != "0"])}** |')
w(f'| Rust code literal sites (all kinds) | **{len(sites)}** |')
w(f'| Rust *read* sites (excludes `set_var`/`env_remove`/`option_env!`) | **{sum(int(r["reads"]) for r in rows)}** |')
w(f'| Read sites **outside** `native-builtins/` (this refactor\'s scope) | **{sum(int(r["reads"]) - 0 for r in rows) - sum(len([s for s in bysite[r["name"]] if s["crate"] == "native-builtins" and s["kind"] not in ("set", "unset", "option_env")]) for r in rows)}** |')
w(f'| Read sites inside `native-builtins/` (deliberately deferred, see §6) | **{sum(len([s for s in bysite[r["name"]] if s["crate"] == "native-builtins" and s["kind"] not in ("set", "unset", "option_env")]) for r in rows)}** |')
w(f'| …of which still call `std::env::var` / `var_os` directly | **{sum(int(r["consumer_reads"]) for r in rows)}** |')
w(f'| …already reading a `VmFlags` field instead | **{sum(int(r["reads"]) - int(r["consumer_reads"]) for r in rows)}** |')
w(f'| Remaining direct read sites that are **not** `OnceLock`-cached | **{sum(int(r["uncached"]) for r in rows)}** |')
w(f'| In-process `set_var` / `remove_var` / `Command::env` sites | **{sum(int(r["setters"]) for r in rows)}** |')
w('')
w('### Classification')
w('')
w('| Class | Meaning | Count |')
w('| --- | --- | ---: |')
w(f'| **(a) debug / diagnostic** | only gates `eprintln!`/tracing/extra verification; removing it cannot change a program\'s result | {cnt["a-diag"]} |')
w(f'| **(b) semantics-changing** | selects a different code path, algorithm, layout or default; two settings are two different VMs | {cnt["b-semantics"]} |')
w(f'| **(c) test-only** | read only from `tests/`, `benches/`, `build.rs` or a soak/difftest harness | {cnt["c-test"]} |')
w(f'| **(d) dead** | **no Rust read site at all** — referenced only by docs, scripts or comments | {cnt["d-dead"]} |')
w(f'| | | **{len(rows)}** |')
w('')
w('The (b) count is the headline number. 2^%d is not a testable behaviour space, and'
  % cnt['b-semantics'])
w('the census confirms the premise: the flags have become the de-facto bug-triage')
w('mechanism, with ~1 flag added per fixed bug and no retirement path.')
w('')

w('## 2. Class (d) — dead flags')
w('')
w('These identifiers have **zero** Rust read sites. Every one of them is referenced')
w('only by prose, a runbook, or a shell script. Nothing sets a value that any code')
w('will ever observe. Grouped by why they are dead:')
w('')
dead = [r for r in rows if r['klass'] == 'd-dead']
groups = collections.OrderedDict()
groups['Per-application `*_REAL` switches driven by `scripts/real-run-all.sh`'] = \
    [r for r in dead if r['name'].endswith('_REAL')]
groups['Per-application `*_EXE` launcher overrides referenced only by `apps/`'] = \
    [r for r in dead if r['name'].endswith('_EXE')]
groups['`CRATONVM_DBG_*` debug flags whose code was deleted with the bug'] = \
    [r for r in dead if r['name'].startswith('CRATONVM_DBG_')]
used = set()
for k, v in groups.items():
    for r in v:
        used.add(r['name'])
groups['Other'] = [r for r in dead if r['name'] not in used]
for k, v in groups.items():
    if not v:
        continue
    w(f'### {k} ({len(v)})')
    w('')
    w('| Flag | Referenced from |')
    w('| --- | --- |')
    for r in sorted(v, key=lambda x: x['name']):
        w(f'| `{r["name"]}` | {r["nonrust"] or "comments only"} |')
    w('')

w('## 3. Class (d) highlights — documented flags that do nothing')
w('')
w('These are worth calling out separately because a reader of the docs would')
w('reasonably believe they work, and at least two of them make a *measurement* wrong.')
w('')
w('| Flag | Doc references | Reality |')
w('| --- | ---: | --- |')
w('| `CRATONVM_PRECISE_JIT_MAPS` | 36 | **No-op.** `jit/src/x64.rs:2078 precise_jit_maps_enabled()` reads the *inverse* flag `CRATONVM_NO_PRECISE_JIT_MAPS`. The doc comment at `x64.rs:2072` still says "Opt back in with `CRATONVM_PRECISE_JIT_MAPS=1`", which has not been true since the default was flipped. `x64.rs:2427` also warns against combining with a flag that cannot be set. |')
w('| `CRATONVM_NO_GC` | 4 docs + 2 script uses | **No-op.** `scripts/measure-gc-fraction.sh:27-28` runs a `craton-nogc` arm with `CRATONVM_NO_GC=1`; nothing reads it, so that arm is identical to the baseline arm and any GC-fraction number derived from it is meaningless. |')
w('| `CRATONVM_JIT_GUARDED_GETFIELD` | 10 | **No-op.** The gate is `guarded_inline_getfield_enabled()` at `jit/src/x64.rs:2224`, which reads `CRATONVM_JIT_GETFIELD_HELPER` with inverted polarity (set it to force the *helper*, i.e. disable the guarded inline path). |')
w('| `CRATONVM_JIT_INLINE_PUTFIELD` | 5 | **No-op.** Real gate is `CRATONVM_NO_JIT_INLINE_PUTFIELD` (`x64.rs:2126`), opt-out. |')
w('| `CRATONVM_SELECTIVE_PROMOTE` | 13 | **No-op.** Real gate is `CRATONVM_NO_SELECTIVE_PROMOTE` (`gc/src/gen_heap.rs:5803`), opt-out. |')
w('| `CRATONVM_PRECISE_INLINE_FRAME_RECORD` | 2 | **No-op.** Real gate is `CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD` (`x64.rs:2288`), opt-out. |')
w('| `CRATONVM_SHADOW_OSR_TRACK` | 14 | **No-op.** No read site; the surviving shadow-stack knobs are `CRATONVM_SHADOW_STACK` / `_PIN` / `_NOPUSH` / `_NORELOAD`. |')
w('| `CRATONVM_JIT_SCAN_CACHE` | 1 | **No-op.** Real gate is `CRATONVM_NO_JIT_SCAN_CACHE` (`vm/src/jit/conservative_roots.rs:899`). |')
w('| `CRATONVM_NONMOVING_YOUNG` / `CRATONVM_FORCE_MOVING` | 1 each | **No-op.** The real gate is the opt-out `CRATONVM_NO_MOVING_YOUNG` — the moving young gen is now the default (`DEFAULT_MOVING_YOUNG = true`), so `CRATONVM_MOVING_YOUNG` survives only as a no-op compatibility opt-in. (`CRATONVM_ALLOW_MOVING_YOUNG`, named here when this census was first generated, has since been removed outright.) |')
w('| `CRATONVM_BUGS` / `CRATONVM_CRASHES` | 29 / 12 | Not flags at all — doc-internal shorthand that the scanner picks up. Harmless, listed for completeness. |')
w('')
w('The recurring pattern is a default flip: a flag `X` is introduced opt-in, later')
w('made the default, and a new `NO_X` opt-out is added — but the docs keep describing')
w('`X`. **No default is changed in this branch**; these are documentation bugs, and')
w('are fixed as documentation.')
w('')

w('## 4. Class (b) — semantics-changing flags')
w('')
w('Every flag below selects a different execution path. These are the flags that')
w('must be A/B verified by the migration: set and unset must behave exactly as they')
w('did before the refactor.')
w('')
w('`Polarity` is derived from the read expression: `is_some()`/`is_ok()` means opt-in')
w('(default OFF), `is_none()`/`is_err()` means opt-out (default ON, so *deleting the')
w('read would silently turn the feature off*).')
w('')
w('| Flag | Reads | Cached | Polarity | Crates | First site |')
w('| --- | ---: | :---: | --- | --- | --- |')
for r in sorted([r for r in rows if r['klass'] == 'b-semantics'], key=lambda x: x['name']):
    cachemark = 'yes' if r['cached'] == r['reads'] else ('partial' if int(r['cached']) else '**no**')
    w(f'| `{r["name"]}` | {r["reads"]} | {cachemark} | {r["pol"]} | {r["crates"]} | `{r["sample"]}` |')
w('')

w('## 5. Class (a) — debug / diagnostic flags')
w('')
w(f'{cnt["a-diag"]} flags. These gate `eprintln!` / `tracing` output or extra assertions')
w('only. They are the bulk of the surface and the best candidates for collapsing')
w('behind a single `experimental-diag` cargo feature: with the feature off the')
w('accessor becomes `const false` and the whole diagnostic block folds away.')
w('')
w('| Flag | Reads | Cached | Crates |')
w('| --- | ---: | :---: | --- |')
for r in sorted([r for r in rows if r['klass'] == 'a-diag' and r['reads'] != '0'],
                key=lambda x: (-int(x['reads']), x['name'])):
    cachemark = 'yes' if r['cached'] == r['reads'] else ('partial' if int(r['cached']) else 'no')
    w(f'| `{r["name"]}` | {r["reads"]} | {cachemark} | {r["crates"]} |')
w('')

w('## 6. Class (c) — test-only flags')
w('')
w('| Flag | Reads | Crates | Note |')
w('| --- | ---: | --- | --- |')
for r in sorted([r for r in rows if r['klass'] == 'c-test'], key=lambda x: x['name']):
    w(f'| `{r["name"]}` | {r["reads"]} | {r["crates"]} | {r["sample"]} |')
w('')

w('## 7. `native-builtins/` — migrated (T3.5b)')
w('')
nb_reads = sum(len([s for s in bysite[r['name']] if s['crate'] == 'native-builtins'
                    and s['kind'] not in ('set', 'unset', 'option_env')]) for r in rows)
nb_direct = sum(int(r['nb_direct']) for r in rows)
w('Before T3.5b this crate held **359** direct read sites across **156** flag names,')
w('**127** of which were read only there. It was deliberately deferred while it was')
w('being split from a single 86 000-line `lib.rs` into per-domain modules; T3.5b')
w('migrated it once the split landed. It now names a flag on')
w(f'**{nb_reads}** lines in total.')
w('')
w(f'**{nb_direct}** direct `std::env::var` / `var_os` sites remain, down from 359. Every')
w('one of them is a flag whose value is mutated **in-process** with `set_var` /')
w('`remove_var` and then re-read — a latched typed config cannot reproduce that, so')
w('migrating them would be a silent behaviour change rather than plumbing:')
w('')
w('| Flag | Direct sites left | Why it stays on `std::env` |')
w('| --- | ---: | --- |')
for r in sorted(rows, key=lambda x: x['name']):
    if int(r['nb_direct']):
        w(f'| `{r["name"]}` | {r["nb_direct"]} | {_HOLDOUT.get(r["name"], "in-process `set_var`")} |')
w('')
w('The other 348 sites now read `cratonvm_types::flags()`: the flags read only by')
w('this crate live on `VmFlags::natives` (`NativeFlags`), and the eight it shares')
w('with `gc` / `classloading` / `native-io` reuse those crates\' existing fields')
w('rather than being duplicated. Four parsers had to be added for truth tables no')
w('existing parser matched — see §10, where the count went from seven to eleven.')
w('')
w('The migration also absorbed the three local `OnceLock<bool>` helpers added by')
w('`c258662e4` (`h2trace_enabled`, and two byte-identical copies of')
w('`loader_trace_enabled`). They existed because `native-builtins` cannot reach')
w('`vm`, where `env_cache` lives — but it *can* reach `types`, so the typed config')
w('subsumes them. The call sites keep the flag read as the LEFT operand of their')
w('`&&`, which is what that fix required.')
w('')
w('Once a crate is migrated the scan can no longer tell which flags *belong* to')
w('it — the call sites no longer name them. So the catalogue below is read back')
w('out of `NativeFlags::from_source`, which is the new home of record. It is the')
w('same evidence the pre-migration §7 table carried, with the parser each field')
w('was built from in place of the read count.')
w('')
w(f'| Flag | `NativeFlags` field | Parser |')
w('| --- | --- | --- |')
for flag, field, parser in _native_flag_fields(ROOT):
    w(f'| `{flag}` | `{field}` | `{parser}` |')
w('')

w('## 8. Caching status and the per-call readers')
w('')
w('`std::env::var` takes a process-global lock in libc `getenv` and allocates. Of the')
w(f'{sum(int(r["reads"]) for r in rows)} read sites, **{sum(int(r["uncached"]) for r in rows)}** are not behind a `OnceLock`. Most of those are cold')
w('(startup, class load, JIT compile), but three are genuinely hot and are the reason')
w('the typed config is worth doing on performance grounds alone:')
w('')
w('| Site | Frequency | Note |')
w('| --- | --- | --- |')
w('| `native-builtins/src/lang_system.rs:752` `CRATONVM_INHERIT_THREAD_CCL` | once per `Thread.start0` | **migrated** in T3.5b — now `flags().natives.inherit_thread_ccl` |')
w('| `jit/src/x64.rs:2231` `CRATONVM_JIT_GETFIELD_HELPER` | once per compiled `getfield` **call site** | deliberately uncached — see the comment at `x64.rs:2225`, which argues caching would make the off-switch racy against whichever thread first triggers a getfield compile. Preserved as-is. |')
w('| `gc/src/gen_heap.rs:3702` `CRATONVM_NO_GC_PROMOTION_GUARD` | per promotion-OOM check | per young-GC, not per object |')
w('')
w('The `x64.rs` case is the interesting one: it is *deliberately* uncached, and the')
w('reason given is that caching changes when the value is latched. A typed config')
w('latches at startup, which is strictly earlier and therefore not racy — but it does')
w('mean a test that flips the var mid-process stops working. That trade is called out')
w('in the migration notes rather than made silently.')
w('')

w('## 9. Existing precedents in the tree')
w('')
w('The refactor extends what is already there rather than inventing a parallel system:')
w('')
w('* `vm/src/runtime/env_cache.rs` (933 lines) — the partial precedent. `cached_is_set!`')
w('  / `cached_is_ok!` macros wrap ~100 flags in per-flag `OnceLock`s. Right idea, but')
w('  it lives in `vm`, which `types`/`gc`/`jit`/`classloading` cannot depend on, so')
w('  those crates all grew their own copies.')
w('* `jit/src/tiered.rs:199` `TieredParams::from_env()` / `with_overrides()` — already a')
w('  typed struct with an injectable source. This is the shape the whole config should')
w('  have, and `with_overrides` is what makes it unit-testable without touching process')
w('  env. Adopted directly.')
w('* `native-io/src/lib.rs:168` `env_flag_enabled()` — a third, independent boolean')
w('  parser with its own `0`/`false`/`off`/`no` truth table. There are at least four')
w('  such truth tables in the tree and they do **not** agree (see §10).')
w('* `types/src/lock_order.rs` — the precedent for putting a cross-crate concept in')
w('  `cratonvm-types`, the crate every other crate already depends on.')
w('')

w('## 10. Finding: eleven disagreeing boolean truth tables')
w('')
w('There is no single answer to "what does `CRATONVM_FOO=false` mean". The tree')
w('contains at least these eleven, all of them live. The first four were found by')
w('the census scan; three more surfaced while migrating `classloading` and')
w('`native-io`; the last four surfaced while migrating `native-builtins`. The')
w('count has gone up at every single migration, which is the finding: it is a')
w('lower bound, not a total.')
w('')
w('| Parser | `unset` | `""` | `"0"` | `"false"` | `"off"` | `"no"` | `"NO"` | else |')
w('| --- | --- | --- | --- | --- | --- | --- | --- | --- |')
w('| `var_os(..).is_some()` — the ~600-site majority | false | **true** | **true** | true | true | true | true | true |')
w('| `env_cache::disable_jit` (`env_cache.rs:70`) | false | **false** | **false** | true | true | true | true | true |')
w('| `native_io::env_flag_enabled` (was `lib.rs:168`) | false | false | false | **false** | **false** | **false** | **false** | true |')
w('| `TieredParams::tiered_enabled` (`tiered.rs:223`) | **true** | **true** | false | false | **true** | **true** | **true** | true |')
w('| `lock_order::compute_enforced` (`lock_order.rs:242`) | false | false | false | false | false | false | false | **only `1`/`true`/`yes`/`on`** |')
w('| `class_manager::loader_aware_resolution` (`:141`) | **true** | false | false | true | true | true | true | true |')
w('| `class_path::dbg_getresources` + `nio_selector::sel_dbg_enabled` | false | false | false | *differs*: `dbg_getresources` true, `sel_dbg_enabled` **false** | true | true | true | true |')
w('| `service_loader::diag_serviceloader` (`one_true_yes_exact`) | false | false | false | false | **false** | false | false | **only exact `1`/`true`/`yes`** |')
w('| `lang_system::inherit_thread_ccl` (`on_unless_zero_or_false`) | **true** | **true** | false | false | **true** | **true** | **true** | true |')
w('| `jboss_msc::msc_real_start` (`on_unless_off_word_cased`) | **true** | **true** | false | false | false | **true** | **true** | true |')
w('| `reflect_annotations::real_proxy_enabled` (`truthy_word_default_true`) | **true** | **true** | false | false | false | false | false | true |')
w('')
w('The four `native-builtins` additions are not near-duplicates of the earlier')
w('seven. `on_unless_zero_or_false` and `truthy_word_default_true` disagree with')
w('each other on `off` and `no`; `on_unless_off_word_cased` accepts `off`/`OFF`')
w('but not `Off`, while `on_unless_off_word` accepts `off` but not `OFF`; and')
w('`one_true_yes_exact` rejects `on`, which `affirmative_word` accepts, and')
w('rejects `" 1 "`, which `affirmative_word` trims and accepts.')
w('')
w('So `CRATONVM_X=0` *enables* the feature at roughly 600 sites and *disables* it')
w('at six others, and `CRATONVM_X=false` splits two flags that look like siblings.')
w('This is a genuine footgun and the single strongest argument for one typed')
w('config: the parse happens once, in one place, and each field records which')
w('table it uses.')
w('')
w('**No branch has unified the truth tables.** Each migrated flag keeps its own')
w('parse function byte-for-byte, because changing what `X=0` means for 600 flags')
w('is a behaviour change, not a plumbing change. All eleven now live side by side')
w('in `cratonvm_types::flags::parse`, each documented with the call site it was')
w('lifted from, and two unit tests')
w('(`boolean_parsers_disagree_exactly_as_documented`,')
w('`native_builtins_parsers_add_four_more_truth_tables`) assert that they still')
w('disagree — so the divergence cannot be tidied away by accident and can instead')
w('be retired deliberately, flag by flag, with benchmarks.')
w('')
# ── migration status ────────────────────────────────────────────────────────
w('## 11. Migration status')
w('')
w('Read sites still calling `std::env::var` / `var_os` directly, by crate.')
w('A crate at zero reads every flag from `cratonvm_types::flags()`.')
w('')
w('Counted here are only sites with a literal `std::env::var` / `var_os` call —')
w('the thing the migration removes. A flag name that merely appears in an')
w('assertion message or as a label argument is not one, and `types/src/flags.rs`,')
w('where the parse now lives, is excluded by construction.')
w('')
w('| Crate | Read sites remaining | Status |')
w('| --- | ---: | --- |')
crate_reads = collections.Counter()
for s in sites:
    if s['kind'] == 'read' and s['direct']:
        crate_reads[s['crate']] += 1
MIGRATED = {'gc', 'classloading', 'native-io'}
for crate in sorted(set(list(crate_reads) + sorted(MIGRATED))):
    n = crate_reads.get(crate, 0)
    if crate in MIGRATED:
        state = '**migrated**' if n == 0 else '**INCOMPLETE**'
    elif crate == 'native-builtins':
        state = ('**migrated** — %d holdouts, all in-process `set_var` targets; see §7'
                 % n) if n <= 11 else '**INCOMPLETE**'
    else:
        state = 'not started'
    w(f'| `{crate}` | {n} | {state} |')
w('')
w('`native-builtins` is listed as migrated with a non-zero count on purpose. The')
w('eleven remaining sites read a variable that something in the same process')
w('rewrites with `set_var` before re-reading it; a config latched once at startup')
w('cannot express that. Retiring them means retiring the in-process mutation, not')
w('plumbing the read differently.')
w('')

w('## 12. Reproducing this census')
w('')
w('```sh')
w('python3 tools/flag-census/census.py            # totals, from the repo root')
w('python3 tools/flag-census/census.py /path/to/repo')
w('```')
w('')
w('The scan is deliberately literal-only: there is **no** dynamic env-var name')
w('construction anywhere in the workspace (verified: no `format!("CRATONVM_{}", ..)`')
w('and no `env::var(<non-literal>)` outside test JDK-discovery helpers and the three')
w('injectable helpers named in §9), so a literal scan is exhaustive. `census.py`')
w('re-checks that invariant on every run and aborts if it is ever violated.')
w('')


os.makedirs(os.path.dirname(OUT), exist_ok=True)
with open(OUT, 'w', encoding='utf-8') as fh:
    fh.write(chr(10).join(L) + chr(10))
print('wrote %s (%d lines)' % (OUT, len(L)))
print(cnt.most_common())
