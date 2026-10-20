# A hidden class's references to its own name bind to a same-named ordinary class loaded later

Status: FIXED (JIT round 21 wave 20, lane hidden-self-resolve), pending orchestrator build (unbuilt: no cargo,
javac or java were run in the lane, so the cause below comes from reading the code and has not been measured)
Switch: `CRATONVM_HIDDEN_SELF_CP_BY_ID` (correctness, default ON, `=0` restores the by-name answers). It is read
once per VM, on the first hidden self-reference asked, and cached in that VM's `HiddenClassBits`
(`classloading/src/class_manager.rs` `HiddenClassBits::self_cp_by_id`).
Found by: fuzz probe `tools/probes/jit/r21/fuzz/R21Fz5HiddenOsr1.java` (item 3 of `r21-fuzz5-probes-20261019.md`).
It fails in `--jdk-only`, `--compatible` and `--nojit`; HotSpot 25 runs it clean.

## Symptom

The probe first defines two hidden classes from the bytes of `R21Fz5HoPayload` with
`Lookup.defineHiddenClass`. `plainArm()` then loads the ordinary `R21Fz5HoPayload` from the class path. From that
point on, the hidden classes' calls to their own methods fail:

```
--jdk-only: java.lang.IllegalAccessError: class R21Fz5HoPayload/0x0000000000000002 tried to access private
            method 'int R21Fz5HoPayload.step(int)' (...) at R21Fz5HiddenOsr1$Hidden.runLoop(R21Fz5HiddenOsr1.java:76)
--nojit:    the same IllegalAccessError for 'int R21Fz5HoPayload.priv(int)', from
            R21Fz5HiddenOsr1.mega(R21Fz5HiddenOsr1.java:90) (the interface call into the hidden applyAsInt)
```

## The HotSpot rule

JVMS 5.4.3.1 and the `defineHiddenClass` specification: in a hidden class, `this_class` and every other
`CONSTANT_Class` entry that names the class's own class-file name resolve to the hidden class itself. They are never
resolved by name through the loader. An ordinary class of the same name, loaded later, must not capture them.

## Cause

CratonVM stores a hidden class as `<class-file name>/0x<hex>`, and that name is in no loader's namespace. The
constant pool still carries the class-file name. Before wave 20, three things handled this:

- the `CONSTANT_Class` door (`constants::hidden_self_reference`);
- the self-call doors (`is_self_class_reference` in `dispatch_static` and in the private `invokevirtual` pin);
- the wave-14 JIT routes (`CRATONVM_JIT_HIDDEN_SELF_BY_ID`).

The member-reference resolution core judges JVMS 5.4.4 access and selection against an owner. It found that owner
by NAME:

- `field_access::owner_in_referencing_namespace` compared names exactly, then fell back to
  `find_class_by_name_for_class`;
- `invoke::resolve_method_metadata_noting` looked up `owner_id` the same way.

While no ordinary class of that name existed, those lookups answered nothing, so the checks were skipped and the
call worked. Once `plainArm()` loaded the class-path copy, they answered that third class. `step` and `priv` were
then judged as the ORDINARY class's private methods, which belong to another nest, so the access was refused. The
`IllegalAccessError` comes from resolution, which is why `--nojit` fails too.

## Fix

The rule now lives in one place: `ClassManager::hidden_self_cp_answer` and `ClassManager::find_cp_class_for_class`
in `classloading/src/class_manager.rs`. The VM reaches it through `constants::hidden_self_cp_owner` and
`constants::hidden_self_cp_owner_in`. An ordinary referencing class pays one load of the per-VM lock-free
`HiddenClassBits` and nothing else; the switch is read only for a hidden class.

The paths that now bind to the hidden class's id:

| Path | Site |
|---|---|
| owner of every member check and selection (method, field, loader constraint, nest host, owner census) | `field_access.rs` `owner_in_referencing_namespace` |
| `resolve_method_metadata` owner (`owner_id`) | `invoke.rs` `resolve_method_metadata_noting` |
| threadless field peek (compile doors): it LOADED the class path's copy for `getstatic <ITSELF>` | `field_access.rs` `resolve_field_ref` |
| loader-aware field resolution: loader-local lookups are skipped for a self name | `field_access.rs` `resolve_field_ref_loader_aware` |
| `invokespecial <ITSELF>.m` (constructors, private methods), for any referencing loader | `invoke.rs` `hidden_self_special_owner` in `execute_invoke_kind` |
| invokespecial selection: the answer is spelled with the constant-pool name | `invoke.rs` `invokespecial_owner_class_name` |
| private / `Object` invokevirtual site predicates | `invoke.rs` `invokevirtual_site_targets_private`, `invokevirtual_site_resolves_to_object` |
| JVMS 5.4.6 selection input | `resolve/selection.rs` `resolved_ref_from_caller` |
| the callee an invoke at a pc names (frame census readers) | `gc_and_alloc.rs` `entry_invoked_at`, `synchronized_callee_invoked_at`; `interpreter.rs` `native_named_by_invoke` |
| JIT site owners: `jit_site_owner`, `jit_site_target_method`, `callee_has_exception_table_uncached`, the stash and resume class lookups, `site_native_owner_scope`, catch-type lookups for implicit and local handlers | `vm/src/jit/helpers.rs` |

