use super::tests_redefinition_count;
use super::{
    CachedBytecodeMethod, CachedInvokeTarget, ClassId, InvokeCache, RedefineGate,
    INVOKE_CACHE_RETIRES_ONLY_REDEFINED_CALLERS,
};
use std::sync::Arc;

fn target(declaring: u32) -> CachedInvokeTarget<()> {
    CachedInvokeTarget::Bytecode {
        cached: Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_reader::CachedMethodParts {
                declaring_class_id: ClassId::new(declaring),
                class_name: Arc::from("p/Helper"),
                method_name: Arc::from("pick"),
                method_descriptor: Arc::from("()I"),
                source_file: None,
                code: Arc::from(&[0x04u8, 0xACu8][..]), // iconst_1; ireturn
                exception_table: Arc::from(&[][..]),
                max_stack: 1,
                max_locals: 0,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        )),
        gate: RedefineGate::never_stale(),
    }
}

/// A redefinition of `C` (three counts, as `redefine_class` and the VM
/// record it) drops `(C, ..)` entries in both maps and keeps `(D, ..)`.
#[test]
fn a_redefinition_of_one_class_keeps_the_other_callers_entries() {
    assert!(INVOKE_CACHE_RETIRES_ONLY_REDEFINED_CALLERS);
    let mut cache = InvokeCache::<()>::new();
    let (c, d, receiver) = (
        ClassId::new(20_201),
        ClassId::new(20_202),
        ClassId::new(20_203),
    );
    cache.put(c, 5, false, 0, target(20_204));
    cache.put(d, 5, false, 0, target(20_204));
    cache.put_poly(c, 6, false, 0, receiver, target(20_204));
    cache.put_poly(d, 6, false, 0, receiver, target(20_204));

    for _ in 0..3 {
        tests_redefinition_count::bump_of(c);
    }
    assert!(cache.get(c, 5, false, 0).is_none(), "the redefined caller");
    assert!(cache.get_poly(c, 6, false, 0, receiver).is_none());
    assert!(cache.get(d, 5, false, 0).is_some(), "an unredefined caller");
    assert!(cache.get_poly(d, 6, false, 0, receiver).is_some());
    assert_eq!(cache.redefinitions_seen(), tests_redefinition_count::get());

    // The redefined caller refills and is served.
    cache.put(c, 5, false, 0, target(20_205));
    assert!(cache.get(c, 5, false, 0).is_some());
}

/// A compiled (`Jit`) entry is dropped whatever its caller: its body may
/// have inlined the redefined class.
#[test]
fn a_class_scoped_retirement_drops_every_compiled_entry() {
    let mut cache = InvokeCache::<()>::new();
    let (c, d) = (ClassId::new(20_241), ClassId::new(20_242));
    let compiled = CachedInvokeTarget::Jit {
        compiled: (),
        num_params: 0,
        return_type: b'I',
        needs_heap: false,
        cached: match target(20_243) {
            CachedInvokeTarget::Bytecode { cached, .. } => cached,
            _ => return,
        },
        gate: RedefineGate::never_stale(),
        supersede_epoch: super::SupersedeGate::never_stale(),
    };
    cache.put(d, 5, false, 0, compiled);
    cache.put(d, 6, false, 0, target(20_243));
    assert!(cache.get(d, 5, false, 0).is_some());
    tests_redefinition_count::bump_of(c);
    assert!(cache.get(d, 5, false, 0).is_none(), "compiled: dropped");
    assert!(cache.get(d, 6, false, 0).is_some(), "bytecode: kept");
}

/// A reader the ring has lapped cannot know which classes moved: it
/// retires everything.
#[test]
fn a_reader_behind_the_ring_retires_every_entry() {
    let mut cache = InvokeCache::<()>::new();
    let (c, d) = (ClassId::new(20_211), ClassId::new(20_212));
    cache.put(d, 5, false, 0, target(20_213));
    assert!(cache.get(d, 5, false, 0).is_some());
    tests_redefinition_count::bump_of(c);
    tests_redefinition_count::bump_of(c);
    tests_redefinition_count::lap(1);
    assert!(cache.get(d, 5, false, 0).is_none());
}

/// A bump that names no class retires everything, and a class-scoped
/// one in the same span does not rescue the other callers.
#[test]
fn an_unattributed_redefinition_retires_every_entry() {
    let mut cache = InvokeCache::<()>::new();
    let (c, d) = (ClassId::new(20_221), ClassId::new(20_222));
    cache.put(d, 5, false, 0, target(20_223));
    tests_redefinition_count::bump_of(c);
    tests_redefinition_count::bump();
    assert!(cache.get(d, 5, false, 0).is_none());
}

/// The fill admission is unchanged: a fill that straddles a class-scoped
/// redefinition of ANOTHER class is still dropped (its snapshot is old).
#[test]
fn a_fill_straddling_an_unrelated_redefinition_is_still_dropped() {
    let mut cache = InvokeCache::<()>::new();
    let (c, d) = (ClassId::new(20_231), ClassId::new(20_232));
    assert!(cache.get(d, 5, false, 0).is_none());
    let as_of = cache.redefinitions_seen();
    tests_redefinition_count::bump_of(c);
    cache.put_as_of(d, 5, false, 0, as_of, target(20_233));
    assert!(cache.get(d, 5, false, 0).is_none());
}
