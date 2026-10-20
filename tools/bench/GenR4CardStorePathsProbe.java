// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Field;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicReferenceArray;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;

/**
 * Generational GC round 4, lane "cards": every NON-putfield reference-store
 * path into a TENURED holder, each followed by minor collections that must
 * find the old-to-young edge through the card table.
 *
 * {@code OldToYoungEdgeProbe} covers plain putfield/aastore. This covers the
 * paths that reach the heap through a different API and therefore through a
 * different barrier call site: VarHandle field set / CAS, the field updater,
 * reflection, AtomicReferenceArray, VarHandle array elements,
 * System.arraycopy into an old destination, Arrays.fill, and sun.misc.Unsafe
 * putObjectVolatile / compareAndSwapObject. A missed card on any of them shows
 * up as a mismatch (or a ClassCastException / NPE), and the path is named.
 *
 * The round-4 audit found no missed barrier on these paths (each routes to
 * GenerationalHeap::write_barrier); this probe pins that, and it also
 * exercises the round-4 self-maintained card-scan bound: the holders sit low
 * in a large old generation, so the bound is far below capacity.
 *
 * Usage: GenR4CardStorePathsProbe [holders] [rounds] [churnDepth]
 *   holders     tenured holders / slots per path (default 4000)
 *   rounds      store+verify passes (default 40)
 *   churnDepth  garbage tree depth built between store and verify (default 13)
 *
 * The last line is deterministic and HotSpot prints the same one:
 *   paths=10 holders=H rounds=R mismatches=0 checksum=C
 *
 * Run (the rset verifier must report edges > 0, or the run proves nothing):
 *   CRATONVM_GC_VERIFY_RSET=1 cratonvm --java-home "$JDK" \
 *       -XX:+UseGenerationalGC -Xmx512m -c tools/bench GenR4CardStorePathsProbe
 *   java -cp tools/bench GenR4CardStorePathsProbe        # oracle
 */
public final class GenR4CardStorePathsProbe {
    static final class Payload {
        final long stamp;
        Payload(long stamp) { this.stamp = stamp; }
    }

    static final class Holder {
        volatile Object f;
        Object g;
        volatile Object h;
    }

    static final class Tree {
        Tree a, b;
        int v;
        Tree(int v) { this.v = v; }
    }

    static final int PATHS = 10;
    static final String[] NAMES = {
        "varhandle-setVolatile", "varhandle-cas", "field-updater-getAndSet",
        "reflect-Field.set", "AtomicReferenceArray.set", "varhandle-array-setRelease",
        "System.arraycopy", "Arrays.fill", "Unsafe.putObjectVolatile",
        "Unsafe.compareAndSwapObject",
    };

    static final VarHandle VH_F;
    static final VarHandle VH_ARR = MethodHandles.arrayElementVarHandle(Object[].class);
    static final AtomicReferenceFieldUpdater<Holder, Object> UPD =
            AtomicReferenceFieldUpdater.newUpdater(Holder.class, Object.class, "f");
    static final Field REFLECT_G;
    static final sun.misc.Unsafe U;
    static final long H_OFFSET;
    static final long ARR_BASE;
    static final long ARR_SCALE;

