// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 66, lane L3 -- the per-call cost of the
// `--compatible` carrier handle's static door note with and without its kept
// door key (`CRATONVM_THROWABLE_SERVED_RAISE_CARRIER_DOOR_KEY`, which takes
// effect only with `CRATONVM_THROWABLE_SERVED_RAISE_SETTLED_DOOR_KEY`).
//
//   handle-static-jdk        - `invokeExact` of one carrier to a static JDK
//                              method (`L3W64DoorKeyBench`'s row)
//   handle-static-jdk-invoke - the same through `invoke` (the asType door)
//   handle-static-jdk-many   - 32 carriers on the same method, round-robin:
//                              more carriers than side-line slots (16), so
//                              lines are evicted; the miss-and-refill cost
//   handle-static-app        - `invokeExact` of a carrier to a static
//                              application method (never noted: the control)
//
// What to measure: each row's ns/op on stderr, `--compatible`, JIT on and
// `--nojit`, interleaved between (a) every switch unset, (b) `DOOR_KEY=1
// SETTLED_DOOR_KEY=1`, (c) (b) plus `CARRIER_DOOR_KEY=1`, (d) (b) plus
// `DOOR_MEMO=1`; medians of several runs (in-JVM timings swing ~3x on the
// host). (c) should close most of (b) - (a) on the first two JDK rows; the
// `many` row shows what a thrashed line costs against (b); the control
// should not move.
//
// Stdout is a deterministic checksum per row and must match HotSpot 25, with
// and without `--nojit`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// handle-static-jdk: 400000
// handle-static-jdk-invoke: 400000
// handle-static-jdk-many: 400000
// handle-static-app: 400000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L3W66CarrierDoorKeyBench {
    static final int WARMUP = 20_000;
    static final int ITERS = 400_000;
    static final int MANY = 32;

    static int one(int x) {
        return Integer.signum(x);
    }

    interface Body {
        int run(int i) throws Throwable;
    }

    static void bench(String name, Body body) throws Throwable {
        int sum = 0;
        for (int i = 0; i < WARMUP; i++) {
            sum += body.run(i + 1);
        }
        sum = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < ITERS; i++) {
            sum += body.run(i + 1);
        }
        long t1 = System.nanoTime();
        System.out.println(name + ": " + sum);
        System.err.printf("%s %.1f ns/op%n", name, (t1 - t0) / (double) ITERS);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodType intInt = MethodType.methodType(int.class, int.class);
        MethodHandle handle = lookup.findStatic(Integer.class, "signum", intInt);
        MethodHandle viaInvoke = lookup.findStatic(Integer.class, "signum", intInt);
        MethodHandle[] many = new MethodHandle[MANY];
        for (int i = 0; i < MANY; i++) {
            many[i] = lookup.findStatic(Integer.class, "signum", intInt);
        }
        MethodHandle app = lookup.findStatic(L3W66CarrierDoorKeyBench.class, "one", intInt);
        bench("handle-static-jdk", i -> (int) handle.invokeExact(i));
        bench("handle-static-jdk-invoke", i -> (int) viaInvoke.invoke(i));
        bench("handle-static-jdk-many", i -> (int) many[i & (MANY - 1)].invokeExact(i));
        bench("handle-static-app", i -> (int) app.invokeExact(i));
    }
}
