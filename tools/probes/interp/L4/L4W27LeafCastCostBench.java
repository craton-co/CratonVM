// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 27, lane L4: the per-call price of the reference
// cast a LEAF method handle now makes (`native-builtins/src/lang_invoke.rs`
// `leaf_reference_refusal` / `leaf_receiver_refusal`; the correctness rows are
// `L4W27AdapterLeafReferenceCasts.java`).
//
//   exact-virtual   - `findVirtual(String, length)` called exactly on a String:
//                     the receiver is the handle's class (fast path: one
//                     read-locked class-name compare)
//   iface-virtual   - `findVirtual(CharSequence, length)` on a String: an
//                     interface receiver type (one `class_id_by_name` + one
//                     `is_subclass` more)
//   insert-exact    - `insertArguments(pair, 0, 1)` with a String for a
//                     String parameter (fast path)
//   insert-iface    - `insertArguments(pairSeq, 0, 1)` with a String for a
//                     CharSequence parameter (the subclass/interface path)
//   insert-object   - the same with an Object parameter (not judged at all:
//                     the control)
//
// How to run (each row's loop runs in ONE invocation; ns/call on stderr,
// a deterministic checksum per row on stdout that must equal HotSpot's):
//
//   cratonvm --java-home <jdk25> [--nojit] -cp <dir> L4W27LeafCastCostBench
//
// A/B: interleave against wave 26 (`faa212874`), medians of 3+. Expected:
// exact-virtual, insert-exact and insert-object flat (within noise);
// iface-virtual and insert-iface up by at most the price of two class-manager
// read locks per call -- small against the rest of a method-handle dispatch.
// If an iface row moves by more than ~10%, the slow path wants a memo.
//
// HotSpot 25 (25.0.3) prints on stdout:
//   exact-virtual 1000000
//   iface-virtual 1000000
//   insert-exact 600000
//   insert-iface 600000
//   insert-object 600000
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W27LeafCastCostBench {
    static final int N = 200_000;

    static int pair(int a, String s) {
        return a + s.length();
    }

    static int pairSeq(int a, CharSequence s) {
        return a + s.length();
    }

    static int pairObj(int a, Object s) {
        return a + 2;
    }

    interface Row {
        long run(int n) throws Throwable;
    }

    static void row(String name, Row r) throws Throwable {
        r.run(N / 10); // warm the handles and the call site
        long t0 = System.nanoTime();
        long sum = r.run(N);
        long t1 = System.nanoTime();
        System.out.println(name + " " + sum);
        System.err.println(name + " " + ((t1 - t0) / N) + " ns/call");
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle len = l.findVirtual(String.class, "length", MethodType.methodType(int.class));
        MethodHandle seqLen = l.findVirtual(CharSequence.class, "length", MethodType.methodType(int.class));
        MethodHandle ins = MethodHandles.insertArguments(
                l.findStatic(L4W27LeafCastCostBench.class, "pair",
                        MethodType.methodType(int.class, int.class, String.class)), 0, 1);
        MethodHandle insSeq = MethodHandles.insertArguments(
                l.findStatic(L4W27LeafCastCostBench.class, "pairSeq",
                        MethodType.methodType(int.class, int.class, CharSequence.class)), 0, 1);
        MethodHandle insObj = MethodHandles.insertArguments(
                l.findStatic(L4W27LeafCastCostBench.class, "pairObj",
                        MethodType.methodType(int.class, int.class, Object.class)), 0, 1);
        String s = "abcde";
        CharSequence cs = s;
        row("exact-virtual", n -> {
            long t = 0;
            for (int i = 0; i < n; i++) {
                t += (int) len.invokeExact(s);
            }
            return t;
        });
        row("iface-virtual", n -> {
            long t = 0;
            for (int i = 0; i < n; i++) {
                t += (int) seqLen.invokeExact(cs);
            }
            return t;
        });
        row("insert-exact", n -> {
            long t = 0;
            for (int i = 0; i < n; i++) {
                t += (int) ins.invokeExact("ab");
            }
            return t;
        });
        row("insert-iface", n -> {
            long t = 0;
            for (int i = 0; i < n; i++) {
                t += (int) insSeq.invokeExact((CharSequence) "ab");
            }
            return t;
        });
        row("insert-object", n -> {
            long t = 0;
            for (int i = 0; i < n; i++) {
                t += (int) insObj.invokeExact((Object) "ab");
            }
            return t;
        });
    }
}