`constants::hidden_self_reference` (the `CONSTANT_Class` door) now uses the per-VM bitmap as its prefilter. Before,
it used the process-wide `HIDDEN_CLASS_COUNTER`, which a single hidden class in any VM armed for every class of
every VM.

## Wave 21 follow-up (items 2-5 of `r21-review-w21-findings-20261019.md`)

- **What `=0` covers.** `CRATONVM_HIDDEN_SELF_CP_BY_ID=0` restores the by-name answer at every caller of
  `ClassManager::hidden_self_cp_answer` / `find_cp_class_for_class`: the table above and the three JVMTI
  invoke-at-pc readers in `jvmti_events.rs`. It does NOT reach the older self doors, which keep answering by the
  class's id:
  - the `CONSTANT_Class` door `constants::hidden_self_reference`;
  - the self-call doors `is_self_class_reference[_in]` (`dispatch_static`, the private-invokevirtual pin, the
    constructor-receiver match);
  - the unwinder's constructor-target test;
  - the `CRATONVM_JIT_HIDDEN_SELF_BY_ID` routes (`jit_hidden_self_owner`, `class_resolved_without_loading`).
- **One rule.** `cratonvm_classloading::hidden_self_name_rule`. Each hidden class's `this_class` is recorded at
  define time (`ClassManager::hidden_class_file_name`) and wins over the stored name's suffix. So a mint site's
  spelling no longer matters. `defineAnonymousClass` minted from the HOST's name, which captured references to the
  host; it now mints from the parsed `this_class`.
- **Cost.** The `invokespecial` probe now runs only in its own arm. An id past the bitmap (the lambda-proxy ids
  included) answers "known ordinary" through a lock-free latch until a hidden class is defined there.

## Tests

- `classloading/src/class_manager.rs`:
  - `hidden_self_cp_name_resolves_to_the_hidden_class_not_the_same_named_one`
  - `hidden_stored_name_is_class_file_name_takes_only_a_hex_suffix`
- `vm/src/runtime/interpreter/invoke.rs`, module `hidden_self_cp_tests` (a hand-built `hsr/P` with a private
  `priv()I`, defined hidden and then ordinary):
  - the owner and the private-site predicate, before and after the same-named load, with the switch on and off;
  - `resolve_method_metadata_as` for the hidden class's own private `Methodref` is not refused after the load, and
    the invokespecial owner is the constant-pool name;
  - `find_cp_class_for_class` against `find_class_by_name_for_class`.

## Verify

- `cargo test -p cratonvm-classloading hidden_self_cp` and `cargo test -p cratonvm-classloading hidden_stored_name`
- `cargo test -p cratonvm-vm hidden_self_cp_tests` (the vm crate's test target)
- Probe: `javac -d out tools/probes/jit/r21/fuzz/R21Fz5HiddenOsr1.java`, then run it on HotSpot 25,
  `cratonvm`, `cratonvm --compatible` and `cratonvm --nojit`. Every line must match. Run
  `CRATONVM_HIDDEN_SELF_CP_BY_ID=0` as a control: it is expected to show the `IllegalAccessError` again.

## Left open

- `jit_bridge.rs`: the by-name callee door's class hint (`try_jit_compile_callee`, `callee_probe_class_id`) still
  compares the hint by name, and this lane did not change it. Wave 14's `jit_callee_name_is_another_loaders_copy`
  already refuses a by-name body for a hidden self-reference, so no wrong body is served today. Binding the hint to
  the id is the remaining cleanup.
- `class_resolved_without_loading` still defers a hidden self-reference (wave 14, `CRATONVM_JIT_HIDDEN_SELF_BY_ID`).
  It does not answer with the id, so compiled sites keep reaching the run-time resolution.
- An array of the hidden class's own name (`[L<name>;`) still resolves its element by name. JVMS does not give
  arrays the self rule, and the probe has no such type.
