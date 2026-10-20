// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.nio.ByteBuffer;

import static java.lang.invoke.MethodType.methodType;

/**
 * gci i5g/g1r: netty 4.2's {@code CleanerJava25} allocation chain with the JDK
 * only (page {@code gc-g1-netty-direct-buffer-methodhandle-result-is-a-reclaimed-object-20261010}).
 *
 * <p>{@code GciG1rMhChainProbe <shape> <iters> <capacity>}. Every iteration
 * builds one {@code Impl} (netty's {@code CleanableDirectBufferImpl}: an
 * {@code AutoCloseable} arena, the segment's {@code ByteBuffer} and its
 * address), checks it, writes and reads the buffer, and closes the shared
 * arena, while a daemon thread allocates byte arrays flat out (the page's
 * churn). Shapes:
 * <ul>
 *   <li>{@code full}: netty's composed handle, {@code (int)Impl}, called with
 *       {@code invokeExact} (foldArguments / insertArguments / permuteArguments
 *       / collectArguments / explicitCastArguments / filterArguments over a
 *       {@code findConstructor} handle) -- the page's path;</li>
 *   <li>{@code ctor}: the arena, the segment, its buffer and address made in
 *       Java, only the constructor handle called through {@code invokeExact};</li>
 *   <li>{@code filter}: the arena and segment made in Java, the
 *       {@code filterArguments} handle over the constructor (asByteBuffer and
 *       address as filters) through {@code invokeExact};</li>
 *   <li>{@code plain}: no handle at all (the control).</li>
 * </ul>
 *
 * <p>stdout (deterministic, compared with HotSpot): one line
 * {@code shape=<s> iters=<n> ok sum=<sum>}, or {@code shape=<s> FAIL} when any
 * iteration's object was not intact. stderr: the first three failures, with
 * the iteration, what was wrong and the elapsed time.
 */
public class GciG1rMhChainProbe {
    static final class Impl {
        private final AutoCloseable closeable;
        private final ByteBuffer buffer;
        private final long memoryAddress;

        Impl(AutoCloseable closeable, ByteBuffer buffer, long memoryAddress) {
            this.closeable = closeable;
            this.buffer = buffer;
            this.memoryAddress = memoryAddress;
        }
    }

    static volatile Object sink;
    static int failures;

    /** netty 4.2.17 CleanerJava25.<clinit>, transcribed. */
    static MethodHandle fullChain() throws Throwable {
        Class<?> arenaCls = Arena.class;
        Class<?> memsegCls = MemorySegment.class;
        Class<Impl> bufCls = Impl.class;
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle ofShared = lookup.findStatic(arenaCls, "ofShared", methodType(arenaCls));
        Object shared = ofShared.invoke();
        ((AutoCloseable) shared).close();
        MethodHandle allocate = lookup.findVirtual(arenaCls, "allocate", methodType(memsegCls, long.class));
        MethodHandle asByteBuffer = lookup.findVirtual(memsegCls, "asByteBuffer", methodType(ByteBuffer.class));
        MethodHandle address = lookup.findVirtual(memsegCls, "address", methodType(long.class));
        MethodHandle bufClsCtor = lookup.findConstructor(bufCls,
                methodType(void.class, AutoCloseable.class, ByteBuffer.class, long.class));
        MethodHandle allocateInt = MethodHandles.explicitCastArguments(allocate,
                methodType(memsegCls, arenaCls, int.class));
        MethodHandle ctorArenaMemsegMemseg = MethodHandles.explicitCastArguments(
                MethodHandles.filterArguments(bufClsCtor, 1, asByteBuffer, address),
                methodType(bufCls, arenaCls, memsegCls, memsegCls));
        MethodHandle ctorArenaMemsegNull = MethodHandles.permuteArguments(ctorArenaMemsegMemseg,
                methodType(bufCls, arenaCls, memsegCls, memsegCls), 0, 1, 1);
        MethodHandle ctorArenaMemseg = MethodHandles.insertArguments(
                ctorArenaMemsegNull, 2, new Object[]{null});
        MethodHandle ctorArenaArenaInt = MethodHandles.collectArguments(ctorArenaMemseg, 1, allocateInt);
        MethodHandle ctorArenaNullInt = MethodHandles.permuteArguments(ctorArenaArenaInt,
                methodType(bufCls, arenaCls, arenaCls, int.class), 0, 0, 2);
        MethodHandle ctorArenaInt = MethodHandles.insertArguments(ctorArenaNullInt, 1, new Object[]{null});
        return MethodHandles.foldArguments(ctorArenaInt, ofShared);
    }

