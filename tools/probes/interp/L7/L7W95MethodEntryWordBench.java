// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 95, lane L7 (stage 1 of
// docs/internal/proposals/interpreter/i62-L1-proposal-one-agent-request-word-the-dispatch-loop-reads-20261026.md):
// the per-invoke cost of `CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL` with NO
// agent. The switch adds nothing per bytecode or per invoke by design: the
// writer runs only behind the `MethodEntry` listener load the invoke doors
// already make, and the reader only in the dispatch loop's slow path (the
// poll word). What it does change is code: the invoke doors and the general
// dispatchers now call the `_in_loop` entry hooks
// (`jvmti_events::fire_method_entry_after_push_in_loop`,
// `install_cached_frame::<IN_LOOP>`), and the loop carries one more
// `diag_gates` bit. These rows say whether that moved the call paths.
//
// Rows (ROUNDS = 400 rounds each; one op = CALLS = 4096 calls, or the same
// loop with no call):
//   loop-only     the counted loop with the call replaced by an add (floor)
//   static        invokestatic of a two-bytecode static method (static door)
//   virtual       monomorphic invokevirtual of a final-class method
//   interface     monomorphic invokeinterface
//   recursive     a self-recursive static method, depth 16 (no frame switch
//                 between caller and callee: the same code pointer)
//   sync-static   invokestatic of a `static synchronized` method
// Per-call cost = (row - loop-only) / CALLS.
//
// Run (--nojit: pure interpretation; each arm with the switch off and on):
//   javac -d out L7W95MethodEntryWordBench.java
//   java -cp out L7W95MethodEntryWordBench                                    (HotSpot reference)
//   cratonvm --jdk-only --nojit -cp out L7W95MethodEntryWordBench
//   CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL=1 cratonvm --jdk-only --nojit -cp out L7W95MethodEntryWordBench
// Interleave the arms, several rounds, medians (the box's in-JVM timings
// swing ~3x between reps).
//
// Output: "checksum <row> <n>" on stdout (deterministic); the timing line
// "row <row> min <ns> med <ns> ns/op" on stderr.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// checksum loop-only 1638400
// checksum static 1638400
// checksum virtual 1638400
// checksum interface 1638400
// checksum recursive 6400
// checksum sync-static 1638400
// HOTSPOT_EXPECTED_END

import java.util.Arrays;

public class L7W95MethodEntryWordBench {
    static final int ROUNDS = 400;
    static final int WARM = 40;
    static final int CALLS = 4096;
    static final int DEPTH = 16;

    interface Row {
        int op();
    }

    interface I {
        int f(int x);
    }

    static final class C implements I {
        public int f(int x) {
            return x;
        }

        int g(int x) {
            return x;
        }
    }

    static final C TARGET = new C();
    static final I ITARGET = TARGET;

    static int one(int x) {
        return x;
    }

    static synchronized int oneSync(int x) {
        return x;
    }

    // One per level: 16 calls + the outermost = the depth, 1 returned.
    static int down(int n) {
        return n == 0 ? 1 : down(n - 1);
    }

    static int loopOnly() {
        int s = 0;
        for (int i = 0; i < CALLS; i++) {
            s += 1;
        }
        return s;
    }

    static int statics() {
        int s = 0;
        for (int i = 0; i < CALLS; i++) {
            s += one(1);
        }
        return s;
    }

    static int virtuals() {
        C c = TARGET;
        int s = 0;
        for (int i = 0; i < CALLS; i++) {
            s += c.g(1);
        }
        return s;
    }

    static int interfaces() {
        I t = ITARGET;
        int s = 0;
        for (int i = 0; i < CALLS; i++) {
            s += t.f(1);
        }
        return s;
    }

    // CALLS / DEPTH chains of DEPTH + 1 frames each: ~CALLS calls per op.
    static int recursive() {
        int s = 0;
        for (int i = 0; i < CALLS / DEPTH / 16; i++) {
            s += down(DEPTH);
        }
        for (int i = 0; i < CALLS / DEPTH - CALLS / DEPTH / 16; i++) {
            down(DEPTH);
        }
        return s;
    }

    static int syncStatics() {
        int s = 0;
        for (int i = 0; i < CALLS; i++) {
            s += oneSync(1);
        }
        return s;
    }

    static void time(String row, Row r) {
        for (int i = 0; i < WARM; i++) {
            r.op();
        }
        long[] ns = new long[ROUNDS];
        long checksum = 0;
        for (int i = 0; i < ROUNDS; i++) {
            long t0 = System.nanoTime();
            checksum += r.op();
            ns[i] = System.nanoTime() - t0;
        }
        Arrays.sort(ns);
        System.out.println("checksum " + row + " " + checksum);
        System.err.println("row " + row + " min " + ns[0] + " med " + ns[ROUNDS / 2] + " ns/op");
    }

    public static void main(String[] args) {
        time("loop-only", L7W95MethodEntryWordBench::loopOnly);
        time("static", L7W95MethodEntryWordBench::statics);
        time("virtual", L7W95MethodEntryWordBench::virtuals);
        time("interface", L7W95MethodEntryWordBench::interfaces);
        time("recursive", L7W95MethodEntryWordBench::recursive);
        time("sync-static", L7W95MethodEntryWordBench::syncStatics);
    }
}
