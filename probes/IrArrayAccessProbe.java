// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// COV-02 (docs/known-issues/c2/cov-02-array-element-access.md) — the E2E half
// of the array-element differential.
//
// jit/tests/ir_vs_singlepass.rs proves the two backends emit the same answer
// for `iaload`/`baload`/`caload`/`saload`/`aaload`/`arraylength` and the
// integral stores over SYNTHETIC array buffers. Two things that harness cannot
// observe, and this probe is for both:
//
//   1. REAL heap arrays, allocated by this VM's allocator, read through the
//      real object header — not a `Vec<u8>` shaped like one.
//   2. `aaload` under a COLLECTING heap. The loaded element is a reference, and
//      the IR node is `IrType::Ref` precisely so `emit_safepoint_map` publishes
//      its spill slot as a rewritable root. An element that is live only in a
//      compiled frame's spill slot, across an allocation that triggers a young
//      collection, is the shape that goes wrong when it is not.
//
// Every check is an IDENTITY or VALUE assertion with a host-computed answer, so
// it is equally runnable on HotSpot — which is the control. Run both:
//
//   java probes/IrArrayAccessProbe.java
//   cratonvm --Xmx 64m probes/IrArrayAccessProbe.java
//
// and, to confirm the methods actually reached the optimizing tier rather than
// passing on the interpreter:
//
//   CRATONVM_DBG=ir-compiles cratonvm --Xmx 64m probes/IrArrayAccessProbe.java \
//     2>&1 | grep 'produced a body for IrArrayAccessProbe'
//
// A run that prints PASS while that grep is empty has proved nothing about this
// lane — the whole point is the compiled body.
public class IrArrayAccessProbe {

    // Warm enough to tier up, and each kernel is a separate method so a refusal
    // shows up as one missing line in the ir-compiles log rather than silently
    // dragging the others down with it.
    private static final int WARM = 60_000;

    private static int failures = 0;

    private static void check(String what, long got, long want) {
        if (got != want) {
            System.out.println("FAIL " + what + ": got " + got + " want " + want);
            failures++;
        }
    }

    private static void checkSame(String what, Object got, Object want) {
        if (got != want) {
            System.out.println("FAIL " + what + ": identity mismatch");
            failures++;
        }
    }

    // ── the kernels ────────────────────────────────────────────────────────

    static int iaload(int[] a, int i) {
        return a[i];
    }

    static int baload(byte[] a, int i) {
        return a[i];
    }

    static int caload(char[] a, int i) {
        return a[i];
    }

    static int saload(short[] a, int i) {
        return a[i];
    }

    static long laload(long[] a, int i) {
        return a[i];
    }

    static Object aaload(Object[] a, int i) {
        return a[i];
    }

    static int arraylength(int[] a) {
        return a.length;
    }

    static void iastore(int[] a, int i, int v) {
        a[i] = v;
    }

    static void bastore(byte[] a, int i, int v) {
        a[i] = (byte) v;
    }

    static void castore(char[] a, int i, int v) {
        a[i] = (char) v;
    }

    static void sastore(short[] a, int i, int v) {
        a[i] = (short) v;
    }

    // `a[i] + a.length` — the element read is in the memory-token chain and the
    // length read is not, so a scheduler that reordered them shows up here.
    static int elemPlusLen(int[] a, int i) {
        return a[i] + a.length;
    }

    // dup_x1, as javac emits it for a compound assignment on an array element:
    //   aload a; iload i; dup2; iaload; iadd; iastore  — actually dup2, so force
    // the real thing with an explicit swap-shaped expression instead. `b - (a-b)`
    // is what the bytecode `iload_0 iload_1 dup_x1 isub isub` computes.
    static int dupX1Shape(int a, int b) {
        return b - (a - b);
    }

    // ── the moving-GC case ─────────────────────────────────────────────────
    //
    // `aaload` an element, then allocate hard enough to force a young
    // collection while that element is live ONLY in the compiled frame, then
    // use it. If the element's spill slot is not published as a root, a
    // relocating collector leaves a stale pointer behind and the identity check
    // (or the field read) fails — usually as a crash rather than a mismatch.
    static Object aaloadAcrossGc(Object[] a, int i, int churn) {
        Object e = a[i];
        long sink = 0;
        for (int k = 0; k < churn; k++) {
            byte[] garbage = new byte[256];
            garbage[0] = (byte) k;
            sink += garbage[0];
        }
        if (sink == Long.MIN_VALUE) {
            // Unreachable; keeps `sink` from being optimized away.
            System.out.println("impossible " + sink);
        }
        return e;
    }

