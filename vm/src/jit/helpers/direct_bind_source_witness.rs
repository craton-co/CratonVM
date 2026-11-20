use super::DirectNativeShadow;

/// Working-tree path of a file under the `vm` crate.
///
/// `CARGO_MANIFEST_DIR` is `<repo>/vm`, so these read the SOURCE, not a
/// build artifact — `source-witness-tests-read-the-working-tree`. An edit
/// that has not been rebuilt still fails this test, which is the point.
pub fn vm_src(rel: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

pub fn read(rel: &str) -> String {
    let path = vm_src(rel);
    if path.is_file() {
        return std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    }
    let dir = path.with_extension("");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.file_name().and_then(|name| name.to_str())
            .is_some_and(|name| name == "mod.rs" || (name.starts_with("part_") && name.ends_with(".rs"))))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no source files in {}", dir.display());
    files.iter().map(|path| std::fs::read_to_string(path).unwrap()).collect()
}

/// Count code (non-comment) occurrences of `crate::jit::helpers::<sym>` for
/// any enumerated shadow.
///
/// Comment lines are excluded by their trimmed prefix rather than by a
/// parser: both doors reference these symbols in prose right beside the
/// code, and a count that included prose would move whenever someone
/// improved a comment. CR is trimmed too, so a CRLF checkout does not
/// change the answer — `windows-crlf-breaks-source-witness-tests`.
pub fn raw_address_takes(rel: &str) -> Vec<(usize, String)> {
    let text = read(rel);
    let mut hits = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r').trim_start();
        if line.starts_with("//") {
            continue;
        }
        for shadow in DirectNativeShadow::ALL {
            let needle = format!("crate::jit::helpers::{}", shadow.helper_symbol());
            if line.contains(&needle) {
                hits.push((i + 1, shadow.helper_symbol().to_string()));
            }
        }
    }
    hits
}

/// Neither direct door names a helper item any more. `H12-1` O1/O2, landed
/// 2026-09-22.
///
/// The baseline was **7 and 2** — seven raw address-takes in the OSR
/// ladder, five of them `bridge` rows (`H12-2` 1), and two in the eager
/// first-call ladder. Its own doc said what to do when the number fell:
/// *"If it goes DOWN, O1/O2 are landing and the baseline should be lowered
/// in the same commit; at zero, delete this module and make the nine items
/// `pub(in crate::jit)`, which is strictly better than any test."*
///
/// Both halves of that happened, and the module is kept rather than
/// deleted for the half that is NOT covered by the visibility change: the
/// thirty-one items are `pub(in crate::jit)` now, so a ladder outside this
/// module cannot name one and the compiler says so — but nothing stops a
/// future change widening one back to `pub` on its way to adding a
/// thirty-second door. This test and
/// [`every_enumerated_helper_is_private_to_the_jit_module`] are the two
/// halves of that: one says the doors do not name them, the other says
/// they could not.
#[test]
pub fn neither_direct_door_takes_a_raw_helper_address_any_more() {
    let osr = raw_address_takes("src/runtime/interpreter/jit_bridge.rs");
    let eager = raw_address_takes("src/runtime/interpreter.rs");
    assert!(
        osr.is_empty(),
        "the OSR ladder names a helper item directly: {osr:?}. Every bind \
         at that door goes through `admit_direct_native_entry`, which is \
         the only thing that asks the policy question. Was 7 before \
         `H12-1` O1."
    );
    assert!(
        eager.is_empty(),
        "the eager first-call ladder names a helper item directly: \
         {eager:?}. Was 2 before `H12-1` O2; both of its binds are \
         `intrinsic` and therefore correct, which is exactly why an \
         unguarded ladder there was a defect waiting for its third bind."
    );
}

/// Every enumerated helper is `pub(in crate::jit)`.
///
/// This is `H12-1` N1's ask as it actually landed. N1 wanted `JitDirectCall`
/// made un-constructible without a policy witness; the H20-1 module block
/// above explains why the token has to ride on the ADDRESS instead (a
/// direct-call row filtered downstream leaves the pc with no `invoke_info`,
/// and the backend compiles the site as a call to the enclosing method). The
/// address is handed out by `admit_direct_native_entry` alone, and this is
/// what makes "alone" true rather than aspirational.
///
/// A source witness and not a compile check because the compile check is
/// the *consequence*: if someone widens one of these back to `pub`, nothing
/// fails to build until a door actually misuses it, and by then the
/// widening is old.
#[test]
pub fn every_enumerated_helper_is_private_to_the_jit_module() {
    // `vm/src/jit/helpers.rs` is split into `vm/src/jit/helpers/*.rs`
    // (origin/dev `00529e979f`); "this file" is their concatenation.
    let text = crate::jit::helpers::read_helpers_src_from_disk();
    for shadow in DirectNativeShadow::ALL {
        let sym = shadow.helper_symbol();
        let decl = format!("pub(in crate::jit) unsafe extern \"C\" fn {sym}(");
        assert!(
            text.contains(&decl),
            "{shadow:?} names `{sym}`, which is not declared \
             `pub(in crate::jit) unsafe extern \"C\" fn`. Widening one of \
             these back to `pub` puts a raw address back within reach of a \
             compile door, which is the `H12-1` defect: a ladder that binds \
             a helper without asking whether the callee would serve it."
        );
    }
}

