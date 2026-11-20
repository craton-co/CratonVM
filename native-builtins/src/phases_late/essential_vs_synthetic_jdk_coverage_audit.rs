use super::*;
use crate::register_essential_natives;
use std::collections::BTreeSet;

fn dump_triples(
    f: impl FnOnce(&mut NativeMethodRegistry),
) -> BTreeSet<(String, String, String)> {
    let mut r = NativeMethodRegistry::new();
    f(&mut r);
    r.dump_registrations()
        .into_iter()
        .map(|(c, m, d, _k)| (c.to_string(), m.to_string(), d.to_string()))
        .collect()
}

/// Turn a declared row list into the triple set it names, rejecting a
/// duplicated row (which would make the two-way ratchet below silently
/// tolerant of one deletion).
fn declared(label: &str, rows: &[Row]) -> BTreeSet<(String, String, String)> {
    let set: BTreeSet<(String, String, String)> = rows
        .iter()
        .map(|(c, m, d, _why)| (c.to_string(), m.to_string(), d.to_string()))
        .collect();
    assert_eq!(
        set.len(),
        rows.len(),
        "{label} lists a triple twice; a duplicated row makes the reverse \
         ratchet unable to see the first deletion"
    );
    set
}

/// `(class, method, descriptor, why this row is here)`. The fourth field is
/// load-bearing: it is what stops a row being added to shut the gate up.
type Row = (&'static str, &'static str, &'static str, &'static str);

/// Triples `register_p59_module` registers and `register_essential_natives`
/// does NOT — the population the original `difference` check watched.
///
/// Each was individually triaged against a real-HotSpot probe (see task
/// history / `reference_essential_vs_synthetic_jdk_registration_split`
/// memory). This is not a blanket allowlist; it is a closed list, and
/// `phase59_module_vs_essential_natives` fails in BOTH directions against
/// it.
const P59_ONLY: &[Row] = &[
    (
        "java/lang/Module",
        "isNamed",
        "()Z",
        "probe-verified to already match HotSpot through the real-bytecode \
         fallback: real `isNamed` reads the dual-written `name` field.",
    ),
    (
        "java/lang/Module",
        "toString",
        "()Ljava/lang/String;",
        "probe-verified: real `toString`'s \"module X\" format never touches \
         the null `descriptor`/`reads` fields.",
    ),
    (
        "java/lang/Module",
        "addReads",
        "(Ljava/lang/Module;)Ljava/lang/Module;",
        "the PUBLIC instance method, distinct from `addReads0` which the \
         essential path does register (lib.rs). Real `implAddReads` \
         delegates to `addReads0`, which updates the `ModuleRegistry`; the \
         `addReads` then `canRead` round trip was probe-verified against \
         HotSpot.",
    ),
];

