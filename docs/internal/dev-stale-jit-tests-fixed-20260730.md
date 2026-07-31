# Four stale JIT tests on `dev`, and four more that were passing vacuously (2026-07-30)

**Status:** ✅ FIXED. `cargo test -p cratonvm-vm` goes from 2,294 passed / 4
failed to **2,302 passed / 0 failed**.

## What was reported

An eight-item list of "pre-existing failures on `dev` @ `c8da3d918`", raised
while closing the CratonBench HashMap half-gap (that session used plain `dev`
as a control and found the same failures name-for-name, which is what proved
they were not its own).

## What was actually still broken

Half the list had already been fixed on `dev` by the time it was triaged — the
branch had moved from `c8da3d918` to `a5f44f3c3`:

| reported | status at `a5f44f3c3` |
|---|---|
| `types` `flag_surface::inventory_matches_the_checked_in_surface` (UTF-8 BOM in `flag-surface.txt`) | already fixed — the BOM came in with `e3cb2ab17` and was removed again by `16ec5d7ad` |
| `native-collections` `fork_join_pool_await_quiescence_registered` | already fixed — the test was rewritten as `fork_join_pool_invoke_registered_and_quiescence_left_to_native_builtins` |
| `interpreter` `hot_files_have_no_production_panics` | already fixed — `jit/src/x64.rs` is back at exactly its ratchet of 16 |
| `interpreter` `b3_gate_scans_full_production_body_of_interpreter` | already fixed |

The remaining four fail **deterministically in isolation** (each was run alone:
`0 passed; 1 failed`), so none of them is the process-global JIT-state
flakiness that `abf40220ab` chased. All four are stale tests, not product
regressions — each asserts behaviour a later, deliberate change replaced.

## 1-2. The `putfield` constructor pair

`classify_complex_ctor_with_putfield` and `complex_ctor_keeps_constructor_ban`
both got `left: Trivial, right: Complex`.

`allow_putfield_init` flipped its default to **true** on 2026-07-28: a
constructor whose body is only field stores is compilable. That flip is
documented at length in `skip_list.rs` (1846 ns/alloc banned vs 226 allowed —
8.2x — with `CtorCheck` proving byte-identical checksums across HotSpot,
`--nojit`, ban-on and ban-off), and it kept `CRATONVM_JIT_PUTFIELD_INIT=0` as a
kill switch. The two tests were left asserting the pre-flip answer.

`classify_init_complexity` now delegates to a pure
`classify_init_complexity_with(bytecode, allow_putfield)`; the gate read
(a `OnceLock`) stays in the public wrapper. That is what makes the kill switch
testable at all — before this, a single process could only ever observe one
side of it, so the documented escape hatch had **zero** coverage.

- `putfield_only_ctor_follows_the_putfield_gate` (renamed from
  `classify_complex_ctor_with_putfield`) pins both sides.
- `the_other_disqualifiers_ignore_the_putfield_gate` pins that `putstatic`,
  `monitorenter`/`monitorexit` and `invokedynamic` stay `Complex` regardless.
- `complex_ctor_keeps_constructor_ban` keeps its name and its point, but now
  uses `monitorenter`/`monitorexit` — a disqualifier the flag does not govern —
  so it tests the ban rather than the flag.
- `putfield_only_ctor_is_jit_eligible_by_default` pins the end-to-end result
  through `should_skip_jit_with_init`, which is what the compiler consults.

## 3. The proxy test

`generated_proxy_class_is_jit_eligible_after_proxy_jitcall_1_removal` got
`left: Some(JdkDynamicProxyTrampoline), right: None`.

Two bans, in sequence: PROXY-JITCALL.1 removed the original, and **SPR-PROXY.1
(2026-07-28) put it back**. The newer one stands and is not a tuning choice —
CratonVM implements proxy semantics at *dispatch*
(`proxy_invoke_handler_shared` / `proxy_annotation_handler_invoke` /
`annotation_proxy_dispatch_impl`), so a compiled proxy body is a third dispatch
path bypassing annotation-member coercion and the foreign-proxy `equals`
delegation. It was pinned by package bisection over eight configurations
against an `OutOfMemoryError` in
`beans.PropertyDescriptorUtilsPropertyResolutionTests`.

