# WildFly domain no-JIT boot: narrow stale-ref long tail — canary-only, now isolated to synthetic-class objects going stale at native-entry pins

Status: OPEN — narrow residual, updated 2026-07-17 (second session). This doc
replaces `wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`,
whose five root-cause families were all closed by
`fix/wildfly-cce0079-residuals3-20260717` (full mechanism writeup:
`docs/internal/fixed-suite-bugs/wildfly-cce-residuals-blocked-region-and-pin-waves-FIXED.md`;
the retired parent doc's full history:
`docs/internal/fixed-suite-bugs/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root-RETIRED.md`).

## Closed by the 2026-07-17 second-session waves (all merged to dev)

1. **XNIO OptionMap/Builder registry roots** — `OptionValue::Obj(ObjectRef)`
   entries in the Rust-side `Registries.maps`/`.builders` were neither GC
   roots nor remapped: every registered OptionMap's object values went stale
   on the first moving GC (canary-caught live: `HttpReadListener.<init>` →
   `native_option_map_get_int` unboxing a stale boxed Integer, thread
   `cratonvm-xnio-accept-1`). `OptionMapInner.entries` is now behind a leaf
   Mutex, scanned by `gc_scan_xnio_future_roots` and remapped by
   `gc_update_xnio_future_refs`; every reader copies entries out before
   allocating (lock-order safety vs the GC scan).
2. **`native_properties_put_all` stale receiver** (canary-caught:
   `CorbaNamingService.<init>` → `put_kv` → identity read through a stale
   header): the snapshot/side-key/chm-extra helpers and the per-entry
   iterator dispatches move `this`/`other`/entry refs; the whole function is
   now pin-disciplined per use, including the branch-2 Map.entrySet walk.
3. **`native_map_put_evict_pinned` node-population refresh** + **`tm_insert_at`
   / `tm_remove_at` per-store refresh** — every ref crossing a store is
   re-read through its pin immediately before its own store (defense in
   depth; see "refuted" below for what these did NOT turn out to fix).
4. **Address-keyed side tables** (silent lost/aliased-entry class, found by
   audit alongside this hunt): JCA `kpg_algo/keysize/name/bcprov` tables
   re-keyed by identity-hash+generation; `classloader_value_sidetable`
   re-keyed AND its object values var-handle-rooted (hot on Spring Boot
   ServicesCatalog); http2 BodySubscriber map, `http_url_connection`
   `real_body_streams` (plus an unpinned-BAOS-across-alloc fix),
   `deprecated_lang` Thread.stop/suspend maps — all converted to GC-stable
   keys and rooted values. `cargo test -p cratonvm-native-builtins --lib`:
   3002/0.