    static {
        try {
            VH_F = MethodHandles.lookup().findVarHandle(Holder.class, "f", Object.class);
            REFLECT_G = Holder.class.getDeclaredField("g");
            Field theUnsafe = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
            theUnsafe.setAccessible(true);
            U = (sun.misc.Unsafe) theUnsafe.get(null);
            H_OFFSET = U.objectFieldOffset(Holder.class.getDeclaredField("h"));
            ARR_BASE = U.arrayBaseOffset(Object[].class);
            ARR_SCALE = U.arrayIndexScale(Object[].class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static Tree build(int depth, int v) {
        Tree t = new Tree(v);
        if (depth > 0) {
            t.a = build(depth - 1, v * 2);
            t.b = build(depth - 1, v * 2 + 1);
        }
        return t;
    }

    static long sum(Tree t) {
        return t == null ? 0 : t.v + sum(t.a) + sum(t.b);
    }

    static long stampFor(int path, int slot, int round) {
        return ((long) path << 40) ^ ((long) slot << 16) ^ (round * 2654435761L);
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        int churnDepth = args.length > 2 ? Integer.parseInt(args[2]) : 13;

        // Tenured side: one holder array per field path, one Object[] per
        // array path. Held for the whole run; the warm-up churn promotes them.
        Holder[][] holders = new Holder[PATHS][];
        for (int p = 0; p < PATHS; p++) {
            holders[p] = new Holder[n];
            for (int i = 0; i < n; i++) {
                holders[p][i] = new Holder();
            }
        }
        AtomicReferenceArray<Object> ara = new AtomicReferenceArray<>(n);
        Object[] vhArr = new Object[n];
        Object[] copyDst = new Object[n];
        Object[] fillDst = new Object[n];
        Object[] casArr = new Object[n];

        long sink = 0;
        for (int w = 0; w < 6; w++) {
            sink += sum(build(churnDepth, w));
        }

        long checksum = 0;
        long mismatches = 0;
        for (int r = 0; r < rounds; r++) {
            Object[] stage = new Object[n];
            for (int i = 0; i < n; i++) {
                VH_F.setVolatile(holders[0][i], new Payload(stampFor(0, i, r)));
                Object cur = VH_F.getVolatile(holders[1][i]);
                if (!VH_F.compareAndSet(holders[1][i], cur, new Payload(stampFor(1, i, r)))) {
                    mismatches++;
                }
                UPD.getAndSet(holders[2][i], new Payload(stampFor(2, i, r)));
                REFLECT_G.set(holders[3][i], new Payload(stampFor(3, i, r)));
                ara.set(i, new Payload(stampFor(4, i, r)));
                VH_ARR.setRelease(vhArr, i, new Payload(stampFor(5, i, r)));
                stage[i] = new Payload(stampFor(6, i, r));
                U.putObjectVolatile(holders[8][i], H_OFFSET, new Payload(stampFor(8, i, r)));
                long off = ARR_BASE + (long) i * ARR_SCALE;
                Object old = U.getObjectVolatile(casArr, off);
                if (!U.compareAndSwapObject(casArr, off, old, new Payload(stampFor(9, i, r)))) {
                    mismatches++;
                }
            }
            System.arraycopy(stage, 0, copyDst, 0, n);
            // Arrays.fill with ONE young payload per 64-slot block, so the
            // expected stamp is per block.
            for (int b = 0; b < n; b += 64) {
                Arrays.fill(fillDst, b, Math.min(n, b + 64), new Payload(stampFor(7, b, r)));
            }
            stage = null;

            // Minor collections happen here; the only path to every payload
            // stored above is an old-to-young edge.
            sink += sum(build(churnDepth, r));

            for (int i = 0; i < n; i++) {
                Object[] got = {
                    holders[0][i].f, holders[1][i].f, holders[2][i].f, holders[3][i].g,
                    ara.get(i), vhArr[i], copyDst[i], fillDst[i], holders[8][i].h, casArr[i],
                };
                for (int p = 0; p < PATHS; p++) {
                    long want = p == 7 ? stampFor(7, i - (i % 64), r) : stampFor(p, i, r);
                    Object o = got[p];
                    if (!(o instanceof Payload) || ((Payload) o).stamp != want) {
                        if (mismatches < 10) {
                            System.out.println("MISMATCH path=" + NAMES[p] + " slot=" + i
                                    + " round=" + r + " got=" + (o == null ? "null"
                                    : o instanceof Payload ? Long.toString(((Payload) o).stamp)
                                    : o.getClass().getName()));
                        }
                        mismatches++;
                    } else {
                        checksum += want * 31 + p;
                    }
                }
            }
        }
        if (sink == 42) {
            System.out.println("unreachable");
        }
        System.out.println("paths=" + PATHS + " holders=" + n + " rounds=" + rounds
                + " mismatches=" + mismatches + " checksum=" + checksum);
    }
}
