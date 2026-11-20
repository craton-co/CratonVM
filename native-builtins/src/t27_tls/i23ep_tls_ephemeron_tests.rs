use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xE23E_0001;
const VM_B: usize = 0xE23E_0002;
const VM_C: usize = 0xE23E_0003;
const VM_D: usize = 0xE23E_0004;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: a key / address only, never dereferenced (the mock hashes
    // the address).
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

fn sorted(v: &[ObjectRef]) -> Vec<usize> {
    let mut out: Vec<usize> = v.iter().map(|o| addr(*o)).collect();
    out.sort_unstable();
    out
}

/// Clears the test's VMs even when an assertion fails.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_tls_state(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

fn ctx_in(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

/// `SSLContext.init(km, tms, ..)`'s rows for `sslctx`: the owner note and
/// one `TrustManager` and one `KeyManager`. Answers the key.
fn init_context(
    c: &mut crate::test_utils::MockNativeContext,
    sslctx: ObjectRef,
    tm: ObjectRef,
    km: ObjectRef,
) -> u64 {
    let key = ctx_obj_key(c, sslctx).unwrap();
    note_ssl_context_owner(&*c, key, sslctx);
    ctx_trust_managers_table().lock().insert(key, vec![tm]);
    ctx_key_managers_table().lock().insert(key, vec![km]);
    key
}

fn scan(
    vm: usize,
    deferrable: &dyn Fn(usize) -> bool,
) -> (Vec<ObjectRef>, Vec<ObjectRef>, Vec<(usize, usize)>) {
    let (mut roots, mut rooted, mut pins) = (Vec::new(), Vec::new(), Vec::new());
    gc_scan_tls_ctx_roots_ephemeron(vm, &mut roots, &mut rooted, deferrable, &mut pins);
    pins.sort_unstable();
    (roots, rooted, pins)
}

#[test]
fn gci_i23ep_a_context_nothing_else_reads_pins_its_managers_to_itself() {
    let _teardown = Teardown(&[VM_A, VM_B]);
    let mut a = ctx_in(VM_A);
    let (sslctx, tm, km) = (at(0xE23E_1000), at(0xE23E_1100), at(0xE23E_1200));
    init_context(&mut a, sslctx, tm, km);

    let (roots, rooted, pins) = scan(VM_A, &|_| true);
    let mut want = vec![(addr(sslctx), addr(tm)), (addr(sslctx), addr(km))];
    want.sort_unstable();
    assert_eq!(pins, want, "each manager hangs off its context");
    assert!(rooted.is_empty(), "nothing rooted: {rooted:?}");
    assert!(!roots.contains(&tm) && !roots.contains(&km));
    assert!(!roots.contains(&sslctx), "the context stays weak");

    // A manager the predicate refuses is rooted.
    let refused = addr(km);
    let (_, rooted, pins) = scan(VM_A, &|x| x != refused);
    assert_eq!(rooted, vec![km]);
    assert_eq!(pins, vec![(addr(sslctx), addr(tm))]);

    // Another VM's scan reports none of it.
    let (roots_b, rooted_b, pins_b) = scan(VM_B, &|_| true);
    assert!(pins_b.is_empty() && rooted_b.is_empty());
    assert!(!roots_b.contains(&tm) && !roots_b.contains(&km));
}

/// A live engine made from the context reads its rows by key, so its
/// managers stay rooted (the engine is not the key's owner object); once
/// the context is found dead while the engine lives, the key is marked
/// dead and its managers stay rooted until the rows go.
#[test]
fn gci_i23ep_a_held_or_dead_marked_key_roots_its_managers() {
    let _teardown = Teardown(&[VM_C]);
    let mut c = ctx_in(VM_C);
    let (sslctx, tm, km) = (at(0xE23E_3000), at(0xE23E_3100), at(0xE23E_3200));
    let key = init_context(&mut c, sslctx, tm, km);
    let engine = at(0xE23E_4000);
    set_engine_trust_ctx_key(&mut c, engine, sslctx).unwrap();
    assert_eq!(ctx_key_holders().lock().holder_count(key), 1, "precondition");

    let (_, rooted, pins) = scan(VM_C, &|_| true);
    assert!(pins.is_empty(), "an engine holds the key: {pins:?}");
    assert_eq!(sorted(&rooted), sorted(&[tm, km]));

    // The context dies, the engine does not: rows kept, key marked dead.
    gc_sweep_tls_rows(VM_C, &|x| x == addr(engine));
    assert!(ctx_key_holders().lock().dead.contains_key(&key), "precondition");
    let (_, rooted, pins) = scan(VM_C, &|_| true);
    assert!(pins.is_empty());
    assert_eq!(sorted(&rooted), sorted(&[tm, km]));

    // Both die: the rows go in that sweep, and nothing is reported.
    gc_sweep_tls_rows(VM_C, &|_| false);
    let (_, rooted, pins) = scan(VM_C, &|_| true);
    assert!(pins.is_empty() && rooted.is_empty());
}

/// Refusing every deferral reports exactly what the two plain scans report.
#[test]
fn gci_i23ep_a_scan_that_defers_nothing_reports_the_plain_scans_roots() {
    let _teardown = Teardown(&[VM_D]);
    let mut d = ctx_in(VM_D);
    init_context(&mut d, at(0xE23E_5000), at(0xE23E_5100), at(0xE23E_5200));
    init_context(&mut d, at(0xE23E_6000), at(0xE23E_6100), at(0xE23E_6200));
    set_engine_trust_ctx_key(&mut d, at(0xE23E_7000), at(0xE23E_6000)).unwrap();

    let mut plain = Vec::new();
    gc_scan_tls_ctx_trust_manager_roots(VM_D, &mut plain);
    gc_scan_tls_ctx_key_manager_roots(VM_D, &mut plain);
    let (roots, rooted, pins) = scan(VM_D, &|_| false);
    assert!(pins.is_empty());
    let mut both = roots;
    both.extend(rooted);
    assert_eq!(sorted(&both), sorted(&plain));
}