    public static void main(String[] args) {
        int[] ints = { 7, -1, Integer.MAX_VALUE, Integer.MIN_VALUE, 0 };
        byte[] bytes = { 0, 1, 127, (byte) 0x80, (byte) 0xff };
        char[] chars = { 0, 'A', 0x7fff, 0x8000, 0xffff };
        short[] shorts = { 0, 65, 32767, (short) 0x8000, (short) 0xffff };
        long[] longs = { 0L, 1L, Long.MAX_VALUE, Long.MIN_VALUE, -1L };

        String[] refs = new String[5];
        for (int i = 0; i < refs.length; i++) {
            // `new String` (not a literal) so each element is a distinct
            // relocatable object rather than an interned constant.
            refs[i] = new String("elem" + i);
        }

        for (int n = 0; n < WARM; n++) {
            for (int i = 0; i < 5; i++) {
                check("iaload", iaload(ints, i), ints[i]);
                check("baload", baload(bytes, i), bytes[i]);
                check("caload", caload(chars, i), chars[i]);
                check("saload", saload(shorts, i), shorts[i]);
                check("laload", laload(longs, i), longs[i]);
                checkSame("aaload", aaload(refs, i), refs[i]);
                check("elemPlusLen", elemPlusLen(ints, i), ints[i] + ints.length);
            }
            check("arraylength", arraylength(ints), 5);
            check("dup_x1", dupX1Shape(10, 3), 3 - (10 - 3));
            if (failures > 0) {
                break;
            }
        }

        // Stores, into a scratch array so the read-back arrays above stay
        // intact. Each iteration writes and reads back through the compiled
        // accessors, so a truncation bug shows as a value mismatch.
        int[] si = new int[5];
        byte[] sb = new byte[5];
        char[] sc = new char[5];
        short[] ss = new short[5];
        for (int n = 0; n < WARM && failures == 0; n++) {
            iastore(si, n % 5, n);
            check("iastore", iaload(si, n % 5), n);
            bastore(sb, n % 5, n);
            check("bastore", baload(sb, n % 5), (byte) n);
            castore(sc, n % 5, n);
            check("castore", caload(sc, n % 5), (char) n);
            sastore(ss, n % 5, n);
            check("sastore", saload(ss, n % 5), (short) n);
        }

        // The exception cases. Both backends must throw the real Java exception
        // at the real bci, which means it must be catchable HERE.
        try {
            iaload(ints, 5);
            System.out.println("FAIL iaload at length: no exception");
            failures++;
        } catch (ArrayIndexOutOfBoundsException expected) {
            // ok
        }
        try {
            iaload(ints, -1);
            System.out.println("FAIL iaload negative: no exception");
            failures++;
        } catch (ArrayIndexOutOfBoundsException expected) {
            // ok
        }
        try {
            iaload(null, 0);
            System.out.println("FAIL iaload null: no exception");
            failures++;
        } catch (NullPointerException expected) {
            // ok
        }
        try {
            iastore(ints, 5, 1);
            System.out.println("FAIL iastore at length: no exception");
            failures++;
        } catch (ArrayIndexOutOfBoundsException expected) {
            // ok
        }
        try {
            iastore(null, 0, 1);
            System.out.println("FAIL iastore null: no exception");
            failures++;
        } catch (NullPointerException expected) {
            // ok
        }
        try {
            arraylength(null);
            System.out.println("FAIL arraylength null: no exception");
            failures++;
        } catch (NullPointerException expected) {
            // ok
        }
        try {
            aaload(refs, 99);
            System.out.println("FAIL aaload out of range: no exception");
            failures++;
        } catch (ArrayIndexOutOfBoundsException expected) {
            // ok
        }

        // The array is still intact after every fault — a guard that ran the
        // access anyway, or a deopt that resumed at the wrong bci, shows here.
        for (int i = 0; i < 5; i++) {
            check("post-fault iaload", iaload(ints, i), ints[i]);
        }

        // Moving GC. Enough churn per call to force several young collections,
        // repeated so the compiled body is the one running.
        for (int n = 0; n < 2_000 && failures == 0; n++) {
            int i = n % refs.length;
            checkSame("aaloadAcrossGc", aaloadAcrossGc(refs, i, 400), refs[i]);
            // Read through the returned reference too: a stale pointer that
            // happens to compare equal would still have the wrong contents.
            String s = (String) aaloadAcrossGc(refs, i, 400);
            check("aaloadAcrossGc content", s.length(), refs[i].length());
            if (!s.equals("elem" + i)) {
                System.out.println("FAIL aaloadAcrossGc content: " + s);
                failures++;
            }
        }

        System.out.println(failures == 0 ? "PASS" : ("FAIL " + failures));
        if (failures != 0) {
            System.exit(1);
        }
    }
}
