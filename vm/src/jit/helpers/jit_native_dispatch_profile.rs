use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

pub const PASSES: usize = 4;
pub const ROUNDS: u32 = 400_000;

pub fn noop_native(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[Value],
) -> crate::error::MethodCallResult {
    Ok(None)
}

pub fn rung(label: &str, mut f: impl FnMut()) {
    print!("{label:<56}");
    for _ in 0..PASSES {
        let t0 = Instant::now();
        for _ in 0..ROUNDS {
            f();
        }
        let ns = t0.elapsed().as_nanos() as f64 / f64::from(ROUNDS);
        print!("{ns:>10.1}");
    }
    println!();
}

/// An `AtomicInteger.get()`-shaped site: instance receiver, no arguments.
pub static GET_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/concurrent/atomic/AtomicInteger",
    method_name: "get",
    descriptor: "()I",
    num_jit_args: 1,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// A `setExclusiveOwnerThread(Thread)`-shaped site: receiver + one object.
pub static SET_OWNER_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/concurrent/locks/AbstractOwnableSynchronizer",
    method_name: "setExclusiveOwnerThread",
    descriptor: "(Ljava/lang/Thread;)V",
    num_jit_args: 2,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// A `System.nanoTime()`-shaped site: static, no arguments.
pub static NANO_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/System",
    method_name: "nanoTime",
    descriptor: "()J",
    num_jit_args: 0,
    return_type: b'J',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

