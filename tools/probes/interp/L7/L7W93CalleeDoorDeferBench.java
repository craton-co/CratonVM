// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 93, lane L7: the by-name callee door
// (`jit_bridge::try_jit_compile_callee*`) compiles an uncompiled callee ON THE
// MUTATOR when a compiled caller's dispatch helper first reaches it, even
// under the background compiler (the tier-up doors and the first-call door
// all enqueue). `CRATONVM_JIT_CALLEE_DOOR_DEFERS_TO_WORKER` (default off)
// enqueues that compile instead and answers the interpreter fallback
// (i89-L4-method-finder-per-class-and-per-handle-cost, wave 93).
//
// Shape: `drive(IntOp, n)` is warmed until compiled with a megamorphic
// `apply` site; then each op is a FRESH class implementing IntOp (defined
// untimed), whose `apply` the compiled `drive` reaches first through its
// dispatch helper. apply(x) = x * k + 1 for the k-th class.
//
// Rows (CLASSES = 300; one op = one class):
//   fresh-callee  drive(op, CALLS) on a class just defined: the door's first
//                 sight of the callee (switch off: a compile on this thread)
//   warm-callee   drive(op, CALLS) again over the same 300 classes, after
//                 the worker had time to publish (the floor)
// A/B: fresh-callee with the switch off and on; warm-callee must not move.
// The census (`CRATONVM_DBG=mh-finder-census`) has no scope here; read
// `CRATONVM_DBG_JITC=1` for `callee-door DEFERRED` lines instead.
//
// Run:
//   javac -d out L7W93CalleeDoorDeferBench.java
//   java -cp out L7W93CalleeDoorDeferBench                                   (HotSpot reference)
//   cratonvm --jdk-only -cp out L7W93CalleeDoorDeferBench
//   CRATONVM_JIT_CALLEE_DOOR_DEFERS_TO_WORKER=1 cratonvm --jdk-only -cp out L7W93CalleeDoorDeferBench
// Interleave the arms, several rounds, medians. Also under --compatible.
//
// Output: "row <name> min <ns> med <ns> ns/op" and "checksum <name> <n>".
// checksum = sum over k = 1..300 of sum over x = 0..199 of (x * k + 1)
//          = 19900 * (300 * 301 / 2) + 200 * 300 = 898545000.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum fresh-callee 898545000
// checksum warm-callee 898545000
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.Arrays;

public class L7W93CalleeDoorDeferBench {
    public interface IntOp {
        int apply(int x);
    }

    static final int CLASSES = 300;
    static final int CALLS = 200;

    static final class Loader extends ClassLoader {
        Loader() {
            super(L7W93CalleeDoorDeferBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);
    static final ClassDesc INT_OP = ClassDesc.of(IntOp.class.getName());

    /** A public class implementing IntOp with apply(x) = x * k + 1. */
    static byte[] build(String name, int k) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(INT_OP);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
            cb.withMethodBody("apply", MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC,
                    code -> code.iload(1).ldc(k).imul().iconst_1().iadd().ireturn());
        });
    }

    static IntOp make(Loader loader, String name, int k) throws Exception {
        Class<?> c = loader.define(name, build(name, k));
        return (IntOp) c.getConstructor().newInstance();
    }

    static long drive(IntOp op, int n) {
        long sum = 0;
        for (int x = 0; x < n; x++) {
            sum += op.apply(x);
        }
        return sum;
    }

    static double median(long[] xs) {
        long[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2.0;
    }

    static long min(long[] xs) {
        long best = Long.MAX_VALUE;
        for (long x : xs) best = Math.min(best, x);
        return best;
    }

    static void report(String name, long[] t, long checksum) {
        System.out.printf("row %s min %d med %.0f ns/op%n", name, min(t), median(t));
        System.out.println("checksum " + name + " " + checksum);
    }

    public static void main(String[] args) throws Exception {
        Loader loader = new Loader();
        // Warm `drive` until compiled, with a megamorphic `apply` site.
        IntOp[] warm = {
            make(loader, "W93Warm1", 1001), make(loader, "W93Warm2", 1002),
            make(loader, "W93Warm3", 1003), make(loader, "W93Warm4", 1004),
        };
        long sink = 0;
        for (int i = 0; i < 20000; i++) {
            sink += drive(warm[i & 3], 64);
        }
        if (sink == 42) {
            System.out.println("unreachable");
        }
        IntOp[] ops = new IntOp[CLASSES];
        long[] t = new long[CLASSES];
        long checksum = 0;
        for (int k = 1; k <= CLASSES; k++) {
            IntOp op = make(loader, "W93Fresh" + k, k);
            ops[k - 1] = op;
            long t0 = System.nanoTime();
            long s = drive(op, CALLS);
            t[k - 1] = System.nanoTime() - t0;
            checksum += s;
        }
        report("fresh-callee", t, checksum);
        Thread.sleep(200);
        checksum = 0;
        for (int k = 1; k <= CLASSES; k++) {
            long t0 = System.nanoTime();
            long s = drive(ops[k - 1], CALLS);
            t[k - 1] = System.nanoTime() - t0;
            checksum += s;
        }
        report("warm-callee", t, checksum);
    }
}
