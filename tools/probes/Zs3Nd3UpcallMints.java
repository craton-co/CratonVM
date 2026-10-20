// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.StructLayout;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Proxy;
import java.util.ArrayList;

/**
 * zs s3/nd3: the remaining native mints on a near-full heap (ZGC round zs,
 * wave s3, lane nd3).
 *
 * <ul>
 *   <li>{@code group}: an upcall with a by-value STRUCT argument, reached by a
 *       downcall into the upcall stub's own address. The dispatch mints a
 *       per-upcall confined arena, its session and the argument's segment
 *       ({@code CRATONVM_ZGC_UPCALL_ARGS_COLLECT}); they went through the
 *       one-attempt doors, which abort on a full ZGC heap.
 *   <li>{@code proxy}: {@code Proxy.getProxyClass} with a fresh loader per
 *       iteration: a proxy class is defined and its mirror minted after the
 *       define (the {@code --compatible} native; {@code --jdk-only} runs the
 *       JDK's {@code ProxyBuilder}, which defines through
 *       {@code ClassLoader.defineClass0}).
 *   <li>{@code mixed}: the two in turn.
 * </ul>
 *
 * <p>The live set is grown with 64 KiB chunks until about {@code freeKb} KiB
 * is left, then each iteration makes 512 bytes of garbage beside the door.
 * HotSpot collects for every refused allocation: {@code door_oome=0}. One
 * deterministic stdout line. Usage: {@code java --enable-native-access=ALL-UNNAMED
 * -Xmx64m Zs3Nd3UpcallMints <mode> [freeKb] [iters]} (defaults 4096 and 20000;
 * HotSpot's own upcall allocations need the room: see {@code Zs2Nd2FfmMints}).
 */
public class Zs3Nd3UpcallMints {
    static volatile Object sink;

    static final StructLayout POINT = MemoryLayout.structLayout(
            ValueLayout.JAVA_INT.withName("x"), ValueLayout.JAVA_INT.withName("y"));

    static int sum(MemorySegment p) {
        return p.get(ValueLayout.JAVA_INT, 0) + p.get(ValueLayout.JAVA_INT, 4);
    }

    @SuppressWarnings("deprecation")
    static long step(String m, int i, MethodHandle call, MemorySegment pt) throws Throwable {
        switch (m) {
            case "group": {
                int x = i % 1000;
                pt.set(ValueLayout.JAVA_INT, 0, x);
                pt.set(ValueLayout.JAVA_INT, 4, 7);
                int r = (int) call.invokeExact(pt);
                return r == x + 7 ? (r & 0xF) : -1;
            }
            case "proxy": {
                ClassLoader fresh = new ClassLoader(Zs3Nd3UpcallMints.class.getClassLoader()) {};
                Class<?> c = Proxy.getProxyClass(fresh, Runnable.class);
                return Proxy.isProxyClass(c) && c.getClassLoader() == fresh ? 1 : -1;
            }
            default:
                throw new IllegalArgumentException(m);
        }
    }

    public static void main(String[] args) throws Throwable {
        String mode = args.length > 0 ? args[0] : "group";
        int freeKb = args.length > 1 ? Integer.parseInt(args[1]) : 4096;
        int iters = args.length > 2 ? Integer.parseInt(args[2]) : 20_000;

        Linker linker = Linker.nativeLinker();
        MethodHandle target = MethodHandles.lookup().findStatic(Zs3Nd3UpcallMints.class, "sum",
                MethodType.methodType(int.class, MemorySegment.class));
        long check = 0;
        try (Arena arena = Arena.ofConfined()) {
            FunctionDescriptor fd = FunctionDescriptor.of(ValueLayout.JAVA_INT, POINT);
            MemorySegment stub = linker.upcallStub(target, fd, arena);
            MethodHandle call = linker.downcallHandle(stub, fd);
            MemorySegment pt = arena.allocate(POINT);
            // Warm every door before the heap is full (linkage, compilation).
            for (int i = 0; i < 20_000; i++) {
                check += step("group", i, call, pt);
                if ((i & 255) == 0) {
                    check += step("proxy", i, call, pt);
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
            String[] cycle = {"group", "proxy"};
            for (int i = 0; i < iters; i++) {
                String m = mode.equals("mixed") ? cycle[i % 2] : mode;
                try {
                    long r = step(m, i, call, pt);
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
}
