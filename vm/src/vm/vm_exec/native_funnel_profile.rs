use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

const PASSES: usize = 4;
const ROUNDS: u32 = 400_000;

fn noop_native(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

/// One row: run `f` `ROUNDS` times per pass, print ns/op for every pass.
fn rung(label: &str, mut f: impl FnMut()) {
    print!("{label:<52}");
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

#[test]
#[ignore = "measurement, not an assertion — see the module doc"]
fn funnel_cost_breakdown() {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(crate::VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "funnel-profile");
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let cb: NativeCallback = noop_native;

    print!("{:<52}", "rung");
    for i in 1..=PASSES {
        print!("{i:>10}");
    }
    println!("   (ns/op per pass; read the LAST)");

    // --- the whole funnel, by argument shape -------------------------
    rung("safe_native_call: 0 args", || {
        black_box(safe_native_call(&shared, &mut thread, cb, &[])).ok();
    });
    let one_int = [Value::Int(7)];
    rung("safe_native_call: 1 int arg", || {
        black_box(safe_native_call(&shared, &mut thread, cb, &one_int)).ok();
    });
    let one_obj = [Value::Object(Some(obj))];
    rung("safe_native_call: 1 object arg", || {
        black_box(safe_native_call(&shared, &mut thread, cb, &one_obj)).ok();
    });
    let one_long = [Value::Long(0x1234_5678)];
    rung("safe_native_call: 1 long arg", || {
        black_box(safe_native_call(&shared, &mut thread, cb, &one_long)).ok();
    });
    let four = [
        Value::Object(Some(obj)),
        Value::Long(0x1234_5678),
        Value::Int(1),
        Value::Int(2),
    ];
    rung("safe_native_call: 4 args (obj,long,int,int)", || {
        black_box(safe_native_call(&shared, &mut thread, cb, &four)).ok();
    });
    rung("safe_native_call_prevalidated: 1 object arg", || {
        black_box(safe_native_call_prevalidated_objects(
            &shared,
            &mut thread,
            cb,
            &one_obj,
        ))
        .ok();
    });
    // JIT round 17 wave 2 (lane funnel): the lean JIT-door funnel, read
    // against the row above.
    rung("safe_native_call_jit_lean: 1 object arg", || {
        black_box(safe_native_call_jit_lean(
            &shared,
            &mut thread,
            cb,
            &one_obj,
        ))
        .ok();
    });

    println!();

    // --- the callback itself, with nothing around it -----------------
    rung("BARE callback (no funnel at all)", || {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        black_box(noop_native(&mut ctx, &[])).ok();
    });

    // --- funnel components, one at a time ----------------------------
    rung("component: native_diag_mask()", || {
        black_box(native_diag_mask());
    });
    rung("component: catch_unwind around the callback", || {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        black_box(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                noop_native(&mut ctx, &[])
            }))
            .map_err(|_| ()),
        )
        .ok();
    });
    rung("component: 2x thread_state::record_transition", || {
        let prior = thread_state::current_state();
        thread_state::record_transition(ThreadExecState::NativeRunning, "funnel-profile");
        thread_state::record_transition(prior, "funnel-profile");
    });
    // What the funnel does instead since 2026-08-17: the same pair of
    // transitions, one thread-local access. The row above is the control
    // and stays, because "the new one is fast" is only a claim next to it.
    rung("component:   ... as one NativeStateSpan", || {
        thread_state::enter_native_state("funnel-profile").restore("funnel-profile");
    });
    rung("component:   ... current_state() alone", || {
        black_box(thread_state::current_state());
    });
    rung("component: native_oom enter + restore", || {
        let depth = crate::runtime::native_oom::enter_native_call();
        crate::runtime::native_oom::restore(depth);
    });
    rung("component: pin push + truncate (1 object)", || {
        let base = thread.native_pin_roots.len();
        pin_value_for_native_call(&shared, &mut thread.native_pin_roots, &one_obj[0]);
        thread.native_pin_roots.truncate(base);
    });
    rung("component: value_as_validated_object_ref(Long)", || {
        black_box(value_as_validated_object_ref(&shared, one_long[0]));
    });
    rung("component: heap.load_and_forward(obj)", || {
        black_box(shared.mem.heap.load_and_forward(obj));
    });
    rung("component: stw_requested load", || {
        black_box(
            shared
                .mem
                .gc_barrier
                .stw_requested
                .load(std::sync::atomic::Ordering::Acquire),
        );
    });
    rung("component: young_spill_pressure()", || {
        black_box(shared.mem.heap.young_spill_pressure());
    });
    rung("component: disable_jit() + native_array_gc swap", || {
        black_box(
            crate::runtime::env_cache::disable_jit()
                && shared
                    .mem
                    .native_array_gc_requested
                    .swap(false, std::sync::atomic::Ordering::Relaxed),
        );
    });
    rung("component: take_jni_pending_exception()", || {
        black_box(crate::native::jni::take_jni_pending_exception());
    });
    rung(
        "component: the two INLINE_NATIVE_ARGS scratch arrays",
        || {
            const N: usize = crate::jit::helpers::INLINE_JIT_NATIVE_ARGS;
            let mut forwarded = [Value::Object(None); N];
            let mut roots = [None::<usize>; N];
            black_box(&mut forwarded);
            black_box(&mut roots);
        },
    );
}
