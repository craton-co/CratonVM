// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gcj j2/rf2: the probe of
// docs/internal/gc/gch-h4wr-itl-pending-old-value-of-a-left-out-young-child-is-rooted-after-its-block-was-freed-RETIRED-UNFIXED-20261018.md
//
// The h4wr window, built on purpose: a never-started child Thread C is YOUNG
// and named only by a dead OLD holder array (its card keeps C through the
// young phase), while C's inherited value V is OLD (childValue hands out a
// pre-tenured array from a pool that is then dropped). A true-root major
// leaves C out of its trace and frees V; until the next young collection
// frees C, a scan that ROOTS the pending values instead of pinning them (a
// concurrent cycle's initial mark, opened by the 1% start trigger meanwhile)
// would hand V's freed block back as a root. The gch h4/wr filter
// (`native_roots.rs` `scan_inherited_tl_pending`) drops such a value; with
// CRATONVM_DBG_WATCHREF=1 every drop prints `[watchref] itl-pending: dropped`
// on stderr, which is the positive control that the window formed.
//
// Java-visible: every value is cleared once its child is unreachable, on
// HotSpot and here (the defect is a stale root, not a kept value), and the
// values a live child holds keep their content.
//
// stdout (deterministic; HotSpot 25 -XX:+UseSerialGC), for `<rounds>`:
//   threads=<50*rounds> childValue_calls=<50*rounds>
//   kept-intact=<rounds>/<rounds>
//   cleared=<49*rounds>/<49*rounds>   (each round keeps one child live)
//   PROBE-OK
// stderr: diagnostics.
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.locks.LockSupport;

public class Gcj2Rf2ItlFreedOldValueProbe {
    static final int N = 50;
    static final int VALUE_BYTES = 4096;

    static byte[][] pool;
    static int next;
    static Object[] holder;
    static volatile Object sink;
    static final List<WeakReference<byte[]>> VALUES = new ArrayList<>();
    /** One live child per round, kept to the end: its value must stay intact. */
    static final List<Thread> KEPT = new ArrayList<>();
    static final List<byte[]> KEPT_VALUES = new ArrayList<>();

    static final InheritableThreadLocal<byte[]> ITL = new InheritableThreadLocal<>() {
        @Override
        protected byte[] childValue(byte[] parent) {
            byte[] v = pool[next];
            pool[next] = null;
            next++;
            synchronized (VALUES) {
                VALUES.add(new WeakReference<>(v));
            }
            return v;
        }
    };

    static void fill(byte[] v, int tag) {
        for (int i = 0; i < v.length; i++) {
            v[i] = (byte) (tag + i * 7);
        }
    }

    static boolean intact(byte[] v, int tag) {
        if (v == null || v.length != VALUE_BYTES) {
            return false;
        }
        for (int i = 0; i < v.length; i++) {
            if (v[i] != (byte) (tag + i * 7)) {
                return false;
            }
        }
        return true;
    }

    static int live() {
        int n = 0;
        synchronized (VALUES) {
            for (WeakReference<byte[]> r : VALUES) {
                if (r.get() != null) {
                    n++;
                }
            }
        }
        return n;
    }

    /** Some young garbage and a little time: the concurrent cycle's window. */
    static void window() {
        for (int k = 0; k < 20; k++) {
            Object[] ring = new Object[64];
            for (int i = 0; i < 256; i++) {
                ring[i & 63] = new byte[512];
            }
            sink = ring;
            LockSupport.parkNanos(5_000_000L);
        }
        sink = null;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        ITL.set(new byte[16]);
        List<Integer> keptTags = new ArrayList<>();
        for (int round = 0; round < rounds; round++) {
            // The values, tenured while the pool holds them.
            pool = new byte[N][];
            for (int i = 0; i < N; i++) {
                pool[i] = new byte[VALUE_BYTES];
                fill(pool[i], round * N + i);
            }
            next = 0;
            for (int g = 0; g < 3; g++) {
                System.gc();
            }
            // The holder, tenured empty.
            holder = new Object[N];
            for (int g = 0; g < 3; g++) {
                System.gc();
            }
            // The children: young, named only by the old holder.
            for (int i = 0; i < N; i++) {
                holder[i] = new Thread(() -> { });
            }
            // One child stays live (a strong root): its old value must survive.
            KEPT.add((Thread) holder[0]);
            keptTags.add(round * N);
            synchronized (VALUES) {
                KEPT_VALUES.add(VALUES.get(VALUES.size() - N).get());
            }
            pool = null;
            holder = null;
            System.gc();
            window();
        }
        int calls;
        synchronized (VALUES) {
            calls = VALUES.size();
        }
        int intactKept = 0;
        for (int r = 0; r < rounds; r++) {
            if (intact(KEPT_VALUES.get(r), keptTags.get(r))) {
                intactKept++;
            }
        }
        // The live children's values are strongly held by KEPT_VALUES too, so
        // only the dropped children's values are expected to clear.
        int expectDead = calls - rounds;
        long start = System.nanoTime();
        int gcCalls = 0;
        while (live() > rounds && System.nanoTime() - start < 30_000_000_000L) {
            System.gc();
            gcCalls++;
            LockSupport.parkNanos(20_000_000L);
        }
        int cleared = calls - live();
        System.err.println("[gcj2rf2-h4wr] gcCalls=" + gcCalls + " kept=" + KEPT.size());
        System.out.println("threads=" + (N * rounds) + " childValue_calls=" + calls);
        System.out.println("kept-intact=" + intactKept + "/" + rounds);
        System.out.println("cleared=" + cleared + "/" + expectDead);
        boolean ok = calls == N * rounds && intactKept == rounds && cleared == expectDead;
        System.out.println(ok ? "PROBE-OK" : "PROBE-FAIL");
    }
}
