// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 40, lane L2:
// `docs/internal/fixed-bugs/interpreter-L2-compiled-recursion-through-the-dispatch-helpers-stops-at-512-levels-on-a-default-thread-RETIRED-20261005.md`.
// On a thread started with no stack size (an 8 MiB carrier), the JIT
// dispatch depth ceiling is 512 levels (`derive_jit_dispatch_depth_ceiling`:
// 8 MiB / 2 / 8 KiB), while the interpreter admits 8192 frames. A compiled
// recursion whose every level (or every other level) enters a dispatch
// helper (`jit_invoke_dispatch` / `jit_invoke_virtual_mic`) then throws
// StackOverflowError at a depth HotSpot and `--nojit` complete.
//
// Three shapes, each warmed on its own default thread (20 000 calls at depth
// 20) and then run 2 000 levels deep on that thread:
//   self     a static method calling itself (a direct self call: not counted)
//   mutual   two static methods of two classes calling each other
//   mega     a virtual call whose receiver rotates over 8 classes
//
// Run: javac -d out L2W40DefaultThreadCompiledRecursion.java
//      cratonvm [--nojit] -cp out L2W40DefaultThreadCompiledRecursion
//
// Expected HotSpot 25 output (default and -Xint; local JDK 25.0.3):
//   self default-2000 2001000
//   mutual default-2000 2001000
//   mega default-2000 2001000
// `--nojit` CratonVM: the same (8192 interpreter frames). Default-mode
// CratonVM: a row whose shape enters a dispatch helper at 1 000 or more of
// its levels is expected to print `soe` until the page is fixed; stderr under
// CRATONVM_DBG_JITC=1 then has `jit-dispatch depth ceiling TRIP: depth=513
// ceiling=512`. A row that matches with no TRIP line is a shape that does
// not reach the helpers (a direct call, an inline-cache hit or the
// megamorphic hashed stub).
public class L2W40DefaultThreadCompiledRecursion {

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

    public static void main(String[] args) throws Exception {
        String[] names = {"self", "mutual", "mega"};
        Shape[] shapes = {
            L2W40DefaultThreadCompiledRecursion::self,
            Ping::ping,
            L2W40DefaultThreadCompiledRecursion::mega,
        };
        for (int s = 0; s < shapes.length; s++) {
            Shape shape = shapes[s];
            String[] out = new String[1];
            Thread t = new Thread(() -> out[0] = deep(shape, 2000), "default");
            t.start();
            t.join();
            System.out.println(names[s] + " default-2000 " + out[0]);
        }
    }
}
