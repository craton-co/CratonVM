# `sun.misc.Unsafe.MEMORY_ACCESS_OPTION` repair still fails for real Keycloak/Infinispan integration — the "already_initialized" check likely has a field-index bug, causing a false-positive skip

Status: open — the fix in `docs/internal/fixed-suite-bugs/testsuite-model-unsafe-putorderedlong-memoryaccessoption-npe-FIXED.md`
(marked fixed 2026-07-12) does **not** actually resolve this in the real Keycloak/Infinispan test suite; its own
validation notes admit the suite-level check was skipped ("Azure host does not currently contain compiled
`testsuite/model` `UserModelTest` artifacts... so the final suite-level check was unavailable"). Reopening with
direct evidence from an actual Keycloak run.

Date observed: 2026-07-13 (second refresh rerun against non-passed-before classes, branch fix/keycloak-nonpassed-rerun-v2-20260710)

## Summary

36 `testsuite/model` classes still fail with the **exact same** `NullPointerException` this fix was supposed to
resolve:

```
=> java.lang.ExceptionInInitializerError
 Caused by: org.infinispan.commons.CacheConfigurationException: Unable to construct a GlobalComponentRegistry!
 Caused by: java.lang.RuntimeException: Failed to construct component io.netty.channel.EventLoopGroup, path io.netty.channel.EventLoopGroup
 Caused by: java.lang.IllegalStateException: failed to create a child event loop
 Caused by: java.lang.NullPointerException: Cannot invoke "sun.misc.Unsafe$MemoryAccessOption.ordinal()"
```

Crucially, the fix's own diagnostic log line confirms it ran and explicitly chose **not** to repair:

```
Post-clinit fixup: sun.misc.Unsafe MEMORY_ACCESS_OPTION policy=ALLOW repaired=false
```

(from this exact class's `.err.log`, moments before the same NPE fires downstream)

## Root cause hypothesis — a field-index computation bug in the repair's own "is it already initialized" check

`vm/src/vm/vm_util.rs` (~line 2265-2285):

```rust
let unsafe_option_slot = {
    let cm = shared.class_manager.read();
    cm.get_class(class_id).and_then(|unsafe_class| {
        let mut static_idx = 0usize;
        for field in &unsafe_class.fields {
            if field.is_static() {
                if &*field.name == "MEMORY_ACCESS_OPTION" {
                    return Some(static_idx);
                }
                static_idx += 1;
            }
        }
        None
    })
};
let already_initialized = unsafe_option_slot.is_some_and(|static_idx| {
    matches!(
        super::vm_object::get_static_shared(shared, class_id, static_idx),
        Value::Object(Some(_))
    )
});
// ...
let repaired = if already_initialized {
    false   // <-- skips repair entirely if this check is a false positive
} else if let Some((enum_class_id, static_idx)) = enum_slot {
    // ... actually performs the repair
};
```

The `already_initialized` check independently recomputes `MEMORY_ACCESS_OPTION`'s static-field index by manually
counting static fields in declaration order (`static_idx += 1` for each static field encountered). If this
manual count doesn't match the indexing convention `get_static_shared`/`set_static_by_name` actually use elsewhere
in the VM (e.g. if it should also count inherited/interface static fields, or if `unsafe_class.fields` orders
fields differently than however `class_id`'s static slot table was built), this check would read the **wrong
slot** — and if that wrong slot happens to already hold *some* non-null object (entirely unrelated to
`MEMORY_ACCESS_OPTION`), `already_initialized` reports `true`, `repaired` is forced to `false`, and the actual
`MEMORY_ACCESS_OPTION` field is left null, exactly reproducing the original bug — with the fix's own log line
(`repaired=false`) as the tell.

This is consistent with why the fix's isolated validation probes (`vm/src/vm/vm_util.rs`'s own comment thread and
the FIXED doc's "Reflection-based `Unsafe.putOrderedLong` probe" / "real Netty `NioEventLoopGroup(1)` probe")
passed — those probes likely trigger `sun/misc/Unsafe`'s `<clinit>` via a simpler, more direct path where the
field-index computation happens to line up correctly, while the full Keycloak/Infinispan bootstrap path
(reached through many more layers of classloading/JIT/whatever else runs first) exercises a `unsafe_class.fields`
ordering or a static-field layout where the computed index diverges from the real one.

## Next steps

1. Compare `unsafe_class.fields`' static-field enumeration (used by this repair check) against however
   `get_static_shared`/`set_static_by_name`'s actual slot-indexing scheme is built elsewhere (likely in
   `class_manager.rs` or wherever static field slots are assigned at class-loading time) — look for a mismatch in
   what counts as a "static field" for indexing purposes (e.g. synthetic fields, fields from a different
   classfile version, or field declaration order differences between however this class gets loaded in the
   isolated probe vs the real Keycloak run).
2. Add a debug assertion or log dumping what `already_initialized`'s read actually sees at that slot (the class
   name/type of whatever object is there) when it returns `true` — this would immediately confirm or refute the
   field-index-mismatch hypothesis by showing whether the "already non-null" object is genuinely a
   `MemoryAccessOption` enum constant or something else entirely.
3. Consider a more robust check: rather than trusting a manually-recomputed index, verify the *type* of whatever
   object occupies the slot (confirm it `instanceof sun.misc.Unsafe$MemoryAccessOption`) before treating it as
   "already initialized" — this would be robust to any indexing discrepancy.
4. Re-verify against all 36 currently-failing `testsuite/model` classes (not just isolated probes) before
   re-closing this doc.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-unsafe-memoryaccess-repair-gap -ClassList <(printf 'module\tclass\ntestsuite/model\torg.keycloak.testsuite.model.authz.ConcurrentAuthzTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-refresh2-20260712.exe -JdkHome $jdk
```
Check the `.err.log` for `Post-clinit fixup: sun.misc.Unsafe MEMORY_ACCESS_OPTION policy=... repaired=...` — expect
`repaired=false` followed shortly by the same `NullPointerException: ...MemoryAccessOption.ordinal()`.

## Evidence

36 classes across
`C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-before-refresh2-shard{1,2,3,4}\all-jit\logs\testsuite_model.*.{out,err}.log`,
2026-07-13 rerun with a binary built from `dev` post the 2026-07-12 fix merge. Fix source (with the suspected
bug): `vm/src/vm/vm_util.rs` lines ~2265-2285. Original (incomplete) fix doc:
`docs/internal/fixed-suite-bugs/testsuite-model-unsafe-putorderedlong-memoryaccessoption-npe-FIXED.md`.
