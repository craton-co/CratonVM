// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

/**
 * gci i2/wr regression probe for the address-memo epoch's VM-side consumers:
 * the JIT helpers' memos ({@code mh_constant_bind_redirect}, the VarHandle
 * primitive-read proofs, {@code direct_receiver_facts}) and the MethodHandle
 * constant rows of {@code jit_bridge.rs}, which since gci i2/wr all key on
 * {@code HeapRealm::address_epoch} (collections, G1 cleanup, and the
 * Generational concurrent sweep's slices).
 *
 * A hot method calls {@code static final} handles ({@code invokeExact} of a
 * static and of a virtual method, a {@code VarHandle} int read) over fresh
 * receivers every round, while the round churns young objects, allocates large
 * arrays (placed in the old generation directly) and drops them, and collects
 * every 50 rounds, so receiver addresses are freed and re-used while the memos
 * are warm. A memo answering for a dead receiver shows as {@code bad} above 0
 * or a different sum. stdout is deterministic (HotSpot 25 is the oracle);
 * nothing goes to stderr.
 */
public class GciWr2EpochConsumersProbe {
    static final class Box {
        int v;

        Box(int v) {
            this.v = v;
        }

        int get() {
            return v;
        }
    }

    static int inc(int x) {
        return x + 1;
    }

    static final MethodHandle MH_INC;
    static final MethodHandle MH_GET;
    static final VarHandle VH_V;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            MH_INC = l.findStatic(GciWr2EpochConsumersProbe.class, "inc",
                    MethodType.methodType(int.class, int.class));
            MH_GET = l.findVirtual(Box.class, "get", MethodType.methodType(int.class));
            VH_V = l.findVarHandle(Box.class, "v", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static Object sink;
    static Object oldSink;
    static int bad;

    static long work(Box[] boxes, int r) throws Throwable {
        long sum = 0;
        for (int i = 0; i < boxes.length; i++) {
            Box b = boxes[i];
            int want = r * 7 + i;
            int viaGet = (int) MH_GET.invokeExact(b);
            int viaVh = (int) VH_V.get(b);
            int viaInc = (int) MH_INC.invokeExact(viaGet);
            if (viaGet != want || viaVh != want || viaInc != want + 1) {
                bad++;
            }
            sum += viaGet + viaVh + viaInc;
        }
        return sum;
    }

    public static void main(String[] args) throws Throwable {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 2000;
        int width = args.length > 1 ? Integer.parseInt(args[1]) : 64;
        long total = 0;
        for (int r = 0; r < rounds; r++) {
            Box[] boxes = new Box[width];
            for (int i = 0; i < width; i++) {
                boxes[i] = new Box(r * 7 + i);
            }
            total += work(boxes, r);
            byte[][] junk = new byte[32][];
            for (int k = 0; k < junk.length; k++) {
                junk[k] = new byte[512];
            }
            sink = junk;
            if (r % 4 == 0) {
                // Large enough for a direct old-generation placement; the
                // previous one dies here.
                oldSink = new long[64 * 1024];
            }
            if (r % 50 == 49) {
                System.gc();
            }
        }
        System.out.println("epoch-consumers: rounds=" + rounds + " width=" + width
                + " total=" + total + " bad=" + bad);
    }
}
