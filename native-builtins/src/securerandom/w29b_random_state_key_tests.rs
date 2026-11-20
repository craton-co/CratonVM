use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeHeapAccess as _, NativeThreadAccess as _};

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; the natives under test hash the address and
    // never dereference it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

/// Forgets its VMs' lock keys (and so, through the registry's hook, their
/// generator rows) even when an assertion fails.
struct ForgetVms(&'static [usize]);
impl Drop for ForgetVms {
    fn drop(&mut self) {
        for &vm in self.0 {
            crate::forget_vm_lock_keys(vm);
        }
    }
}

/// `new Random(seed)`'s first `n` `nextLong()`s, computed from the spec.
fn reference_longs(seed: i64, n: usize) -> Vec<i64> {
    let mut s = scramble_seed(seed);
    let mut next = |bits: u32| -> i64 {
        s = s.wrapping_mul(LCG_MULTIPLIER).wrapping_add(LCG_INCREMENT) & LCG_MASK;
        ((s >> (48 - bits)) as i32) as i64
    };
    (0..n)
        .map(|_| {
            let hi = next(32);
            let lo = next(32);
            (hi << 32).wrapping_add(lo)
        })
        .collect()
}

fn seed(c: &mut crate::test_utils::MockNativeContext, r: ObjectRef, s: i64) {
    native_random_init_seed(c, &[Value::Object(Some(r)), Value::Long(s)]).unwrap();
}

fn next_long(c: &mut crate::test_utils::MockNativeContext, r: ObjectRef) -> i64 {
    match native_random_next_long(c, &[Value::Object(Some(r))]) {
        Ok(Some(Value::Long(v))) => v,
        other => panic!("nextLong answered {other:?}"),
    }
}

fn seed_row(key: usize) -> Option<u64> {
    SEED_TABLE
        .read()
        .as_ref()
        .and_then(|t| t.get(&key).copied())
}

/// The census page's scenario: two live `Random`s of ONE VM with one
/// identity hash each replay their own seed's stream, interleaved. Before
/// w29-b they shared one row: the second `<init>` reseeded the first, and
/// every draw advanced both.
#[test]
fn two_live_randoms_sharing_an_identity_hash_keep_their_own_streams() {
    const VM: usize = 0xB29_0001;
    let _g = ForgetVms(&[VM]);
    let mut c = ctx_for(VM);
    let (a, b) = (obj(0x1_B291_0000), obj(0x2_B291_0000));
    assert_eq!(
        c.identity_hash_code(a),
        c.identity_hash_code(b),
        "premise: the two instances share an identity hash"
    );
    seed(&mut c, a, 42);
    seed(&mut c, b, 7);
    let (want_a, want_b) = (reference_longs(42, 3), reference_longs(7, 3));
    for i in 0..3 {
        assert_eq!(next_long(&mut c, a), want_a[i], "a's draw {i}");
        assert_eq!(next_long(&mut c, b), want_b[i], "b's draw {i}");
    }
}

/// The same instance address in two VMs (the cross-VM half: the tables
/// had no VM in the key at all).
#[test]
fn two_vms_randoms_sharing_an_identity_hash_keep_their_own_streams() {
    const VM_A: usize = 0xB29_0011;
    const VM_B: usize = 0xB29_0012;
    let _g = ForgetVms(&[VM_A, VM_B]);
    let (mut a, mut b) = (ctx_for(VM_A), ctx_for(VM_B));
    let r = obj(0x1_B292_0000);
    seed(&mut a, r, 1);
    seed(&mut b, r, 2);
    assert_eq!(next_long(&mut a, r), reference_longs(1, 1)[0]);
    assert_eq!(next_long(&mut b, r), reference_longs(2, 1)[0]);
}

/// A dead `Random`'s seed and cached Gaussian go with its lock key -- the
/// plain lock-key sweep every backend's collection runs, no ZGC and no
/// eviction channel involved -- and a live collider's rows stay.
#[test]
fn a_dead_randoms_rows_go_with_its_key_and_a_live_colliders_stay() {
    const VM: usize = 0xB29_0021;
    let _g = ForgetVms(&[VM]);
    let mut c = ctx_for(VM);
    let (dead, live) = (0x1_B293_0000usize, 0x2_B293_0000usize);
    seed(&mut c, obj(dead), 3);
    seed(&mut c, obj(live), 4);
    // Leaves a cached partner deviate in GAUSSIAN_TABLE.
    native_random_next_gaussian(&mut c, &[Value::Object(Some(obj(dead)))]).unwrap();
    let kd = crate::existing_weak_lock_key(&c, obj(dead)).expect("dead's key");
    let kl = crate::existing_weak_lock_key(&c, obj(live)).expect("live's key");
    assert!(seed_row(kd).is_some());
    assert!(GAUSSIAN_TABLE
        .read()
        .as_ref()
        .is_some_and(|t| t.contains_key(&kd)));

    assert_eq!(crate::gc_sweep_lock_keys(VM, &|x| x == live), 1);
    assert_eq!(seed_row(kd), None, "the dead Random's seed row survived");
    assert!(
        !GAUSSIAN_TABLE
            .read()
            .as_ref()
            .is_some_and(|t| t.contains_key(&kd)),
        "the dead Random's cached Gaussian survived"
    );
    assert!(seed_row(kl).is_some(), "the live collider's seed row went");
    // VM teardown takes the survivor's rows too.
    crate::forget_vm_lock_keys(VM);
    assert_eq!(seed_row(kl), None);
}

/// A sweep of VM A's keys never touches VM B's rows (the ZGC channel's
/// cross-VM eviction).
#[test]
fn one_vms_sweep_keeps_another_vms_rows() {
    const VM_A: usize = 0xB29_0031;
    const VM_B: usize = 0xB29_0032;
    let _g = ForgetVms(&[VM_A, VM_B]);
    let (mut a, mut b) = (ctx_for(VM_A), ctx_for(VM_B));
    let r = obj(0x1_B294_0000);
    seed(&mut a, r, 5);
    seed(&mut b, r, 6);
    let kb = crate::existing_weak_lock_key(&b, r).expect("B's key");
    crate::gc_sweep_lock_keys(VM_A, &|_| false);
    assert!(
        seed_row(kb).is_some(),
        "VM A's collection dropped VM B's row"
    );
    assert_eq!(next_long(&mut b, r), reference_longs(6, 1)[0]);
}

/// Round 13 wave 9 (lane shadow3): a `Random` SUBCLASS receiver stands
/// down to `Random`'s bytecode only when that bytecode exists. The mock,
/// like the synthetic JDK, has a `java/util/Random` with no `next(I)I`
/// body, so a subclass instance keeps the side-table natives and replays
/// the seeded stream (the positive arm needs a real class file: probe
/// `R13Shadow3RandomSubclassYield`).
#[test]
fn a_subclass_of_a_codeless_random_keeps_the_natives() {
    use cratonvm_native_api::NativeClassAccess as _;
    const VM: usize = 0xB29_0051;
    let _g = ForgetVms(&[VM]);
    let mut c = ctx_for(VM);
    let random = c.ensure_class_initialized(RANDOM_CLASS).unwrap();
    let sub = c.ensure_class_initialized("test/r13w9/SubRandom").unwrap();
    c.set_superclass(sub, random);
    let exact = c.alloc_object(random, 2);
    let r = c.alloc_object(sub, 2);
    assert!(
        !random_receiver_runs_bytecode(&c, exact),
        "an exact Random never yields"
    );
    assert!(!random_receiver_runs_bytecode(&c, r), "no body to yield to");
    seed(&mut c, r, 42);
    assert_eq!(next_long(&mut c, r), reference_longs(42, 1)[0]);
}

/// Two seeded SHA1PRNG instances with one hash, seeded alike, each replay
/// the full stream: a shared row would hand the second one the first's
/// continuation. An unseeded instance with that hash has no key and falls
/// through to the OS CSPRNG.
#[test]
fn sha1prng_colliders_each_replay_their_own_stream() {
    const VM: usize = 0xB29_0041;
    let _g = ForgetVms(&[VM]);
    let mut c = ctx_for(VM);
    let (a, b, unseeded) = (obj(0x1_B295_0000), obj(0x2_B295_0000), obj(0x3_B295_0000));
    sha1prng_set_seed_bytes(&mut c, a, b"w29b");
    sha1prng_set_seed_bytes(&mut c, b, b"w29b");
    let ka = sha1prng_key(&c, a).expect("a is seeded");
    let kb = sha1prng_key(&c, b).expect("b is seeded");
    assert_ne!(ka, kb);
    let (mut from_a, mut from_b) = ([0u8; 24], [0u8; 24]);
    assert!(sha1prng_next_bytes(ka, &mut from_a));
    assert!(sha1prng_next_bytes(kb, &mut from_b));
    assert_eq!(from_a, from_b, "b's stream continued a's");
    assert_eq!(sha1prng_key(&c, unseeded), None);
    // `b` dies, `a` lives.
    assert_eq!(
        crate::gc_sweep_lock_keys(VM, &|x| x == a.as_ptr() as usize),
        1
    );
    assert!(
        !SHA1PRNG_TABLE
            .read()
            .as_ref()
            .is_some_and(|t| t.contains_key(&kb)),
        "the dead instance's SHA1PRNG state survived"
    );
    assert!(
        sha1prng_next_bytes(ka, &mut from_a),
        "the live collider lost its state"
    );
}
