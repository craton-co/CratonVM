// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 16, lane L3: the by-name invocation routes a
// native reaches when it calls back into Java.
//
//   reflect-static  - `Method.invoke(null, i)` on a static
//   reflect-virtual - `Method.invoke(target, i)` on an instance method
//   mh-static       - `MethodHandle.invokeExact(i)` from `findStatic`
//                     (wave 16: `mh_dispatch` -> `NativeContext::invoke_static`
//                     -> `invoke_static_shared`)
//   mh-inherited    - the same through a subclass that inherits the static
//                     (the declaring-class initialization check on every call)
//   mh-virtual      - `MethodHandle.invokeExact(target, i)` from `findVirtual`
//   lambda          - a bound method reference `IntUnaryOperator op =
//                     target::add; op.applyAsInt(i)` (the interpreter's
//                     lambda dispatch)
//
// What to measure: each row's ns/call on stderr, interleaved against the
// previous build (medians of several runs; in-JVM timings swing). Wave 16
// should leave `mh-static` flat and may make `mh-inherited` slower per call
// (it re-resolves the declaring class each call instead of initializing the
// subclass once) — that row is the one to watch. `reflect-*`,
// `mh-virtual` and `lambda` are the baseline for the next by-name stages
// (i13-L5 memo proposal, i14-L5 invoke-kind proposal).
//
// Wave 20 (lane L3): a `findStatic` handle remembers its owner once a call
// has left nothing to initialize (`MH_STATIC_SETTLED`,
// `NativeInvokeAccess::invoke_static_settling`), and later calls skip the
// initialization check. `mh-inherited` should get faster (it paid a
// class-manager read and a member resolution per call for the check) and
// `mh-static` flat or slightly faster; the other rows are controls.
//
// Stdout is a deterministic checksum per row and must match HotSpot 25.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.function.IntUnaryOperator;

public class ByNameCallBench {
    public static class Base {
        public static int twice(int x) {
            return x * 2 + 1;
        }
    }

    public static class Derived extends Base {
    }

    public static class Target {
        private final int bias;

        public Target(int bias) {
            this.bias = bias;
        }

        public int add(int x) {
            return x + bias;
        }

        public static int square(int x) {
            return x * x;
        }
    }

    static final int WARMUP = 20_000;
    static final int ITERS = 200_000;

    interface Row {
        long run(int iters) throws Throwable;
    }

    static void row(String name, Row r) throws Throwable {
        r.run(WARMUP);
        long t0 = System.nanoTime();
        long sum = r.run(ITERS);
        long ns = System.nanoTime() - t0;
        System.out.println(name + " " + sum);
        System.err.printf("%-16s %8.1f ns/call%n", name, (double) ns / ITERS);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodType intToInt = MethodType.methodType(int.class, int.class);
        Target target = new Target(3);

        Method square = Target.class.getMethod("square", int.class);
        Method add = Target.class.getMethod("add", int.class);
        MethodHandle mhStatic = lookup.findStatic(Target.class, "square", intToInt);
        MethodHandle mhInherited = lookup.findStatic(Derived.class, "twice", intToInt);
        MethodHandle mhVirtual = lookup.findVirtual(Target.class, "add", intToInt);
        IntUnaryOperator op = target::add;

        row("reflect-static", n -> {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += (Integer) square.invoke(null, i & 1023);
            }
            return s;
        });
        row("reflect-virtual", n -> {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += (Integer) add.invoke(target, i & 1023);
            }
            return s;
        });
        row("mh-static", n -> {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += (int) mhStatic.invokeExact(i & 1023);
            }
            return s;
        });
        row("mh-inherited", n -> {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += (int) mhInherited.invokeExact(i & 1023);
            }
            return s;
        });
        row("mh-virtual", n -> {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += (int) mhVirtual.invokeExact(target, i & 1023);
            }
            return s;
        });
        row("lambda", n -> {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += op.applyAsInt(i & 1023);
            }
            return s;
        });
    }
}
