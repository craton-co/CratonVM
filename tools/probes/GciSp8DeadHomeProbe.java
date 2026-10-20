// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;

/**
 * gci i8/sp (2026-10-13): does a SINGLE-PASS compiled frame keep a dead
 * reference home live across a GC point that HotSpot's compiled frame frees?
 * Items 5(b) and 5(c) of
 * {@code docs/known-issues/gc/gengc-r5w5-oomjit9-ir-dead-homes-residuals-20260927.md}.
 *
 * <p>One case per process (first argument), so no case inherits another's
 * stack leftovers:
 * <ul>
 *   <li>{@code dead-local} -- 5(b): {@code deadLocal} holds its 16 MB
 *       {@code long[] a} in a local that is a reference on every path (the
 *       single-pass map's "must be an oop" set names it) but dead by liveness
 *       at the call to {@code cleared}, which collects. No door holds it:
 *       the judged caller is reached from the interpreted {@code judge} with
 *       primitive arguments only, and {@code wrap} / {@code cleared} are
 *       {@code synchronized}, so the single-pass tier does not splice them.</li>
 *   <li>{@code dead-local-spliced} -- 5(b) at a GC point INSIDE a splice:
 *       the same shape, but the collecting call is made by {@code gcVia}, a
 *       small static the single-pass tier splices into the caller.
 *       {@code CRATONVM_JIT_SP_MAP_LOCAL_LIVENESS} is refused inside a splice;
 *       {@code CRATONVM_JIT_SP_MAP_LOCAL_LIVENESS_IN_SPLICES} lifts that.</li>
 *   <li>{@code leftover} -- 5(c): {@code spray} copies the array into 24
 *       locals that are live together (most get frame homes) and returns; the
 *       array is then reachable from nothing but a {@link WeakReference}.
 *       {@code leftoverCheck}'s fresh frame, at the same stack addresses,
 *       splices {@code gcVia}, whose reserved-but-unwritten merge words lie
 *       below the live cursor at the nested collecting call. If one of them
 *       overlays a word {@code spray} wrote, the band scan roots the array
 *       ({@code CRATONVM_JIT_SP_ZERO_SPILL_BAND} zeroes the band at entry).
 *       Whether the words overlay is a layout fact, so a PASS of the
 *       single-pass arm is "no population at this layout", not a verdict.</li>
 * </ul>
 *
 * <p>HotSpot 25 ({@code java -XX:+UseSerialGC -Xmx64m GciSp8DeadHomeProbe <case>})
 * prints {@code PASS <case>} then {@code PASS all 1} (with {@code all}: the
 * three cases and {@code PASS all 3}) and exits 0: its compiled frames' oop
 * maps come from liveness (C1 {@code LinearScan}, C2 dead-local kills), and a
 * returned frame's words are never roots. Otherwise {@code FAIL <n> of <m>},
 * exit 1. Every caller is warmed with empty arrays and no collection, then
 * the process sleeps 2 s and warms again so background compiles land before
 * the judged call. Diagnostics go to stderr only.
 *
 * <p>Arms (rows in {@code docs/internal/gc-round-gci-20261011/vi8-sp.list}):
 * the single-pass arm adds
 * {@code CRATONVM_JIT_IR_CALL=0 CRATONVM_JIT_IR_CALL_SPECIAL=0 CRATONVM_JIT_IR_CALL_VIRTUAL=0},
 * which keeps every method here (each makes a call) out of the optimizing
 * tier, as {@code tools/bench/Gcd1ArgPinSpecialProbe.java}'s gating arm does.
 */
public final class GciSp8DeadHomeProbe {
    /** 16 MB of {@code long}. */
    static final int BIG = 2 * 1024 * 1024;

    static int failures;
    static int cases;
    static long sink;
    static WeakReference<long[]> weak;

    /** Collects until the referent goes (at most three times). Synchronized: never spliced. */
    static synchronized boolean cleared(WeakReference<?> ref, boolean check) {
        if (!check) {
            return true;
        }
        for (int i = 0; i < 3 && ref.get() != null; i++) {
            System.gc();
        }
        return ref.get() == null;
    }

    /** Synchronized: never spliced, so the caller's {@code a} stays in the caller's own home. */
    static synchronized WeakReference<long[]> wrap(long[] a) {
        return new WeakReference<>(a);
    }

    /** Small and static: the single-pass tier splices it; its call to {@code cleared} is the GC point. */
    static boolean gcVia(WeakReference<?> ref, boolean check) {
        return cleared(ref, check);
    }

