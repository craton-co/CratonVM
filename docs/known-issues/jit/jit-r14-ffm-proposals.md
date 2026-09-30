# JIT round 14, lane ffm: proposals (ranked)

Status: OPEN (proposal book; ideas, not work items, until the owner queues one)
Area: FFM (`native-builtins/src/panama*.rs`, `phases_late/foreign_ffm.rs`, `lang_invoke.rs` layout handles)
Found by: round 14 wave 1 lane ffm

## FFM14-1: resolve an upcall's direct target to a method id once, not by name per call

- Benefit: round 14 wave 1's `DirectStatic` (`panama_upcall.rs`) still hands
  `invoke_static_settling` three strings per upcall, which the VM resolves (class by name, then
  the method) unless the settled owner short-cuts part of it. Resolving once at stub creation to
  a `(ClassId, method index)` and invoking by id would take the name lookups off every upcall
  (the qsort comparator shape makes ~530 000 upcalls in `R12UpcallQsort ints`).
- Cost: small-medium: a `NativeContext::invoke_static_by_method_id` (or reuse whatever the JIT's
  direct-call helpers use), plus class-redefinition invalidation (drop the id when the class is
  redefined; fall back to the strings).
- Risk: medium (redefinition, class unloading of the owner -- the stub keeps its target handle
  alive, which keeps the class alive).
- First step: measure what `invoke_static_settling` costs per call with a settled owner
  (`CRATONVM_FFM_UPCALL_DIRECT_STATIC` on/off on `R14FfmUpcallDirect`), before writing anything.

## FFM14-2: extend the direct upcall road to `findVirtual` on a bound receiver and to `findStatic` + `asType` identity

- Benefit: callback libraries often bind an instance (`findVirtual(..).bindTo(listener)`); those
  still run the whole `mh_dispatch` static/virtual arm per upcall.
- Cost: small: a `mh_plain_bound_virtual_leaf` beside `mh_plain_static_leaf`, the receiver kept
  as a stub root (the registry already scans `target`; a second root slot is needed).
- Risk: low-medium (null receiver, receiver class redefinition).
- First step: census which handle shapes reach `upcall_dispatch` in the Tomcat/Netty FFM paths
  (`CRATONVM_DBG_MH_DISPATCH=1` on one run).

## FFM14-3: one per-VM record for the default lookup's libraries

- Benefit: round 14 loads `syslookup` once per default-lookup OBJECT (cached in the receiver);
  a per-VM slot (next to `panama::global_arena_handle`) would load it once per VM and let the
  default lookup search exactly the C runtime + `syslookup`
  (`r14w1-ffm-symbol-lookup-search-set-and-scope-FIXED-20260929.md` item 1).
- Cost: small (a per-VM cell like the global arena's; the per-VM statics ratchet counts a new
  `static` -- reuse the existing per-VM map instead).
- Risk: low.
- First step: move the cached index from the receiver slot into that map, keyed by `vm_identity`.

## Round 14 wave 3 (lane ffm): FFM14-3 landed

`panama::syslookup_cell` holds each VM's `syslookup` `funcs` table address (`-1` unavailable),
keyed by `vm_identity` and dropped by `forget_vm_ffm_singletons`; a fresh `defaultLookup()`
receiver fills its slot from it instead of loading again. The "search exactly the C runtime +
`syslookup`" half is `NativeSystemAccess::find_process_native_symbol` +
`CRATONVM_FFM_DEFAULT_LOOKUP_C_RUNTIME`, live once the VM patch
`r14w3-ffm-process-symbol-lookup-vm-patch-FIXED-20260929.md` is applied.

## FFM14-4: allocation-free `get(ADDRESS)` in compiled code

- Benefit: every address read allocates a segment carrier (HotSpot's C2 scalar-replaces it).
  A pointer-chasing loop (`p = p.get(ADDRESS.withTargetLayout(NODE), NEXT)`) allocates one per
  hop.
- Cost: large (escape analysis of the carrier through the JIT's FFM element fast path).
- Risk: medium.
- First step: count carrier allocations in a linked-list walk probe
  (`CRATONVM_DBG_...` allocation census) to size the win.
