// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 64, lane L3 -- the per-call cost of the door
// record's note (`CRATONVM_THROWABLE_SERVED_RAISE_DOOR_KEY`) on the doors it
// applies to, and what the per-thread selection memo
// (`CRATONVM_THROWABLE_SERVED_RAISE_DOOR_MEMO`, `stackwalker::DoorKeyMemo`)
// gives back. Each door note costs a class-manager read and a method scan
// without the memo; with it, a hash of the method name and a compare.
//
//   reflect-static-jdk - `Method.invoke` of a static JDK method (the served
//                        `Method.invoke`'s door, noted since wave 64)
//   handle-static-jdk  - `invokeExact` of a `REF_invokeStatic` handle to a
//                        static JDK method (a static door, `--compatible`)
//   reflect-static-app - `Method.invoke` of a static application method
//                        (never noted: the control)
//
// What to measure: each row's ns/op on stderr, `--compatible`, JIT on and
// `--nojit`, interleaved between (a) every switch unset, (b) `DOOR_KEY=1`,
// (c) `DOOR_KEY=1 DOOR_MEMO=1`; medians of several runs (in-JVM timings
// swing ~3x on the host). (b) - (a) is the note's cost; (c) should close
// most of it on the two JDK rows and leave the control unchanged.
//
// Stdout is a deterministic checksum per row and must match HotSpot 25, with
// and without `--nojit`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// reflect-static-jdk: 400000
// handle-static-jdk: 400000
// reflect-static-app: 400000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;

public class L3W64DoorKeyBench {
    static final int WARMUP = 20_000;
    static final int ITERS = 400_000;

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
        Method signum = Integer.class.getMethod("signum", int.class);
        Method app = L3W64DoorKeyBench.class.getDeclaredMethod("one", int.class);
        MethodHandle handle = MethodHandles.lookup().findStatic(Integer.class, "signum",
                MethodType.methodType(int.class, int.class));
        bench("reflect-static-jdk", i -> (Integer) signum.invoke(null, i));
        bench("handle-static-jdk", i -> (int) handle.invokeExact(i));
        bench("reflect-static-app", i -> (Integer) app.invoke(null, i));
    }
}
