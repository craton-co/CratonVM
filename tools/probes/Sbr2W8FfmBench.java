import java.lang.foreign.*;
import java.lang.invoke.VarHandle;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;

/** SBR2 wave 8 lane FFM A/B: prints "<section> <rep> <ns/op>" lines. Run interleaved per binary, medians of >= 5 reps.
 *  Args: [reps] [scale]. */
public class Sbr2W8FfmBench {
    static long sink;
    static String filter = null;
    interface Body { void run(int n); }
    static void time(String name, int reps, int n, Body b) {
        if (filter != null && !name.contains(filter)) return;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            b.run(n);
            long t1 = System.nanoTime();
            System.out.println(name + " " + r + " " + (t1 - t0) / (double) n);
        }
    }
    public static void main(String[] a) {
        int reps = a.length > 0 ? Integer.parseInt(a[0]) : 5;
        int scale = a.length > 1 ? Integer.parseInt(a[1]) : 1;
        if (a.length > 2) filter = a[2];
        ByteBuffer dbb = ByteBuffer.allocateDirect(4096).order(ByteOrder.nativeOrder());
        time("dbb.get/putByte", reps, 200000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) { dbb.put(i & 4095, (byte) i); s += dbb.get((i * 7) & 4095); } sink += s; });
        time("dbb.get/putInt", reps, 200000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) { dbb.putInt((i & 1023) << 2, i); s += dbb.getInt(((i * 7) & 1023) << 2); } sink += s; });
        time("dbb.get/putLong", reps, 200000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) { dbb.putLong((i & 511) << 3, i); s += dbb.getLong(((i * 7) & 511) << 3); } sink += s; });
        time("dbb.get/putShort", reps, 200000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) { dbb.putShort((i & 2047) << 1, (short) i); s += dbb.getShort(((i * 7) & 2047) << 1); } sink += s; });
        try (Arena x = Arena.ofConfined()) {
            MemorySegment seg = x.allocate(4096);
            time("seg.get/setInt", reps, 200000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) { seg.set(ValueLayout.JAVA_INT, (i & 1023) << 2, i); s += seg.get(ValueLayout.JAVA_INT, ((i * 7) & 1023) << 2); } sink += s; });
            time("seg.get/setLong", reps, 200000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) { seg.set(ValueLayout.JAVA_LONG, (i & 511) << 3, i); s += seg.get(ValueLayout.JAVA_LONG, ((i * 7) & 511) << 3); } sink += s; });
            VarHandle vh = ValueLayout.JAVA_INT.varHandle();
            time("seg.vh.get/setInt", reps, 100000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) { vh.set(seg, (long) ((i & 1023) << 2), i); s += (int) vh.get(seg, (long) (((i * 7) & 1023) << 2)); } sink += s; });
            VarHandle avh = ValueLayout.JAVA_INT.arrayElementVarHandle();
            time("seg.vh.arrayElement.get/setInt", reps, 100000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) { avh.set(seg, 0L, (long) (i & 1023), i); s += (int) avh.get(seg, 0L, (long) ((i * 7) & 1023)); } sink += s; });
            MemorySegment d = x.allocate(4096);
            time("seg.copy4k", reps, 20000 * scale, n -> { for (int i = 0; i < n; i++) MemorySegment.copy(seg, 0, d, 0, 4096); });
            time("seg.fill4k", reps, 20000 * scale, n -> { for (int i = 0; i < n; i++) seg.fill((byte) i); });
            int[] arr = new int[1024];
            time("seg.copyToArray", reps, 20000 * scale, n -> { for (int i = 0; i < n; i++) MemorySegment.copy(seg, ValueLayout.JAVA_INT, 0, arr, 0, 1024); });
            time("seg.ofArray.get", reps, 100000 * scale, n -> { long s = 0; MemorySegment h = MemorySegment.ofArray(arr); for (int i = 0; i < n; i++) s += h.get(ValueLayout.JAVA_INT, (i & 1023) << 2); sink += s; });
            time("seg.asSlice", reps, 100000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) s += seg.asSlice(i & 1023, 8).byteSize(); sink += s; });
            time("seg.elements.sum", reps, 200 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) s += seg.elements(ValueLayout.JAVA_INT).mapToInt(e -> e.get(ValueLayout.JAVA_INT, 0)).sum(); sink += s; });
        }
        time("arena.confined.alloc+close", reps, 20000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) { try (Arena x = Arena.ofConfined()) { s += x.allocate(64).byteSize(); } } sink += s; });
        time("arena.shared.alloc+close", reps, 5000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) { try (Arena x = Arena.ofShared()) { s += x.allocate(64).byteSize(); } } sink += s; });
        time("arena.auto.alloc", reps, 20000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) s += Arena.ofAuto().allocate(64).byteSize(); sink += s; });
        time("layout.struct.mint", reps, 20000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) s += MemoryLayout.structLayout(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG).byteSize(); sink += s; });
        time("layout.union.mint", reps, 20000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) s += MemoryLayout.unionLayout(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG).byteSize(); sink += s; });
        time("layout.seq.mint", reps, 20000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) s += MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT).byteSize(); sink += s; });
        time("layout.pad.mint", reps, 20000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) s += MemoryLayout.paddingLayout(4).byteSize(); sink += s; });
        time("fd.of", reps, 20000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) s += FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.ADDRESS).argumentLayouts().size(); sink += s; });
        Linker lk = Linker.nativeLinker();
        time("linker.downcallHandle(setup)", reps, 2000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) s += lk.downcallHandle(MemorySegment.ofAddress(8), FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG)).type().parameterCount(); sink += s; });
        FunctionDescriptor pfd = FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG);
        time("linker.downcallHandle(prebuilt fd)", reps, 2000 * scale, n -> { long s = 0; for (int i = 0; i < n; i++) s += lk.downcallHandle(MemorySegment.ofAddress(8), pfd).type().parameterCount(); sink += s; });
        try {
            java.lang.invoke.MethodHandle labs = lk.downcallHandle(lk.defaultLookup().find("abs").get(), FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
            time("downcall.abs.invoke", reps, 20000 * scale, n -> { long s = 0; try { for (int i = 0; i < n; i++) s += (int) labs.invokeExact(-i); } catch (Throwable t) { throw new RuntimeException(t); } sink += s; });
            java.lang.invoke.MethodHandle strlen = lk.downcallHandle(lk.defaultLookup().find("strlen").get(), FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
            try (Arena y = Arena.ofConfined()) { MemorySegment str = y.allocateFrom("hello world"); time("downcall.strlen.invoke", reps, 20000 * scale, n -> { long s = 0; try { for (int i = 0; i < n; i++) s += (long) strlen.invokeExact(str); } catch (Throwable t) { throw new RuntimeException(t); } sink += s; }); }
        } catch (Throwable t) { System.out.println("downcall bench skipped " + t); }
        System.out.println("sink " + (sink != 42));
    }
}
