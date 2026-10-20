// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.SoftReference;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.Map;

/**
 * zr z8/nr: allocation through native doors that used to make ONE attempt and
 * raise {@code OutOfMemoryError} without collecting, on a near-full heap.
 *
 * <p>The live set is grown with 64 KiB chunks until about {@code freeKb} KiB
 * of the heap is left (measured from {@code Runtime} after a
 * {@code System.gc()}; an early error drops four chunks). The churn loop then
 * allocates its garbage ONLY through the door under test, so no Java
 * {@code new} (which collects on refusal) can clean the heap in front of it:
 *
 * <ul>
 *   <li>{@code allocinst}: {@code sun.misc.Unsafe.allocateInstance(Box.class)}
 *       (the VM's {@code instantiate} door);
 *   <li>{@code ctor}: {@code Constructor.newInstance()} of {@code Box}
 *       (constructor method handle -> {@code Unsafe.allocateInstance});
 *   <li>{@code threads}: {@code Thread.getAllStackTraces()} every 8th
 *       iteration ({@code Thread.getThreads} / {@code dumpThreads} arrays);
 *   <li>{@code soft2}: SoftRefBeforeOome with a second thread churning garbage
 *       while main retains {@code Box} instances from {@code allocateInstance}
 *       until the error: the soft referent must be cleared by then.
 * </ul>
 *
 * <p>HotSpot stalls and collects for every refused allocation, so its lines
 * are {@code door_oome=0} for the churn modes and {@code oome true} /
 * {@code cleared true} for {@code soft2}.
 *
 * <p>Usage: {@code java -Xmx64m Zr8NrNativeDoors <mode> [freeKb] [iters]}
 * (defaults 1024 and 20000; below ~1.33 MiB free the ZGC near-full floor
 * defers every occupancy collection, page
 * {@code zgc-zr7rv6-near-full-floor-runs-the-heap-dry-...}).
 */
public class Zr8NrNativeDoors {
    static volatile Object sink;
    static volatile boolean stop;

    /** 14 longs: ~128 bytes per instance. */
    static final class Box {
        long a0, a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13;

        public Box() {}
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "allocinst";
        int freeKb = args.length > 1 ? Integer.parseInt(args[1]) : 1024;
        int iters = args.length > 2 ? Integer.parseInt(args[2]) : 20_000;
        Field f = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        f.setAccessible(true);
        sun.misc.Unsafe unsafe = (sun.misc.Unsafe) f.get(null);
        Constructor<Box> ctor = Box.class.getDeclaredConstructor();
        // Warm every door before the heap is full (class init, method handles).
        sink = unsafe.allocateInstance(Box.class);
        sink = ctor.newInstance();
        sink = Thread.getAllStackTraces();
        if (mode.equals("soft2")) {
            soft2(unsafe);
            return;
        }
        // Fill to (max - freeKb) with live 64 KiB chunks WITHOUT reaching an
        // OutOfMemoryError: a fill to the error takes HotSpot ZGC minutes of
        // allocation stalls on a 64 MiB heap.
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
        int done = 0;
        for (int i = 0; i < iters; i++) {
            try {
                switch (mode) {
                    case "allocinst":
                        sink = unsafe.allocateInstance(Box.class);
                        break;
                    case "ctor":
                        sink = ctor.newInstance();
                        break;
                    case "threads":
                        if ((i & 7) == 0) {
                            Map<Thread, StackTraceElement[]> m = Thread.getAllStackTraces();
                            sink = m;
                        }
                        break;
                    default:
                        throw new IllegalArgumentException(mode);
                }
            } catch (OutOfMemoryError e) {
                doorOome++;
            }
            done++;
        }
        System.out.println("mode=" + mode + " door_oome=" + doorOome + " iters=" + done
                + " filled=" + !live.isEmpty());
        sink = live;
    }

    static void soft2(sun.misc.Unsafe unsafe) throws Exception {
        SoftReference<byte[]> soft = new SoftReference<>(new byte[16 << 20]);
        Thread churn = new Thread(() -> {
            while (!stop) {
                try {
                    sink = new byte[256];
                } catch (OutOfMemoryError e) {
                    // keep churning
                }
            }
        });
        churn.setDaemon(true);
        churn.start();
        ArrayList<Object> hold = new ArrayList<>();
        boolean oome = false;
        try {
            while (true) {
                hold.add(unsafe.allocateInstance(Box.class));
            }
        } catch (OutOfMemoryError e) {
            hold = null;
            oome = true;
        }
        stop = true;
        churn.join();
        System.out.println("oome " + oome);
        System.out.println("cleared " + (soft.get() == null));
    }
}