    /** 5(b): {@code a} is a reference on every path and dead at the {@code cleared} call. */
    static boolean deadLocal(int n, boolean check) {
        long[] a = new long[n];
        WeakReference<long[]> ref = wrap(a);
        sink += a.length;
        return cleared(ref, check);
    }

    /** 5(b) inside a splice: the collecting call is {@code gcVia}'s. */
    static boolean deadLocalSpliced(int n, boolean check) {
        long[] a = new long[n];
        WeakReference<long[]> ref = wrap(a);
        sink += a.length;
        return gcVia(ref, check);
    }

    /** 5(c), part 1: leaves copies of the array in its (returned) frame. */
    static long spray(int n) {
        long[] a = new long[n];
        weak = new WeakReference<>(a);
        Object a01 = a, a02 = a, a03 = a, a04 = a, a05 = a, a06 = a, a07 = a, a08 = a;
        Object a09 = a, a10 = a, a11 = a, a12 = a, a13 = a, a14 = a, a15 = a, a16 = a;
        Object a17 = a, a18 = a, a19 = a, a20 = a, a21 = a, a22 = a, a23 = a, a24 = a;
        // Every copy is read here, so all 24 are live together.
        long same = n;
        same += a01 == a02 ? 1 : 0;
        same += a03 == a04 ? 1 : 0;
        same += a05 == a06 ? 1 : 0;
        same += a07 == a08 ? 1 : 0;
        same += a09 == a10 ? 1 : 0;
        same += a11 == a12 ? 1 : 0;
        same += a13 == a14 ? 1 : 0;
        same += a15 == a16 ? 1 : 0;
        same += a17 == a18 ? 1 : 0;
        same += a19 == a20 ? 1 : 0;
        same += a21 == a22 ? 1 : 0;
        same += a23 == a24 ? 1 : 0;
        return same;
    }

    /** 5(c), part 2: a fresh frame at {@code spray}'s addresses whose splice collects. */
    static boolean leftoverCheck(boolean check) {
        return gcVia(weak, check);
    }

    static void warm(String which) {
        for (int i = 0; i < 20_000; i++) {
            switch (which) {
                case "dead-local":
                    sink += deadLocal(0, false) ? 1 : 0;
                    break;
                case "dead-local-spliced":
                    sink += deadLocalSpliced(0, false) ? 1 : 0;
                    break;
                case "leftover":
                    sink += spray(0);
                    sink += leftoverCheck(false) ? 1 : 0;
                    break;
                default:
                    sink += deadLocal(0, false) ? 1 : 0;
                    sink += deadLocalSpliced(0, false) ? 1 : 0;
                    sink += spray(0);
                    sink += leftoverCheck(false) ? 1 : 0;
                    break;
            }
        }
    }

    static void verdict(String name, boolean ok) {
        cases++;
        if (!ok) {
            failures++;
        }
        System.out.println((ok ? "PASS " : "FAIL ") + name);
    }

    /** Called once, so it runs interpreted: the judged bodies are entered with primitive arguments only. */
    static void judge(String which) {
        try {
            switch (which) {
                case "dead-local":
                    verdict(which, deadLocal(BIG, true));
                    break;
                case "dead-local-spliced":
                    verdict(which, deadLocalSpliced(BIG, true));
                    break;
                case "leftover":
                    sink += spray(BIG);
                    verdict(which, leftoverCheck(true));
                    break;
                default:
                    break;
            }
        } catch (OutOfMemoryError again) {
            verdict(which, false);
        }
    }

    public static void main(String[] args) {
        final String which = args.length > 0 ? args[0] : "all";
        if (!which.equals("all") && !which.equals("dead-local")
                && !which.equals("dead-local-spliced") && !which.equals("leftover")) {
            System.out.println("usage: GciSp8DeadHomeProbe dead-local|dead-local-spliced|leftover|all");
            System.exit(2);
        }
        warm(which);
        try {
            Thread.sleep(2000);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        warm(which);
        if (which.equals("all")) {
            judge("dead-local");
            judge("dead-local-spliced");
            judge("leftover");
        } else {
            judge(which);
        }
        System.err.println("[sp8] sink=" + sink);
        if (failures == 0) {
            System.out.println("PASS all " + cases);
        } else {
            System.out.println("FAIL " + failures + " of " + cases);
            System.exit(1);
        }
    }
}
