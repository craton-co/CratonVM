// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 40, lane L2:
// `docs/internal/fixed-bugs/interpreter-L7-compiled-recursion-ignores-a-threads-requested-stack-size-FIXED-20261005.md`,
// the remainder wave 39 left: compiled recursion that goes through the JIT
// dispatch helpers (`jit_invoke_dispatch` / `jit_invoke_virtual_mic`, not a
// direct self call and not a raw inline-cache hit) is bounded by the
// per-thread dispatch depth ceiling, which was half the carrier at 8 KiB per
// level: 2560 levels on the 40 MiB carrier of a 32 MiB request. Since wave 40
// a carrier above the default keeps only the default's 4 MiB of headroom
// (`interpreter::derive_recorded_jit_dispatch_depth_ceiling`): 4608 levels
// there, as far as the compiled self-recursion floor lets a direct self call
// go on it (wave 39).
//
// Three recursion shapes, each warmed on its own thread (20 000 calls at
// depth 20, so every level is compiled) and then run 3 000 levels deep on a
// 32 MiB thread; a 400-level run of each on a default thread is the control;
// an unbounded recursion on a 32 MiB thread must still end in
// StackOverflowError.
//   self     a static method calling itself (the wave-39 shape)
//   mutual   two static methods of two classes calling each other
//   mega     a virtual call whose receiver rotates over 8 classes (past the
//            4-way inline cache)
//
// Run: javac -d out L2W40DispatchRecursionRequestedStack.java
//      cratonvm [--nojit] -cp out L2W40DispatchRecursionRequestedStack
//
// Expected HotSpot 25 output (default and -Xint; local JDK 25.0.3), and
// CratonVM's in every mode:
//   self requested-32m-3000 4501500
//   self default-400 80200
//   mutual requested-32m-3000 4501500
//   mutual default-400 80200
//   mega requested-32m-3000 4501500
//   mega default-400 80200
//   requested-runaway soe
//
// Positive control (CratonVM, CRATONVM_DBG_JITC=1, stderr): once per 32 MiB
// thread
//   [cratonvm-jitc] jit-dispatch depth ceiling scaled: carrier=41943040 ceiling=4608
// and no `jit-dispatch depth ceiling TRIP` line before the runaway row. A
// `requested-32m-3000 soe` row names its bound on stderr: `jit-dispatch
// depth ceiling TRIP` (the ceiling), `jit-dispatch native stack floor TRIP`
// or `self-call stack guard TRIP` (the floor). Which shapes reach the
// dispatch helpers at every level depends on the call-site binding (a raw
// direct call or inline-cache hit is not counted); a shape that never
// prints a ceiling TRIP on the pre-wave-40 build does not reach it.
public class L2W40DispatchRecursionRequestedStack {

    static long self(int n) {
        return n == 0 ? 0 : n + self(n - 1);
    }

    static class Ping {
        static long ping(int n) {
            return n == 0 ? 0 : n + Pong.pong(n - 1);
        }
    }

    static class Pong {
        static long pong(int n) {
            return n == 0 ? 0 : n + Ping.ping(n - 1);
        }
    }

    abstract static class Node {
        abstract long down(int n);
    }

    static final Node[] NODES = {
        new N0(), new N1(), new N2(), new N3(), new N4(), new N5(), new N6(), new N7(),
    };

    static long mega(int n) {
        return NODES[n & 7].down(n);
    }

    static final class N0 extends Node {
        long down(int n) { return n == 0 ? 0 : n + NODES[(n - 1) & 7].down(n - 1); }
    }

    static final class N1 extends Node {
        long down(int n) { return n == 0 ? 0 : n + NODES[(n - 1) & 7].down(n - 1); }
    }

    static final class N2 extends Node {
        long down(int n) { return n == 0 ? 0 : n + NODES[(n - 1) & 7].down(n - 1); }
    }

    static final class N3 extends Node {
        long down(int n) { return n == 0 ? 0 : n + NODES[(n - 1) & 7].down(n - 1); }
    }

    static final class N4 extends Node {
        long down(int n) { return n == 0 ? 0 : n + NODES[(n - 1) & 7].down(n - 1); }
    }

    static final class N5 extends Node {
        long down(int n) { return n == 0 ? 0 : n + NODES[(n - 1) & 7].down(n - 1); }
    }

    static final class N6 extends Node {
        long down(int n) { return n == 0 ? 0 : n + NODES[(n - 1) & 7].down(n - 1); }
    }

    static final class N7 extends Node {
        long down(int n) { return n == 0 ? 0 : n + NODES[(n - 1) & 7].down(n - 1); }
    }

    interface Shape {
        long run(int n);
    }

    static int runaway(int n) {
        return runaway(n + 1) + 1;
    }

    static String deep(Shape shape, int levels) {
        long warm = 0;
        for (int i = 0; i < 20_000; i++) {
            warm += shape.run(20);
        }
        if (warm != 20_000L * 210) {
            return "bad-warmup";
        }
        try {
            return Long.toString(shape.run(levels));
        } catch (StackOverflowError e) {
            return "soe";
        }
    }

    static String onThread(long stackSize, java.util.function.Supplier<String> task)
            throws Exception {
        String[] out = new String[1];
        Runnable r = () -> out[0] = task.get();
        Thread t = stackSize == 0 ? new Thread(r, "deep") : new Thread(null, r, "deep", stackSize);
        t.start();
        t.join();
        return out[0];
    }

    public static void main(String[] args) throws Exception {
        long m32 = 32L << 20;
        String[] names = {"self", "mutual", "mega"};
        Shape[] shapes = {
            L2W40DispatchRecursionRequestedStack::self,
            Ping::ping,
            L2W40DispatchRecursionRequestedStack::mega,
        };
        for (int s = 0; s < shapes.length; s++) {
            Shape shape = shapes[s];
            System.out.println(names[s] + " requested-32m-3000 "
                    + onThread(m32, () -> deep(shape, 3000)));
            System.out.println(names[s] + " default-400 "
                    + onThread(0, () -> deep(shape, 400)));
        }
        System.out.println("requested-runaway " + onThread(m32, () -> {
            try {
                runaway(0);
                return "returned";
            } catch (StackOverflowError e) {
                return "soe";
            }
        }));
    }
}