/// Every enumerated shadow has a distinct symbol, triple and address.
///
/// A duplicated address would mean two variants naming one helper, which
/// would make a refusal for one silently refuse the other;
/// `duplicate-native-registrations-verify-which-wins` is the adjacent trap.
/// A duplicated TRIPLE would be the H7-1 shape: two helpers asking about
/// one registry row, one of which is not the row its call actually reaches.
#[test]
pub fn the_enumeration_is_injective() {
    let mut symbols: Vec<&str> = DirectNativeShadow::ALL
        .iter()
        .map(|s| s.helper_symbol())
        .collect();
    symbols.sort_unstable();
    symbols.dedup();
    assert_eq!(symbols.len(), DirectNativeShadow::ALL.len());

    let mut triples: Vec<(&str, &str, &str)> =
        DirectNativeShadow::ALL.iter().map(|s| s.triple()).collect();
    triples.sort_unstable();
    triples.dedup();
    assert_eq!(triples.len(), DirectNativeShadow::ALL.len());

    let mut addrs: Vec<usize> = DirectNativeShadow::ALL
        .iter()
        .map(|s| s.entry_addr())
        .collect();
    assert!(
        addrs.iter().all(|&a| a != 0),
        "a shadow's address is never 0; 0 is the ladders' \"not wired\" sentinel"
    );
    addrs.sort_unstable();
    addrs.dedup();
    assert_eq!(
        addrs.len(),
        DirectNativeShadow::ALL.len(),
        "two variants resolve to one helper address"
    );
}

/// The enumerated symbol names are the names the ladders actually write.
///
/// `helper_symbol` is a string, so it can drift from the item it names.
/// This pins it to the working tree: every symbol must be declared here.
#[test]
pub fn every_enumerated_symbol_is_a_real_item_in_this_file() {
    // `vm/src/jit/helpers.rs` is split into `vm/src/jit/helpers/*.rs`
    // (origin/dev `00529e979f`); "this file" is their concatenation.
    let text = crate::jit::helpers::read_helpers_src_from_disk();
    for shadow in DirectNativeShadow::ALL {
        let decl = format!("extern \"C\" fn {}(", shadow.helper_symbol());
        assert!(
            text.contains(&decl),
            "{:?} names `{}`, which is not declared in this file",
            shadow,
            shadow.helper_symbol()
        );
    }
}

/// `DirectNativeShadow::for_scoped_memory_op` agrees with the jit crate's
/// own recognition of the same methods.
///
/// Two enums in two crates describe one population — the jit crate cannot
/// name a VM symbol, which is the whole reason both exist — so the risk is
/// that two hand-written orderings drift and a `putLong` site binds
/// `putInt`'s policy answer. Round-tripping through `is_scoped_memory_op`
/// closes that: the shadow's own triple, fed back to the recogniser, must
/// produce the op the map was asked about.
#[test]
pub fn every_scoped_memory_op_maps_to_the_shadow_that_spells_its_triple() {
    let mut seen = 0usize;
    for shadow in DirectNativeShadow::ALL {
        let (class, method, descriptor) = shadow.triple();
        if class != "jdk/internal/misc/ScopedMemoryAccess" {
            continue;
        }
        seen += 1;
        let op = cratonvm_jit::is_scoped_memory_op(class, method, descriptor)
            .unwrap_or_else(|| panic!("{shadow:?} spells a triple `is_scoped_memory_op` does not recognise: {class}.{method}{descriptor}"));
        assert_eq!(
            DirectNativeShadow::for_scoped_memory_op(op),
            *shadow,
            "`for_scoped_memory_op({op:?})` does not round-trip to the \
             shadow whose triple spells that op"
        );
    }
    assert_eq!(
        seen, 19,
        "the `ScopedMemoryAccess` family is eight unaligned accessors and \
         their eight `*Internal` twins, the two byte internals, and copyMemoryInternal; found {seen}"
    );
}