    static MethodHandle ctorHandle() throws Throwable {
        return MethodHandles.lookup().findConstructor(Impl.class,
                methodType(void.class, AutoCloseable.class, ByteBuffer.class, long.class));
    }

    static MethodHandle filterHandle() throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle asByteBuffer = lookup.findVirtual(MemorySegment.class, "asByteBuffer",
                methodType(ByteBuffer.class));
        MethodHandle address = lookup.findVirtual(MemorySegment.class, "address", methodType(long.class));
        MethodHandle filtered = MethodHandles.filterArguments(ctorHandle(), 1, asByteBuffer, address);
        return MethodHandles.explicitCastArguments(filtered,
                methodType(Impl.class, Arena.class, MemorySegment.class, MemorySegment.class));
    }

    static void fail(String shape, int i, String what, long t0) {
        failures++;
        if (failures <= 3) {
            System.err.println("[g1r-mhchain] shape=" + shape + " FAIL i=" + i + " " + what
                    + " after_ms=" + (System.nanoTime() - t0) / 1_000_000);
        }
    }

    public static void main(String[] args) throws Throwable {
        String shape = args.length > 0 ? args[0] : "full";
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        int capacity = args.length > 2 ? Integer.parseInt(args[2]) : 70000;
        MethodHandle full = shape.equals("full") ? fullChain() : null;
        MethodHandle ctor = shape.equals("ctor") ? ctorHandle() : null;
        MethodHandle filter = shape.equals("filter") ? filterHandle() : null;
        if (full == null && ctor == null && filter == null && !shape.equals("plain")) {
            System.out.println("usage: GciG1rMhChainProbe full|ctor|filter|plain [iters] [capacity]");
            return;
        }
        Thread churn = new Thread(() -> {
            Object[] ring = new Object[64];
            int k = 0;
            while (true) {
                ring[k++ & 63] = new byte[(k & 7) * 1024 + 16];
            }
        });
        churn.setDaemon(true);
        churn.start();
        long t0 = System.nanoTime();
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            Impl b;
            try {
                if (full != null) {
                    b = (Impl) full.invokeExact(capacity);
                } else {
                    Arena arena = Arena.ofShared();
                    MemorySegment segment = arena.allocate(capacity);
                    if (ctor != null) {
                        b = (Impl) ctor.invokeExact((AutoCloseable) arena, segment.asByteBuffer(),
                                segment.address());
                    } else if (filter != null) {
                        b = (Impl) filter.invokeExact(arena, segment, segment);
                    } else {
                        b = new Impl(arena, segment.asByteBuffer(), segment.address());
                    }
                }
            } catch (Throwable t) {
                fail(shape, i, "threw " + t, t0);
                continue;
            }
            Object o = b;
            if (o == null) {
                fail(shape, i, "null result", t0);
                continue;
            }
            if (o.getClass() != Impl.class) {
                fail(shape, i, "class " + o.getClass().getName(), t0);
                continue;
            }
            if (b.buffer == null || b.closeable == null || b.memoryAddress == 0) {
                fail(shape, i, "fields buffer=" + (b.buffer != null) + " closeable="
                        + (b.closeable != null) + " address=" + (b.memoryAddress != 0), t0);
                continue;
            }
            try {
                if (b.buffer.capacity() != capacity) {
                    fail(shape, i, "capacity " + b.buffer.capacity(), t0);
                    continue;
                }
                b.buffer.putInt(0, i);
                sum += b.buffer.getInt(0);
                sum += b.buffer.capacity();
                b.closeable.close();
            } catch (Throwable t) {
                fail(shape, i, "use threw " + t, t0);
                continue;
            }
            if ((i & 63) == 0) {
                sink = new byte[8192];
            }
        }
        if (failures == 0) {
            System.out.println("shape=" + shape + " iters=" + iters + " ok sum=" + sum);
        } else {
            System.out.println("shape=" + shape + " FAIL");
            System.err.println("[g1r-mhchain] shape=" + shape + " failures=" + failures + " of " + iters);
        }
        System.err.println("[g1r-mhchain] shape=" + shape + " wall_ms=" + (System.nanoTime() - t0) / 1_000_000);
    }
}
