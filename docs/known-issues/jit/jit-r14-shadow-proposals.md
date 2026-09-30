# JIT round 14, lane shadow: proposals (ranked)

Filed by round 14 wave 2, lane shadow. Ideas, not work items, until the owner queues one.

## SH14-1 -- One ancestor-walk helper instead of five copies (value: high; cost: medium)

**What.** The "walk the superclass chain for a registered native" logic exists five times:
`vm_exec.rs` `invoke_or_native`, `dispatch_virtual.rs` `populate_virtual_invoke_cache` and
the vtable fast path's shadow probe, `invoke.rs` `try_stackless_invoke` step 1, and
`vm/src/jit/helpers.rs` `resolve_native_owner_for_receiver`. They already disagree: the
redefine guard differs per copy (`redefine_immune_forced_native` vs
`redefine_immune_reflection_native` + `_layout_native` vs
`redefine_immune_forced_native_for_receiver`), only some asked the enforcement dial (the
stackless walk never did until this wave), and each needed its own edit for the retired-row
mask (`r14w2-shadow-invoke-or-native-retired-row-mask-patch-FIXED-20260929.md` is the fifth).
**Benefit.** The next dispatch rule (AGENTS.md's "real class bytes win") lands once; the
`[DIAL_DOOR]` census and the mask cannot drift between doors. **Cost/risk.** Medium: the
five copies differ in lock discipline (held guard vs `try_read`) and in what they return
(callback, id, owner name); a helper returning `(owner ClassId, NativeMethodId, NativeKind)`
under a caller-held `&ClassManager` fits all five. **First step.** Write
`fn ancestor_native(shared, cm, from: (ClassId, Option<ClassId>), m, d) -> AncestorNative`
in `native_override.rs` with the union of the rules, then switch the JIT site cache (the
copy that must "stay faithful" to `invoke_or_native`) and diff the site-cache refusal census
on the probe battery.

## SH14-2 -- A `[DIAL_DOOR]`-style row for the mask (value: medium; cost: small)

**What.** Count, per triple, how often a walk masked an ancestor `Bridge`
(`retired_row_masks_ancestor_bridge` returning `true`), and print it with
`--jdk-only-report`. **Benefit.** Tells the orchestrator which receivers the mask actually
serves on a corpus (today only the static gate says which PAIRS exist), and turns the kill
switch arm into a measurement. **Cost.** Small, but the counter needs a production reader
(`scripts/check-orphan-instruments.sh`) and the `DispatchDoor` enum lives in `vm_exec.rs`.
**First step.** Reuse `record_native_shadow_ran_over_bytecode`'s once-per-triple sink with a
new tag.

## SH14-3 -- Receiver-aware interface doors (value: medium; cost: medium)

**What.** A `Bridge` registered on an INTERFACE method (`java/util/Collection.toArray()`,
`native-collections` `native_al_to_array_entry`) is found by any door that looks natives up
by the call site's class, whatever the receiver; the retired-row mask covers superclass
walks only. **Benefit.** Closes the last way a retired receiver can be served by a native
from a `Collection`-typed call site. **First step.** List the doors that key on the CP class
for an interface call (the interpreter's slow path keys on the receiver's class --
`invoke.rs` `invoke_class`; `invoke_on_class_shared_inner`'s interface retarget and the JIT
`mh_carrier` / special paths need reading), then give `resolve_step1_native` the receiver
class and ask the mask along the receiver's chain up to the first declaring class.

## SH14-4 -- `Runtime$Version.feature` / `build` with a total mint (value: low; cost: small)

**What.** Retire the two rows (4 registrations) once `native_runtime_version`
(`native-builtins/src/lang_system.rs`) always writes the real `version` list (fall back to a
one-element list of the host feature instead of leaving the field null), and delete the name
arms for them in `native_override.rs` / `vm_exec.rs`. **Benefit.** Removes one class-name
allow-list pair. **Risk.** `JarFile.<clinit>` calls `Runtime.version().feature()`; a null
`version` there is an NPE at boot. **First step.** Make the mint total and assert it in a
unit test before the table row.

## SH14-5 -- Move the `BaisEvent` observation off `ByteArrayInputStream` (value: medium; cost: medium)

**What.** The census's rank-2 family (`ByteArrayInputStream.read()I`, `read([BII)I`,
`close()V`, 6 registrations) stays native only because those natives are where `native-io`
dispatches `BaisEvent` for the HTTP drain instant (`http_url_connection.rs`
`huc_live_bais_event`). Give the HTTP layer its own stream subclass (or observe at the
`HttpURLConnection` input-stream wrapper) and the three rows retire with the rest of the
family. **First step.** Enumerate who installs the hook (`install_bais_event_hook`) and which
streams it actually observes on the HTTPS fixture.
