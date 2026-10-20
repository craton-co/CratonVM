// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.ArrayList;
import java.util.List;

/**
 * SBR round 5, lane avh: the array-element var handle's access across garbage collections,
 * across handles of different layouts (one filed verdict per handle), a user's own
 * coordinate filter (must NOT be served as a layout scale), a second thread, and the
 * accesses that must keep failing the way HotSpot fails them once the fast road is warm.
 * Prints one line per check; diff against HotSpot 25.
 */
public class Sbr5AvhArrayVhGc {
    static void line(String name, Object v) { System.out.println(name + " -> " + v); }

    static void churn() {
        List<byte[]> junk = new ArrayList<>();
        for (int i = 0; i < 3000; i++) junk.add(new byte[1024]);
        System.gc();
    }

    static Object err(Runnable r) {
        try { r.run(); return "no exception"; } catch (Throwable t) { return t.getClass().getName() + ": " + String.valueOf(t.getMessage()).replaceAll("0x[0-9a-f]+", "ADDR"); }
    }

    static long userScale(long offset, long index) { return offset + index * 16 + 4; }

    public static void main(String[] args) throws Throwable {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(4096, 8);
            VarHandle vi = ValueLayout.JAVA_INT.arrayElementVarHandle();
            VarHandle vj = ValueLayout.JAVA_LONG.arrayElementVarHandle();
            VarHandle vs = ValueLayout.JAVA_SHORT.arrayElementVarHandle();
            MemoryLayout pair = MemoryLayout.structLayout(ValueLayout.JAVA_INT.withName("a"), ValueLayout.JAVA_INT.withName("b"));
            VarHandle vpair = pair.arrayElementVarHandle(MemoryLayout.PathElement.groupElement("b"));
            VarHandle vseq = MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT).arrayElementVarHandle(MemoryLayout.PathElement.sequenceElement());

            long sum = 0;
            for (int round = 0; round < 8; round++) {
                for (long i = 0; i < 40; i++) { vi.set(seg, 0L, i, (int) (i * 3 + round)); vj.set(seg, 1024L, i, i * 7L + round); vs.set(seg, 3000L, i, (short) (i + round)); }
                churn();
                for (long i = 0; i < 40; i++) sum += (int) vi.get(seg, 0L, i) + (long) vj.get(seg, 1024L, i) + (short) vs.get(seg, 3000L, i);
                vpair.set(seg, 2048L, (long) round, round * 11);
                vseq.set(seg, 0L, 1L, 2L, round);
                sum += (int) vpair.get(seg, 2048L, (long) round) + (int) vseq.get(seg, 0L, 1L, 2L);
            }
            line("sum", sum);

            // the same handles keep refusing what HotSpot refuses once warm
            for (int k = 0; k < 3; k++) {
                line("neg.index." + k, err(() -> { int x = (int) vi.get(seg, 0L, -1L); }));
                line("neg.base." + k, err(() -> { int x = (int) vi.get(seg, -4L, 0L); }));
                line("oob." + k, err(() -> { int x = (int) vi.get(seg, 0L, 100000L); }));
                line("overflow." + k, err(() -> { int x = (int) vi.get(seg, 0L, Long.MAX_VALUE); }));
                line("wrong.value." + k, err(() -> vi.set(seg, 0L, 0L, "s")));
                line("wrong.coord." + k, err(() -> { int x = (int) vi.get(seg, 0L, "i"); }));
                line("null.seg." + k, err(() -> { int x = (int) vi.get((MemorySegment) null, 0L, 0L); }));
                line("readonly." + k, err(() -> vi.set(seg.asReadOnly(), 0L, 0L, 1)));
                line("misaligned." + k, err(() -> { int x = (int) vi.get(seg.asSlice(1), 0L, 0L); }));
                churn();
            }
            line("after.errors", (int) vi.get(seg, 0L, 5L));

            // a closed arena
            Arena a2 = Arena.ofConfined();
            MemorySegment s2 = a2.allocate(64, 8);
            vi.set(s2, 0L, 1L, 9);
            a2.close();
            line("closed", err(() -> { int x = (int) vi.get(s2, 0L, 1L); }));

            // another thread, a shared arena
            Arena shared = Arena.ofShared();
            MemorySegment s3 = shared.allocate(256, 8);
            Thread[] ts = new Thread[4];
            long[] out = new long[4];
            for (int t = 0; t < ts.length; t++) {
                final int tt = t;
                ts[t] = new Thread(() -> { long s = 0; for (long i = 0; i < 200; i++) { vi.set(s3, 0L, i & 31, (int) i + tt); s += (int) vi.get(s3, 0L, i & 31); } out[tt] = s > 0 ? 1 : 0; });
                ts[t].start();
            }
            for (Thread t : ts) t.join();
            line("threads", out[0] + out[1] + out[2] + out[3]);
            shared.close();

            // a thread other than the owner of a confined arena
            Thread[] bad = new Thread[1];
            Object[] res = new Object[1];
            bad[0] = new Thread(() -> res[0] = err(() -> { int x = (int) vi.get(seg, 0L, 1L); }));
            bad[0].start(); bad[0].join();
            line("wrong.thread", res[0]);

            // the user's own coordinate filter is not a layout scale
            VarHandle raw = ValueLayout.JAVA_INT.varHandle();
            VarHandle mine = MethodHandles.collectCoordinates(raw, 1, MethodHandles.lookup().findStatic(Sbr5AvhArrayVhGc.class, "userScale", java.lang.invoke.MethodType.methodType(long.class, long.class, long.class)));
            for (int k = 0; k < 3; k++) {
                mine.set(seg, 0L, 2L, 77 + k);
                line("user.scale." + k, (int) mine.get(seg, 0L, 2L) + " " + (int) raw.get(seg, 36L) + " " + (int) vi.get(seg, 0L, 8L));
            }
        }
    }
}