5. **Diagnostic-infrastructure repair** (changes how to read ALL prior
   sessions' evidence): `debug_forwarded_target` gated on `is_heap_addr`,
   which does NOT cover the quarantine RING — so the PIN-STALE (stale at
   pin time) and PIN-TABLE-STALE (stale in pin table at read time) canaries
   were **structurally blind** to exactly the addresses they exist to
   catch. Every "PIN probes silent" datapoint before this repair is void.
   The probe now also accepts ring-arena addresses. Also added:
   `[storechk]` prints thread+cycle, `PIN-STALE` prints the forwarded
   object's class, and a `[SETFIELD-GC]` epoch probe (below).

## Mechanisms REFUTED this session (do not re-chase)

- **GC-inside-`ctx.set_field`**: the `[SETFIELD-GC]` epoch probe (minor-GC
  count captured at set_field entry vs exit, BLOCKGC-gated) fired **zero**
  times across full canary batches that still produced staleness — no GC
  ever completes inside a plain ref store. The T21/T22 "value current at
  refresh, stale at store" inference was an artifact of the blind PIN
  probes (above): the value was stale all along.
- **Excluded-while-running (blocked-region exit/census race)**:
  `CRATONVM_DBG_BLOCKED_ACCESS=warn` stayed silent across full firing
  batches.
- **Pin-table remap gap**: with the repaired probe, PIN-TABLE-STALE reports
  are always PRECEDED by a PIN-STALE for the same address — the table entry
  was stale at pin time, not de-remapped later.

## What remains (the actual tail)

With the repaired probes, every remaining firing reduces to **PIN-STALE at
a native's ENTRY pins**: the native receives an already-stale reference.
Sites captured (T24/T25, d24/d25 binaries):

- `native_stream_for_each` (entry pins; lazy-spliterator branch) — HC and
  server-one, repeatedly.
- `native_hs_iterator` → `collect_view_snapshot_ordered` (entry pin).
- `native_service_builder_install` (+ its `wire_provides_injectors`) — MSC.
- The stale object's class prints as `cid=214748xxxx` — **high-bit-tagged
  synthetic class ids** (lambda-proxy/synthetic classes absent from
  `class_manager`), i.e. the stale objects are lambda/proxy-shaped
  (consumers/spliterators/injector lambdas), NOT plain data objects.

Since `safe_native_call_impl` heals+pins every OBJECT ARG at native entry
(the `load_and_forward` barrier — verified ring-capable), the stale ref
must enter via a path that bypasses that barrier. Prime suspects, in order:

1. **Synthetic/lambda-proxy objects whose refs live in a Rust-side registry
   missed by root scan/remap** (the OptionMap pattern again, for the lambda
   registry / stream chain / MSC injector state) — the high-bit class ids
   strongly hint at the lambda-proxy machinery's own side tables.
2. A field-remap gap for **selective promotion** in the non-moving young
   sweep (promoted objects relocate; check that all REACHABLE holders'
   fields — not just roots/monitors/overlays — are rewritten through
   `pointer_map` on that path).
3. The `execute_invokevirtual_cached` pop-to-call window on inline-cache
   MISS (resolution can load classes between arg pop and the heal barrier —
   the barrier heals AFTER, so this should be covered; verify the barrier
   is reached on every miss path).

## How to pick this up

Worktree/probes: `/data/data/wt-cceres3-20260717/probes/` (Azure host) —
`run-domain.sh` / `batch.sh`, frozen binaries `cratonvm-cceres3-d1..d25`
(d25 = all fixes + full diagnostics), logs + `summary.txt` (batches
T18..T25 are this session's; each header line names the build). Repro:
domain batch with `CRATONVM_DISABLE_JIT=1 CRATONVM_DBG_STALE_OBJREF=1
CRATONVM_DBG_STALE_OBJREF_CYCLES=8 CRATONVM_DBG_BLOCKGC=1
CRATONVM_DBG_UNPIN_RING=1 RUST_BACKTRACE=1` (~most boots fire within
600 s; several boots now reach `WFLYSRV0025` even under the canary).

Start from a fresh PIN-STALE capture (it now prints the stale object's
class + the pinning native's backtrace): resolve the synthetic class id to
its lambda registry entry (`lambda_proxy_host`/`lambda_functional_interface`
by cid), then audit where THAT object's ref is held on the Rust side across
GCs — mirror the `gc_scan_xnio_future_roots`/`gc_update_xnio_future_refs`
scan+remap pair (or var-handle roots) for the owning registry. The pin rule
everything else fell to: the pin stack is strictly LIFO per scope — one
truncate from the scope's first pin; never unpin per-iteration pins pinned
before later ones; callbacks must never pin into a callee's scope (use JNI
global roots for accumulate-under-callback, cf. `stream_apply_chain_full`).

NOTE for whoever continues on the shared Windows checkout: the local
`vm/src/vm/vm_exec.rs` carries another session's uncommitted WIP
(TypeArgAnnotations) — never whole-file-sync it to the probe worktree; use
the anchored surgery scripts (`vm_exec_surgery*.py` in the worktree root)
and commit vm_exec changes on the remote side only.
