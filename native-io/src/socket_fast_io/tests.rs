use super::*;

/// Price the work `Scratch` removes, directly.
///
/// `#[ignore]`d because it is a MEASUREMENT, not an assertion — the same
/// convention as `vm_exec.rs::native_funnel_profile` and
/// `jit/helpers.rs::jit_native_dispatch_profile`:
///
/// ```bash
/// cargo test --release -p cratonvm-native-io --lib -- --ignored --nocapture scratch_cost
/// ```
///
/// **Why this exists rather than an end-to-end socket probe.**
/// `probes/SocketTransferCostProbe.java` prices a read through a loopback
/// socket and CANNOT resolve this effect on a Windows host: one write+read
/// pair costs ~65 µs there, rounds drift 2x under ambient load, and the
/// HotSpot control — which must show no capacity dependence at all —
/// reported deltas from -4 350 to +6 168 ns. A quantity smaller than its
/// own control's spread is not measured, and quoting it would be quoting
/// the host.
///
/// So price the changed work in isolation and get the multiplier from the
/// census (`CRATONVM_SC_IO_STATS=1`) on a real workload instead. That is
/// the method that corrected the native-call figure from a Java probe's
/// 340-490 ns to the funnel's true ~120 ns, and it is robust to load in a
/// way a wall-clock socket number is not.
///
/// The number this prints is an UPPER BOUND on the per-call saving: it is
/// what the old path spent on allocation and zeroing, which the new path
/// spends nothing on once the high-water mark is reached. It is not a
/// throughput claim, and multiplying it by a call count gives a ceiling,
/// not a prediction.
#[test]
#[ignore = "measurement, not an assertion"]
fn scratch_cost_profile() {
    // Sizes a reactor actually offers: netty's adaptive allocator caps at
    // 65 536, Tomcat's NIO read buffer defaults to 8 192.
    for cap in [128usize, 1024, 8192, 65536] {
        // Warm the thread's buffer to this size first, so the measured
        // passes are steady state rather than one growth.
        {
            let mut w = Scratch::new(cap);
            std::hint::black_box(w.as_mut());
        }
        const PASSES: usize = 3;
        const N: usize = 20_000;
        let mut old_ns = u128::MAX;
        let mut new_ns = u128::MAX;
        for _ in 0..PASSES {
            // OLD: a fresh zeroed allocation per call.
            let t = std::time::Instant::now();
            for _ in 0..N {
                let mut v = vec![0u8; cap];
                std::hint::black_box(&mut v[..]);
            }
            old_ns = old_ns.min(t.elapsed().as_nanos() / N as u128);

            // NEW: the thread's buffer, already long enough.
            let t = std::time::Instant::now();
            for _ in 0..N {
                let mut sc = Scratch::new(cap);
                std::hint::black_box(sc.as_mut());
            }
            new_ns = new_ns.min(t.elapsed().as_nanos() / N as u128);
        }
        // The MINIMUM across passes, not the mean: on a loaded host the
        // mean measures the load and the minimum measures the code.
        println!(
            "scratch cap={cap:6}  alloc+zero={old_ns:6} ns  reuse={new_ns:6} ns \
              saved={:6} ns/call",
            old_ns.saturating_sub(new_ns)
        );
    }
}

/// The high-water buffer must hand back exactly the requested length, and
/// must not shrink when a smaller request follows a larger one — the
/// shrink is what would reintroduce the zeroing this module removes.
#[test]
fn scratch_holds_high_water_mark() {
    {
        let mut big = Scratch::new(4096);
        assert_eq!(big.as_mut().len(), 4096);
        big.as_mut()[0] = 0xAB;
    }
    {
        let small = Scratch::new(16);
        assert_eq!(small.as_slice().len(), 16);
    }
    {
        // Back up to the earlier high-water mark: served without growing.
        let again = Scratch::new(4096);
        assert_eq!(again.as_slice().len(), 4096);
    }
}

/// A request above the retention cap must still be served correctly — it
/// is a one-off allocation, not a refusal.
#[test]
fn scratch_serves_oversized_requests() {
    let big = Scratch::new(SCRATCH_MAX_RETAIN + 1);
    assert_eq!(big.as_slice().len(), SCRATCH_MAX_RETAIN + 1);
}

/// `prefix` clamps rather than panicking: `n` is a kernel return value and
/// a transport answering more than it was offered must truncate.
#[test]
fn prefix_clamps_to_length() {
    let s = Scratch::new(8);
    assert_eq!(s.prefix(4).len(), 4);
    assert_eq!(s.prefix(99).len(), 8);
}

/// A nested `Scratch` on one thread must not deadlock or panic on the
/// thread-local borrow, and the outer buffer must survive.
#[test]
fn nested_scratch_does_not_double_borrow() {
    let outer = Scratch::new(2048);
    let inner = Scratch::new(64);
    assert_eq!(outer.as_slice().len(), 2048);
    assert_eq!(inner.as_slice().len(), 64);
}
