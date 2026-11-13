// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 5 (lane FFMUP): what an FFM up-call's Java sees of its address arguments, the rows the
// batched argument mint (`CRATONVM_FFM_UPCALL_MINT_BATCH`) must keep exact. Each up-call is reached
// through a down-call of the stub's own address (C calling the stub), or through qsort.
//   java --enable-native-access=ALL-UNNAMED J27FfmUpcallRows [-Dj27.time=1]
// One `fact <name> <value>` line per answer (must equal HotSpot 25); `time` lines only with
// -Dj27.time=1 (informational).
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import static java.lang.foreign.ValueLayout.*;

public class J27FfmUpcallRows {
    static final Linker L = Linker.nativeLinker();
    static final MethodHandles.Lookup LK = MethodHandles.lookup();
    static final MethodHandle QSORT = L.downcallHandle(L.defaultLookup().find("qsort").orElseThrow(),
        FunctionDescriptor.ofVoid(JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG));
    static final AddressLayout INT_PTR = ADDRESS.withTargetLayout(JAVA_INT);

    static String seen;
    static MemorySegment kept;
    static long upcalls;

    // (MemorySegment, int, MemorySegment)int: the segments' shapes and the int in between.
    static int mixed(MemorySegment a, int k, MemorySegment b) {
        seen = a.byteSize() + "/" + b.byteSize() + "/" + k + "/" + a.isNative() + "/" + a.isReadOnly()
            + "/" + (a.scope().equals(Arena.global().scope())) + "/" + (b.scope().equals(Arena.global().scope()))
            + "/" + a.get(JAVA_INT, 0) + "/" + (b.address() - a.address());
        return a.get(JAVA_INT, 0) * 10 + k;
    }

    // A collection inside the up-call: the argument segments stay usable, and one kept past it too.
    static int collects(MemorySegment a, MemorySegment b) {
        byte[][] junk = new byte[64][];
        for (int i = 0; i < junk.length; i++) junk[i] = new byte[4096];
        System.gc();
        kept = a;
        return a.get(JAVA_INT, 0) + b.get(JAVA_INT, 0) + junk.length;
    }

    static String state;
    static int stateOf(MemorySegment a) {
        state = Thread.currentThread().getState().toString();
        return a.get(JAVA_INT, 0);
    }

    static int cmp(MemorySegment a, MemorySegment b) {
        upcalls++;
        return Integer.compare(a.get(JAVA_INT, 0), b.get(JAVA_INT, 0));
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    static MethodHandle stubCaller(MethodHandle target, FunctionDescriptor fd, Arena arena) {
        MemorySegment stub = L.upcallStub(target, fd, arena);
        return L.downcallHandle(stub, fd);
    }

    public static void main(String[] args) throws Throwable {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment ints = arena.allocate(JAVA_INT, 4);
            for (int i = 0; i < 4; i++) ints.setAtIndex(JAVA_INT, i, 7 + i);
            MemorySegment second = ints.asSlice(8);

            FunctionDescriptor mixedFd = FunctionDescriptor.of(JAVA_INT, INT_PTR, JAVA_INT, ADDRESS);
            MethodHandle mixed = stubCaller(LK.findStatic(J27FfmUpcallRows.class, "mixed",
                MethodType.methodType(int.class, MemorySegment.class, int.class, MemorySegment.class)), mixedFd, arena);
            int r = (int) mixed.invokeExact(ints, 3, second);
            fact("mixed-return", r);
            fact("mixed-seen", seen);
            for (int i = 0; i < 2000; i++) r = (int) mixed.invokeExact(ints, i & 7, second);
            fact("mixed-warm-return", r);
            fact("mixed-warm-seen", seen);

            FunctionDescriptor gcFd = FunctionDescriptor.of(JAVA_INT, INT_PTR, INT_PTR);
            MethodHandle gc = stubCaller(LK.findStatic(J27FfmUpcallRows.class, "collects",
                MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class)), gcFd, arena);
            int g = 0;
            for (int i = 0; i < 5; i++) g += (int) gc.invokeExact(ints, second);
            fact("gc-return-sum", g);
            fact("gc-kept-after", kept.byteSize() + "/" + kept.get(JAVA_INT, 0) + "/" + (kept.address() == ints.address()));

            FunctionDescriptor stFd = FunctionDescriptor.of(JAVA_INT, INT_PTR);
            MethodHandle st = stubCaller(LK.findStatic(J27FfmUpcallRows.class, "stateOf",
                MethodType.methodType(int.class, MemorySegment.class)), stFd, arena);
            fact("state-return", (int) st.invokeExact(second));
            fact("state-in-upcall", state);

            // A by-value struct argument next to an address: the per-argument path.
            StructLayout pair = MemoryLayout.structLayout(JAVA_INT.withName("x"), JAVA_INT.withName("y"));
            FunctionDescriptor grpFd = FunctionDescriptor.of(JAVA_INT, pair, INT_PTR);
            MethodHandle grp = stubCaller(LK.findStatic(J27FfmUpcallRows.class, "group",
                MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class)), grpFd, arena);
            MemorySegment p = arena.allocate(pair);
            p.set(JAVA_INT, 0, 40);
            p.set(JAVA_INT, 4, 2);
            fact("group-return", (int) grp.invokeExact(p, ints));

            MethodHandle cmpMh = LK.findStatic(J27FfmUpcallRows.class, "cmp",
                MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class));
            long cmpStub = L.upcallStub(cmpMh, FunctionDescriptor.of(JAVA_INT, INT_PTR, INT_PTR), arena).address();
            MemorySegment arr = arena.allocate(32, 16);
            long acc = 0;
            boolean time = System.getProperty("j27.time") != null;
            int sorts = time ? 40_000 : 2_000;
            long t0 = System.nanoTime(), u0 = upcalls;
            int seed = 12345;
            for (int i = 0; i < sorts; i++) {
                for (int k = 0; k < 8; k++) {
                    seed = seed * 1103515245 + 12345;
                    arr.setAtIndex(JAVA_INT, k, (seed >>> 8) % 1000);
                }
                QSORT.invokeExact(arr.address(), 8L, 4L, cmpStub);
                if (i < 2_000) acc = acc * 31 + arr.getAtIndex(JAVA_INT, 0) + arr.getAtIndex(JAVA_INT, 7);
            }
            fact("qsort-checksum", acc);
            if (time) {
                System.out.println("time qsort-ns-per-upcall " + (System.nanoTime() - t0) / Math.max(1, upcalls - u0));
            }
        }
    }

    static int group(MemorySegment pair, MemorySegment a) {
        return pair.get(JAVA_INT, 0) + pair.get(JAVA_INT, 4) + a.get(JAVA_INT, 0) * 100 + (int) pair.byteSize();
    }
}