`configproxy-cglib-loaderid-fixed-20260727.md` read this correctly at the time
("a JIT-ban removal landed on `dev` while its test still asserts the old
policy") but had the direction backwards — it is the *ban* that is current.
The test is now `generated_proxy_class_is_never_jit_eligible_spr_proxy_1`,
covering both JDK proxy naming conventions (`jdk/proxyN/$ProxyM` and
`<pkg>/$ProxyM`) under both policies, plus a companion
(`ordinary_classes_named_proxy_are_not_caught_by_spr_proxy_1`) pinning that the
guard keys on the `$ProxyN` naming rule and not on "contains Proxy".

## 4. The shadow-window fixture — and the four tests it was silently voiding

`shadow_window_is_recovered_from_a_live_compiled_frame` panicked at
`window must resolve`.

`fake_shadow_thread` built a shadow buffer only `values.len()` slots wide and
aliased `end` to `top`. That was fine while `shadow_window_from_frame` merely
required non-null 8-aligned words. It stopped being fine when that resolver was
hardened against the band-verifier SIGSEGV (a `base` of `0x5555_0000_0004`
walked as a shadow window): it now requires the exact
`ShadowStack::ensure_allocated` shape — three 8-aligned addresses with
`end - base` **exactly** `DEFAULT_SHADOW_SLOTS * 8` and `top` inside
`[base, end]`. A three-slot buffer fails that.

**The interesting part is what else that broke.** Four sibling tests kept
passing:

- `nulled_thread_slot_publishes_nothing_rather_than_proving_everything`
- `shadow_window_is_unresolvable_without_the_layout_offsets`
- `shadow_window_rejects_an_unaligned_base`
- `shadow_window_rejects_a_window_wider_than_the_backing_buffer`

Every one of them asserts `is_none()` — and the fixture already resolved to
`None` *before* the mutation each test applies. They were asserting nothing.
This is the same failure shape the interpreter's own
`b3_gate_scans_full_production_body_of_interpreter` exists to catch in a
different file: a test that passes because its precondition collapsed, not
because its subject is correct.

The fixture now allocates the full `DEFAULT_SHADOW_SLOTS` buffer and derives
`end` instead of aliasing it, so the resolving test passes and the four
`is_none()` tests become load-bearing again.
`shadow_window_rejects_a_buffer_of_the_wrong_size` is added to pin the size arm
directly — the arm the old fixture violated by accident.

## Verification

Azure Linux bench host, worktree `/data/data/wt-devtests-20260730`, branched
from `dev` @ `a5f44f3c3`.

| target | before (`a5f44f3c3`) | after |
|---|---|---|
| `cratonvm-vm` lib | 2,294 passed / 4 failed | **2,302 passed / 0 failed** |
| `cratonvm-types` lib | 419 / 0 | 419 / 0 |
| `cratonvm-native-collections` lib | 86 / 0 | 86 / 0 |
| `cratonvm-jit` (all targets) | — | 1,251 / 0 |
| `cratonvm-gc` (all targets) | — | 914 / 0 |

The +8 in `cratonvm-vm` is 4 repaired and 4 added. Worth recording separately:
`cratonvm-jit` is clean here, so the `live_monitor_ops_execute_direct_runtime_stubs`
SIGSEGV that `resolvabletype-array-receiver-mic-guard-fixed-20260728.md` reports
aborting that binary on pristine dev is also gone (fixed on `dev` by
`fcc723007` / `abf40220ab`, not by this change).

Only one non-test line changed: `classify_init_complexity` splitting into a
wrapper plus a pure `classify_init_complexity_with`, a behaviour-identical
extraction. A fat-LTO release binary was built from the same tree
(`cratonvm-devtests-23b7582f11`) and smoke-run — `HashMapOnly 300000` returned
the exact checksum `1394997450000` — to confirm the extraction did not disturb
the product.
