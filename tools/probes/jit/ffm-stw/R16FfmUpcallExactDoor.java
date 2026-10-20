// SPDX-License-Identifier: Apache-2.0
// JIT round 16 wave 9 (lane ffm), UP1-1 (CRATONVM_FFM_UPCALL_EXACT_DOOR): upcall
// targets of several shapes, each of the descriptor's exact type
// (MemorySegment, MemorySegment)int, driven by qsort. HotSpot invokes each
// target invokeExact at the descriptor's type. Prints "<shape>: sorted" per
// shape (the correctness lines), then "<shape> <n> ms" for 100 sorts of 64
// ints (the A/B lines; normalised by run16.sh). Which invoker each stub took:
// CRATONVM_DBG_MH_DISPATCH=1, stderr lines "[FFM_UPCALL_INVOKER] ...".
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class R16FfmUpcallExactDoor {
    static int calls;

    static int cmpStatic(MemorySegment a, MemorySegment b) {
        calls++;
        return Integer.compare(a.get(ValueLayout.JAVA_INT, 0), b.get(ValueLayout.JAVA_INT, 0));
    }

    static int cmpBiased(int bias, MemorySegment a, MemorySegment b) {
        calls++;
        return Integer.compare(a.get(ValueLayout.JAVA_INT, 0) + bias, b.get(ValueLayout.JAVA_INT, 0) + bias);
    }

    static int cmpWide(MemorySegment a, MemorySegment b, long ignored) {
        calls++;
        return Integer.compare(a.get(ValueLayout.JAVA_INT, 0), b.get(ValueLayout.JAVA_INT, 0));
    }

    static boolean always(MemorySegment a, MemorySegment b) {
        return true;
    }

    static int never(MemorySegment a, MemorySegment b) {
        throw new AssertionError("fallback");
    }

    final int sign;

    R16FfmUpcallExactDoor(int sign) {
        this.sign = sign;
    }

    int cmpVirtual(MemorySegment a, MemorySegment b) {
        calls++;
        return sign * Integer.compare(a.get(ValueLayout.JAVA_INT, 0), b.get(ValueLayout.JAVA_INT, 0));
    }

    static boolean sortWith(MethodHandle qsort, MemorySegment stub, MemorySegment array, int n, int round)
            throws Throwable {
        for (int i = 0; i < n; i++) {
            array.setAtIndex(ValueLayout.JAVA_INT, i, (i * 37 + round) % n);
        }
        qsort.invokeExact(array, (long) n, 4L, stub);
        for (int i = 0; i < n; i++) {
            if (array.getAtIndex(ValueLayout.JAVA_INT, i) != i) {
                return false;
            }
        }
        return true;
    }

    public static void main(String[] args) throws Throwable {
        Linker linker = Linker.nativeLinker();
        MethodHandle qsort = linker.downcallHandle(linker.defaultLookup().find("qsort").orElseThrow(),
                FunctionDescriptor.ofVoid(ValueLayout.ADDRESS, ValueLayout.JAVA_LONG,
                        ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
        FunctionDescriptor cmpDesc = FunctionDescriptor.of(ValueLayout.JAVA_INT,
                ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT),
                ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT));
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType seg2 = MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class);
        MethodHandle direct = l.findStatic(R16FfmUpcallExactDoor.class, "cmpStatic", seg2);
        MethodHandle bound = l.findVirtual(R16FfmUpcallExactDoor.class, "cmpVirtual", seg2)
                .bindTo(new R16FfmUpcallExactDoor(1));
        MethodHandle inserted = MethodHandles.insertArguments(l.findStatic(R16FfmUpcallExactDoor.class,
                "cmpBiased", MethodType.methodType(int.class, int.class, MemorySegment.class,
                        MemorySegment.class)), 0, 5);
        MethodHandle dropped = MethodHandles.insertArguments(l.findStatic(R16FfmUpcallExactDoor.class,
                "cmpWide", MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class,
                        long.class)), 2, 0L);
        MethodHandle guarded = MethodHandles.guardWithTest(
                l.findStatic(R16FfmUpcallExactDoor.class, "always",
                        MethodType.methodType(boolean.class, MemorySegment.class, MemorySegment.class)),
                direct,
                l.findStatic(R16FfmUpcallExactDoor.class, "never", seg2));
        MethodHandle filtered = MethodHandles.filterReturnValue(direct, MethodHandles.identity(int.class));
        String[] names = {"direct", "bound", "inserted", "dropped", "guarded", "filtered"};
        MethodHandle[] targets = {direct, bound, inserted, dropped, guarded, filtered};
        int n = 64;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment array = arena.allocate(ValueLayout.JAVA_INT, n);
            MemorySegment[] stubs = new MemorySegment[targets.length];
            for (int t = 0; t < targets.length; t++) {
                stubs[t] = linker.upcallStub(targets[t], cmpDesc, arena);
                System.out.println(names[t] + ": " + (sortWith(qsort, stubs[t], array, n, t) ? "sorted" : "NOT sorted"));
            }
            for (int t = 0; t < targets.length; t++) {
                boolean ok = true;
                for (int r = 0; r < 20; r++) {
                    ok &= sortWith(qsort, stubs[t], array, n, r);
                }
                long t0 = System.nanoTime();
                for (int r = 0; r < 100; r++) {
                    ok &= sortWith(qsort, stubs[t], array, n, r);
                }
                long ms = (System.nanoTime() - t0) / 1_000_000;
                System.out.println(names[t] + " " + ms + " ms" + (ok ? "" : " WRONG"));
            }
        }
        System.out.println("calls>0 " + (calls > 0));
    }
}