#[test]
#[ignore = "measurement, not an assertion — see the module doc"]
pub fn compiled_native_call_step_breakdown() {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "jit-dispatch-profile");
    let recv = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let other = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let recv_raw = recv.as_ptr() as i64;
    let other_raw = other.as_ptr() as i64;
    let one_arg: [i64; 1] = [recv_raw];
    let two_args: [i64; 2] = [recv_raw, other_raw];
    let no_args: [i64; 0] = [];
    let cb: cratonvm_native_api::NativeCallback = noop_native;
    let key = jit_site_key(shared.vm_identity, &GET_INFO as *const _ as usize);

    // Seed the site cache so the probe below measures a HIT, which is the
    // steady state every one of these numbers is about.
    NATIVE_SITE_CACHE.with(|c| {
        c.borrow_mut().insert(
            key,
            (
                shared.natives.native_methods.generation(),
                Some(NativeSiteCache {
                    kind: LeafNativeKind::Callback,
                    leaf: true,
                    callback: cb,
                    native_id: None,
                    receiver_class_id: Some(0),
                    // The rungs below price an UNCLASSIFIED native, which
                    // is what the cache overwhelmingly serves.
                    capability_kind: None,
                    desc: NativeSiteDescriptor::decode(GET_INFO.descriptor),
                    // These rungs price the ORDINARY native dispatch. A
                    // signature-polymorphic entry takes a different tail
                    // (`try_varhandle_instance_field_read`), so setting
                    // this would make the numbers about something else.
                    poly: false,
                    vh_value_site: 0,
                    sync: None,
                    native_owner: None,
                    fill_redefine_epoch: 0,
                    owner_scoped: false,
                    refused_for_receiver: false,
                }),
            ),
        );
    });

    print!("{:<56}", "rung");
    for i in 1..=PASSES {
        print!("{i:>10}");
    }
    println!("   (ns/op per pass; read the LAST)");

    // --- the receiver validator, and how many times it is paid ---------
    rung("heap.is_object_address(receiver)", || {
        black_box(shared.mem.heap.is_object_address(recv_raw as usize));
    });
    rung("heap.class_id_of(receiver)", || {
        black_box(shared.mem.heap.class_id_of(recv));
    });
    rung("heap.class_id_of_validated(receiver)", || {
        black_box(shared.mem.heap.class_id_of_validated(recv));
    });
    rung("heap.load_and_forward(receiver)", || {
        black_box(shared.mem.heap.load_and_forward(recv));
    });

    println!();

    // --- jit_invoke_dispatch's preamble, per step ----------------------
    rung("note_jit_boundary()", || {
        crate::jit::conservative_roots::note_jit_boundary();
    });
    rung("heap.flush_thread_satb()", || {
        shared.mem.heap.flush_thread_satb();
    });
    rung("mic_prof::dump_maybe_disp()", || {
        mic_prof::dump_maybe_disp();
    });
    rung("env_cache::disable_jit()", || {
        black_box(crate::runtime::env_cache::disable_jit());
    });
    rung("forward_jit_reference_args: 1 recv, ()I", || {
        black_box(forward_jit_reference_args(&shared, &GET_INFO, &one_arg));
    });
    rung("forward_jit_reference_args: recv+obj, (LThread;)V", || {
        black_box(forward_jit_reference_args(
            &shared,
            &SET_OWNER_INFO,
            &two_args,
        ));
    });
    rung("flush_raw_entry_dispatch_caches()", || {
        flush_raw_entry_dispatch_caches();
    });
    rung("jit_site_key()", || {
        black_box(jit_site_key(
            shared.vm_identity,
            &GET_INFO as *const _ as usize,
        ));
    });
    rung("site_alias_detect_enabled() gate [off]", || {
        if black_box(site_alias_detect_enabled()) {
            note_site_identity(key, &GET_INFO);
        }
    });
    // What the gate BUYS, on the one call site that used to lack it
    // (`jit_invoke_virtual_mic`, fixed by `5d299ca6f`). This is the
    // steady state, and the CHEAPEST case: the key is already in the map
    // and still names the same site, so it is one thread-local access,
    // one `FxHashMap` lookup and three `String` comparisons, with no
    // insert and no `eprintln!`. The very first call for a key is dearer
    // — it allocates and stores three `String`s in a map that is never
    // bounded and never cleared — and a key that HAS been recycled costs
    // that again plus the event line. Seed the entry first, or this
    // measures the insert on pass 1 and the hit on passes 2..N and goes
    // flat at neither.
    note_site_identity(key, &GET_INFO);
    rung("note_site_identity() UNGUARDED [warm hit]", || {
        note_site_identity(black_box(key), &GET_INFO);
    });
    rung("OBJECT_NATIVE_DISPATCH_CACHE probe (miss)", || {
        black_box(OBJECT_NATIVE_DISPATCH_CACHE.with(|c| c.borrow().get(&key).copied()));
    });

    println!();

    // --- try_jit_site_cached_native_dispatch, per step -----------------
    rung("native_site_cache_enabled()", || {
        black_box(native_site_cache_enabled());
    });
    rung("native_methods.generation()", || {
        black_box(shared.natives.native_methods.generation());
    });
    rung("NATIVE_SITE_CACHE probe (hit)", || {
        black_box(NATIVE_SITE_CACHE.with(|c| c.borrow().get(&key).copied()));
    });
    rung("class_id_or_name_was_redefined()", || {
        black_box(class_id_or_name_was_redefined(
            &shared,
            GET_INFO.declaring_class_id,
            GET_INFO.class_name,
        ));
    });
    rung("class_was_redefined()", || {
        black_box(class_was_redefined(&shared, ClassId::new(0)));
    });
    rung("jit_thread_mut()", || {
        // SAFETY: measurement-only; no JIT thread is installed, so this
        // takes the null arm — which is exactly the TLS read being priced.
        black_box(unsafe { jit_thread_mut() }.is_some());
    });
    rung("decode_dispatch_values: 1 recv, ()I", || {
        // SAFETY: `one_arg` holds a validated heap address for `recv`.
        black_box(unsafe { decode_dispatch_values(&shared, &GET_INFO, &one_arg) });
    });
    rung("decode_dispatch_values: recv+obj, (LThread;)V", || {
        // SAFETY: both slots hold validated heap addresses.
        black_box(unsafe { decode_dispatch_values(&shared, &SET_OWNER_INFO, &two_args) });
    });
    rung("decode_dispatch_values: static, ()J", || {
        // SAFETY: an empty slice is trivially in-bounds.
        black_box(unsafe { decode_dispatch_values(&shared, &NANO_INFO, &no_args) });
    });
    let mut buf = JitDecodedArgs::new();
    rung("decode_..._into: 1 recv (resolved), ()I", || {
        // SAFETY: `one_arg[0]` is `recv`, already validated above.
        unsafe {
            decode_dispatch_values_into(
                &shared,
                &GET_INFO,
                &one_arg,
                Some(Some(recv)),
                &mut buf,
            )
        };
        black_box(&mut buf);
    });
    rung("decode_..._into: recv+obj (resolved), (LThread;)V", || {
        // SAFETY: both slots hold validated heap addresses.
        unsafe {
            decode_dispatch_values_into(
                &shared,
                &SET_OWNER_INFO,
                &two_args,
                Some(Some(recv)),
                &mut buf,
            )
        };
        black_box(&mut buf);
    });
    rung("decode_..._into: static, ()J", || {
        // SAFETY: an empty slice is trivially in-bounds.
        unsafe { decode_dispatch_values_into(&shared, &NANO_INFO, &no_args, None, &mut buf) };
        black_box(&mut buf);
    });

    rung("record_invocation() [no id]", || {
        count_jit_native_dispatch(&shared, None);
    });
    // The three DIAGNOSTIC counters every site-cached native dispatch pays,
    // priced separately because the rung above takes the `None` arm and so
    // measures nothing. The first two are per-thread `JitCounterBlock`
    // load+store bumps since round 11 (no `lock` prefix); only
    // `record_invocation` with a real id is still a shared `lock xadd`
    // (`native-api` registry, `r11-rt-helper-hot-paths` item 3).
    rung("counter: note_membership_walk(1)", || {
        note_membership_walk(1);
    });
    rung("counter: leaf-hit tally (per-thread block)", || {
        note_leaf_native_hit();
    });
    {
        let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
        { let r = &mut registry; 
            r.register("p/P", "m", "()V", noop_native, cratonvm_native_api::NativeKind::Bridge);
         }
        let id = registry.resolve_id("p/P", "m", "()V");
        rung("counter: record_invocation() [real id]", || {
            if let Some(id) = id {
                registry.record_invocation(id);
            }
        });
    }
    rung("counter: all three, as one dispatch pays them", || {
        note_membership_walk(1);
        note_leaf_native_hit();
        count_jit_native_dispatch(&shared, None);
    });
    rung("counter: both JIT-side tallies, one TLS access", || {
        JIT_COUNTERS.with(|b| {
            JitCounterBlock::bump(&b.membership_walk[1]);
            JitCounterBlock::bump(&b.leaf_native_hits);
        });
    });
    rung("coerce_native_return(Int, \"()I\")", || {
        black_box(crate::vm::coerce_native_return(
            Some(Value::Int(3)),
            GET_INFO.descriptor,
        ));
    });

    println!();

    // --- the two call wrappers, for scale ------------------------------
    let recv_val = [Value::Object(Some(recv))];
    rung("safe_native_call_leaf: 1 object arg", || {
        black_box(crate::vm::safe_native_call_leaf(
            &shared,
            &mut thread,
            cb,
            &recv_val,
        ))
        .ok();
    });
    rung("safe_native_call_prevalidated: 1 object arg", || {
        black_box(crate::vm::safe_native_call_prevalidated_objects(
            &shared,
            &mut thread,
            cb,
            &recv_val,
        ))
        .ok();
    });
    let two_vals = [Value::Object(Some(recv)), Value::Object(Some(other))];
    rung("safe_native_call_prevalidated: 2 object args", || {
        black_box(crate::vm::safe_native_call_prevalidated_objects(
            &shared,
            &mut thread,
            cb,
            &two_vals,
        ))
        .ok();
    });

    println!();

    // --- the one native body on the AQS path that is not a field write ---
    // `setExclusiveOwnerThread` is intercepted for `ThreadMXBean`, not for
    // speed, and the interception's whole cost is this index update: a
    // global mutex over a `HashMap`, the thread-registry `RwLock`, and a
    // per-thread `Mutex` over a `Vec`. It runs twice per uncontended
    // `ReentrantLock.lock()`/`unlock()` pair.
    let registry = &shared.threads.thread_registry;
    rung(
        "ThreadRegistry::set_jmx_owned_synchronizer (acquire+release)",
        || {
            registry.set_jmx_owned_synchronizer(
                Some(crate::threading::jvm_thread::ThreadId(0)),
                recv,
            );
            registry.set_jmx_owned_synchronizer(None, recv);
        },
    );
    // The own-handle form, for the same acquire/release pair. A bare
    //  has no registered thread, so the handle is built here
    // rather than fetched — which is what the owning thread holds.
    let own_list: parking_lot::Mutex<Vec<ObjectRef>> = parking_lot::Mutex::new(Vec::new());
    rung("  ... _own (acquire+release, no registry RwLock)", || {
        registry.set_jmx_owned_synchronizer_own(
            crate::threading::jvm_thread::ThreadId(0),
            &own_list,
            Some(crate::threading::jvm_thread::ThreadId(0)),
            recv,
        );
        registry.set_jmx_owned_synchronizer_own(
            crate::threading::jvm_thread::ThreadId(0),
            &own_list,
            None,
            recv,
        );
    });
}