/// Triples registered by BOTH registrars — the population the original
/// `difference` check was structurally unable to see.
///
/// A triple lands here when `register_p59_module` and
/// `register_essential_natives` each install a body for it. In
/// synthetic-jdk mode phase 59 runs LAST and therefore WINS
/// (`register_builtins` calls `register_essential_natives` and then
/// `register_synthetic_overrides`); in `--fast-boot` / `--real-jdk`
/// `register_p59_module` is never called at all, so the essential body is
/// the only one. That asymmetry is the hazard: a divergence between the two
/// bodies is a MODE-DEPENDENT behaviour change that no diff of the triples
/// can show, because the triple is identical on both sides.
///
/// Rows are marked:
///
/// - **SHARED FN** — both sides register the same `fn` item, so there are
///   no two bodies to diverge. Safe by construction, not by review.
/// - **TWIN** — two separately written bodies. Whoever changes one must
///   read the other, and say here what the difference is for.
///
/// Two TWIN rows below carry a KNOWN divergence, established by
/// `docs/feature-designs/native-dispatch-architecture.md`
/// §1.4 and §5, and NOT closed by this lane (both bodies live in
/// `phases_late/reflect_invoke.rs`, which this lane does not own).
const P59_AND_ESSENTIAL: &[Row] = &[
    (
        "java/lang/ModuleLayer",
        "boot",
        "()Ljava/lang/ModuleLayer;",
        "TWIN, KNOWN DIVERGENT. essential: `jboss_jdkspecific::native_module_layer_boot`, \
         memoised per VM (`ModuleLayer.boot() == ModuleLayer.boot()` is a \
         spec'd identity), allocating `MODULE_LAYER_FIELD_COUNT` = 2 slots \
         and populating `parents`/`nameToModule`/`modules` by NAME. p59: a \
         fresh, unmemoised layer per call, requesting 1 slot against a \
         declared synthetic width of 2 (class_manager.rs `instance_fields(2)`), \
         with none of those three fields. See E16 §5.3.",
    ),
    (
        "java/lang/ModuleLayer",
        "modules",
        "()Ljava/util/Set;",
        "TWIN, KNOWN DIVERGENT. essential: `jboss_jdkspecific::native_module_layer_modules`, \
         which derives and CACHES `layer.servicesCatalog` from `nameToModule` \
         as a side effect. `service_loader.rs:775` invokes this method purely \
         for that side effect and then reads the field by name. p59 ignores \
         its receiver and builds a fresh `HashSet` from the module registry, \
         so in synthetic-jdk mode (where p59 wins) that read finds nothing. \
         See E16 §1.4.",
    ),
    (
        "java/lang/ModuleLayer",
        "findModule",
        "(Ljava/lang/String;)Ljava/util/Optional;",
        "TWIN. essential: `jboss_jdkspecific::native_module_layer_find_module`, \
         which answers from the layer's `nameToModule`. p59 answers from \
         `all_module_names()` filtered by `module_is_class_path_only`. Both \
         now exclude a modular jar that reached the registry only through \
         `-cp`; that filter was added to p59 by lane E16 precisely because \
         this row exists.",
    ),
    (
        "java/lang/Module",
        "getName",
        "()Ljava/lang/String;",
        "TWIN. essential: `jboss_jdkspecific::native_module_get_name` (reads \
         the real `name` field by NAME, on a Module that is an instance of \
         the real class). p59 reads slot 0 of a 2-slot synthetic Module. The \
         two agree only because p59's Modules are the ones p59 itself \
         allocated.",
    ),
    (
        "java/lang/Module",
        "getLayer",
        "()Ljava/lang/ModuleLayer;",
        "TWIN. essential: `jboss_jdkspecific::native_module_get_layer`. See \
         the `MODULE_FIELD_COUNT` doc comment in jboss_jdkspecific.rs for \
         what a hand-picked slot index costs on this exact method: a raw \
         slot-1 write intended for `layer` landed on the REAL `name` field \
         of the shared unnamed-module mirror.",
    ),
    (
        "java/lang/Module",
        "getPackages",
        "()Ljava/util/Set;",
        "TWIN. essential: `jboss_jdkspecific::native_module_get_packages`, \
         backed by the off-object `module_packages_table` that \
         `defineModule0` seeds, falling back to `BOOT_JDK_PACKAGES`. p59 \
         answers from the VM's package index. Different sources, same \
         question.",
    ),
    (
        "java/lang/Module",
        "canRead",
        "(Ljava/lang/Module;)Z",
        "SHARED FN. lib.rs registers `crate::phases_late::native_module_can_read` \
         — the same fn item p59 registers. One body.",
    ),
    (
        "java/lang/Module",
        "addExports",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Module;",
        "SHARED FN. lib.rs registers `crate::phases_late::native_module_add_exports`. \
         One body.",
    ),
    (
        "java/lang/Module",
        "addOpens",
        "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Module;",
        "SHARED FN. lib.rs registers `crate::phases_late::native_module_add_opens`. \
         One body.",
    ),
    (
        "java/lang/Module",
        "getDescriptor",
        "()Ljava/lang/module/ModuleDescriptor;",
        "TWIN. Was P59-ONLY when this guard was written, with the note that \
         the essential-side fix lived on an unmerged branch. That branch \
         merged: lib.rs now registers a `getDescriptor` that prefers the \
         object's real `descriptor` field and falls back to building one \
         from the module name. p59 allocates a 2-slot `ModuleDescriptor` \
         and copies slot 0. The stale P59-ONLY row is deleted, which is the \
         reverse direction of this ratchet doing its job.",
    ),
    (
        "java/lang/Module",
        "isExported",
        "(Ljava/lang/String;)Z",
        "TWIN. essential (lib.rs) resolves the receiver with \
         `module_name_of_mirror`; p59 uses `read_module_name`. Both then ask \
         the VM's export graph.",
    ),
    (
        "java/lang/Module",
        "isExported",
        "(Ljava/lang/String;Ljava/lang/Module;)Z",
        "TWIN. Qualified form of the row above; same receiver-resolution \
         difference.",
    ),
    (
        "java/lang/Module",
        "isOpen",
        "(Ljava/lang/String;)Z",
        "TWIN. Same shape as `isExported(String)`, against the opens graph.",
    ),
    (
        "java/lang/Module",
        "isOpen",
        "(Ljava/lang/String;Ljava/lang/Module;)Z",
        "TWIN. Qualified form of the row above.",
    ),
    (
        "java/lang/module/ModuleDescriptor",
        "name",
        "()Ljava/lang/String;",
        "TWIN. essential: `reflect_annotations::register_module_builder_overrides`, \
         reached from `register_essential_natives_with_shims`, on a \
         16-slot descriptor written by NAME. p59 reads slot 0 of the 2-slot \
         descriptor its own `getDescriptor` allocates.",
    ),
    (
        "java/lang/module/ModuleDescriptor",
        "isAutomatic",
        "()Z",
        "TWIN. Was P59-ONLY when this guard was written (\"resolves once the \
         getDescriptor branch merges\"). It merged, via \
         `register_module_builder_overrides`. Stale row deleted.",
    ),
    (
        "java/lang/module/ModuleDescriptor",
        "isOpen",
        "()Z",
        "TWIN. Same history as `isAutomatic` above.",
    ),
    (
        "java/lang/Class",
        "getModule",
        "()Ljava/lang/Module;",
        "TWIN. essential (lib.rs) caches ONE canonical Module per module \
         name, because the JDK compares Modules by identity and a fresh \
         instance per call broke \
         `Throwable.validateSuppressedExceptionsList`. p59 has its own \
         canonical-per-name cache. Two caches for one identity invariant.",
    ),
];

