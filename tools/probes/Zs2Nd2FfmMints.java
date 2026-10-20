// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.lang.foreign.AddressLayout;
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;

/**
 * zs s2/nd2: the native mints that run AFTER an effect that cannot be redone,
 * on a near-full heap (ZGC round zs, wave s2, lane nd2).
 *
 * <ul>
 *   <li>{@code addr}: a downcall returning a pointer ({@code strchr} into an
 *       arena string). The {@code MemorySegment} for the return is minted
 *       after the C call; compiled, the JIT's direct segment stub made it
 *       through the one-attempt door, which aborts on a full ZGC heap
 *       ({@code CRATONVM_ZGC_FFM_MINT_COLLECT}).
 *   <li>{@code upcall}: {@code qsort} of 8 ints with a Java comparator: each
 *       upcall mints its two address arguments (same switch).
 *   <li>{@code define}: {@code Lookup.defineHiddenClass} of a tiny class: the
 *       mirror is minted after the define
 *       ({@code CRATONVM_ZGC_DEFINE_MIRROR_COLLECT}).
 *   <li>{@code mixed}: the three in turn.
 * </ul>
 *
 * <p>The live set is grown with 64 KiB chunks until about {@code freeKb} KiB
 * is left (the {@code Zs1NdNativeDoors} fill), then each iteration makes 512
 * bytes of garbage beside the door. HotSpot collects for every refused
 * allocation: {@code door_oome=0}. One deterministic stdout line.
 * Usage: {@code java --enable-native-access=ALL-UNNAMED -Xmx64m Zs2Nd2FfmMints
 * <mode> [freeKb] [iters]} (defaults 1024 and 20000).
 */
public class Zs2Nd2FfmMints {
    static volatile Object sink;

    /** The hidden class's template (its bytes are read from the class path). */
    public static final class Tiny {
        public static int answer() {
            return 42;
        }
    }

    static int compare(MemorySegment a, MemorySegment b) {
        return Integer.compare(a.get(ValueLayout.JAVA_INT, 0), b.get(ValueLayout.JAVA_INT, 0));
    }

    public static void main(String[] args) throws Throwable {
        String mode = args.length > 0 ? args[0] : "addr";
        int freeKb = args.length > 1 ? Integer.parseInt(args[1]) : 1024;
        int iters = args.length > 2 ? Integer.parseInt(args[2]) : 20_000;

        Linker linker = Linker.nativeLinker();
        MethodHandle strchr = linker.downcallHandle(
                linker.defaultLookup().find("strchr").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.JAVA_INT));
        AddressLayout intPtr = ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT);
        MethodHandle qsort = linker.downcallHandle(
                linker.defaultLookup().find("qsort").orElseThrow(),
                FunctionDescriptor.ofVoid(ValueLayout.ADDRESS, ValueLayout.JAVA_LONG,
                        ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
        MethodHandle cmp = MethodHandles.lookup().findStatic(Zs2Nd2FfmMints.class, "compare",
                MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class));
        byte[] tinyBytes;
        try (InputStream in = Zs2Nd2FfmMints.class.getResourceAsStream("Zs2Nd2FfmMints$Tiny.class")) {
            tinyBytes = in.readAllBytes();
        }
        MethodHandles.Lookup lookup = MethodHandles.lookup();

        long check = 0;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment text = arena.allocateFrom("abcdefghijklmnopqrstuvwxyz");
            MemorySegment ints = arena.allocate(ValueLayout.JAVA_INT, 8);
            MemorySegment cmpStub = linker.upcallStub(cmp,
                    FunctionDescriptor.of(ValueLayout.JAVA_INT, intPtr, intPtr), arena);
            // Warm every door before the heap is full (linkage, compilation).
            for (int i = 0; i < 20_000; i++) {
                check += step("addr", i, strchr, qsort, cmpStub, text, ints, lookup, tinyBytes);
                if ((i & 63) == 0) {
                    check += step("upcall", i, strchr, qsort, cmpStub, text, ints, lookup, tinyBytes);
                }
                if ((i & 1023) == 0) {
                    check += step("define", i, strchr, qsort, cmpStub, text, ints, lookup, tinyBytes);
                }
            }
            check = 0;
            System.gc();
            Runtime rt = Runtime.getRuntime();
            long used0 = rt.totalMemory() - rt.freeMemory();
            long room = rt.maxMemory() - used0 - (long) freeKb * 1024;
            ArrayList<byte[]> live = new ArrayList<>();
            try {
                for (long n = Math.max(0, room / (64 * 1024)); n > 0; n--) {
                    live.add(new byte[64 * 1024 - 16]);
                }
            } catch (OutOfMemoryError e) {
                for (int i = 0; i < 4 && !live.isEmpty(); i++) {
                    live.remove(live.size() - 1);
                }
            }
            int doorOome = 0;
            int bad = 0;
            int done = 0;
            String[] cycle = {"addr", "upcall", "define"};
            for (int i = 0; i < iters; i++) {
                String m = mode.equals("mixed") ? cycle[i % 3] : mode;
                try {
                    long r = step(m, i, strchr, qsort, cmpStub, text, ints, lookup, tinyBytes);
                    if (r < 0) {
                        bad++;
                    } else {
                        check += r;
                    }
                    sink = new byte[512];
                } catch (OutOfMemoryError e) {
                    doorOome++;
                }
                done++;
            }
            System.out.println("mode=" + mode + " door_oome=" + doorOome + " bad=" + bad
                    + " iters=" + done + " filled=" + !live.isEmpty() + " check=" + check);
            sink = live;
        }
    }

    /** One door; answers a non-negative check value, or -1 on a wrong answer. */
    static long step(String m, int i, MethodHandle strchr, MethodHandle qsort,
            MemorySegment cmpStub, MemorySegment text, MemorySegment ints,
            MethodHandles.Lookup lookup, byte[] tinyBytes) throws Throwable {
        switch (m) {
            case "addr": {
                int want = i % 26;
                MemorySegment at = (MemorySegment) strchr.invokeExact(text, 'a' + want);
                return at.address() - text.address() == want ? want : -1;
            }
            case "upcall": {
                for (int k = 0; k < 8; k++) {
                    ints.setAtIndex(ValueLayout.JAVA_INT, k, (i * 31 + k * 7919) % 1000);
                }
                qsort.invokeExact(ints, 8L, 4L, cmpStub);
                for (int k = 1; k < 8; k++) {
                    if (ints.getAtIndex(ValueLayout.JAVA_INT, k - 1)
                            > ints.getAtIndex(ValueLayout.JAVA_INT, k)) {
                        return -1;
                    }
                }
                return ints.getAtIndex(ValueLayout.JAVA_INT, 0) & 0xF;
            }
            case "define": {
                Class<?> hidden = lookup.defineHiddenClass(tinyBytes, false).lookupClass();
                return hidden.isHidden() ? 1 : -1;
            }
            default:
                throw new IllegalArgumentException(m);
        }
    }
}