/// Partitions `register_p59_module`'s triples against
/// `register_essential_natives` and ratchets BOTH halves, in BOTH
/// directions, against the two lists above.
///
/// **What makes this fail.** Any of:
///
/// 1. a triple `register_p59_module` registers that `register_essential_natives`
///    does not, and that `P59_ONLY` does not name — the original check, the
///    "silently missing from the build that matters" shape that broke
///    `Module.getDescriptor()`;
/// 2. a triple registered by BOTH and not named in `P59_AND_ESSENTIAL` —
///    the shape the original check was structurally unable to see, because
///    `p59.difference(&essential)` is empty for exactly the triples where
///    two bodies compete and the mode decides which one runs;
/// 3. a row in either list that no longer describes reality — a triple that
///    stopped being p59-only because the essential path picked it up, or
///    stopped being double-registered because one side dropped it. Three
///    such rows were found stale when this ratchet was added
///    (`Module.getDescriptor`, `ModuleDescriptor.isAutomatic`,
///    `ModuleDescriptor.isOpen`), all of which the one-way `difference`
///    check had been quietly carrying as permission;
/// 4. the same triple named on both lists, or twice on one list.
///
/// It does NOT compare BODIES. Nothing here can tell you that p59's
/// `ModuleLayer.modules()` dropped the `servicesCatalog` side effect the
/// essential twin has — only that both register the method, and that
/// somebody had to write down which one wins where. That is the whole
/// claim: an unnamed intersection triple is now loud, and a name left
/// behind after the intersection changes is also loud.
#[test]
fn phase59_module_vs_essential_natives() {
    let p59 = dump_triples(|r| register_p59_module(r));
    let essential = dump_triples(|r| register_essential_natives(r));

    let declared_only = declared("P59_ONLY", P59_ONLY);
    let declared_both = declared("P59_AND_ESSENTIAL", P59_AND_ESSENTIAL);
    let on_both_lists: Vec<_> = declared_only.intersection(&declared_both).collect();
    assert!(
        on_both_lists.is_empty(),
        "a triple cannot be both p59-only and double-registered; \
         listed twice: {on_both_lists:?}"
    );

    let actual_only: BTreeSet<(String, String, String)> =
        p59.difference(&essential).cloned().collect();
    let actual_both: BTreeSet<(String, String, String)> =
        p59.intersection(&essential).cloned().collect();

    fn audit(
        problems: &mut Vec<String>,
        label: &str,
        actual: &BTreeSet<(String, String, String)>,
        listed: &BTreeSet<(String, String, String)>,
        undeclared_hint: &str,
        stale_hint: &str,
    ) {
        for t in actual.difference(listed) {
            problems.push(format!(
                "UNDECLARED in {label}: {}.{}{} — {undeclared_hint}",
                t.0, t.1, t.2
            ));
        }
        for t in listed.difference(actual) {
            problems.push(format!(
                "STALE row in {label}: {}.{}{} — {stale_hint}",
                t.0, t.1, t.2
            ));
        }
    }

    let mut problems: Vec<String> = Vec::new();
    audit(
        &mut problems,
        "P59_ONLY",
        &actual_only,
        &declared_only,
        "register_p59_module registers it and register_essential_natives does \
         not, so it is missing from the build the corpora run. Triage against \
         a real-HotSpot probe, then either register it on the essential path \
         or add a P59_ONLY row saying why that is correct.",
        "it is no longer p59-only. If the essential path picked it up, MOVE \
         the row to P59_AND_ESSENTIAL and say which body wins in which mode; \
         if p59 dropped it, DELETE the row. Leaving it here is how a triaged \
         list turns into permission.",
    );
    audit(
        &mut problems,
        "P59_AND_ESSENTIAL",
        &actual_both,
        &declared_both,
        "BOTH registrars install a body for this triple. In synthetic-jdk mode \
         phase 59 runs last and WINS; in --fast-boot/--real-jdk phase 59 is \
         never called and the essential body is the only one. Read both \
         bodies, then add a row saying whether they are the same fn item \
         (SHARED FN) or two implementations (TWIN) and what the difference \
         is for. An unread intersection is how ModuleLayer.modules() lost \
         its servicesCatalog side effect in one mode only.",
        "it is no longer double-registered. Find out which side dropped it: \
         if the essential path did, this is now a p59-only triple and the \
         row belongs in P59_ONLY; if p59 did, DELETE the row.",
    );

    assert!(
        problems.is_empty(),
        "\n{} problem(s) in the phase-59 / essential module-native \
         partition:\n  {}\n",
        problems.len(),
        problems.join("\n  ")
    );
}
