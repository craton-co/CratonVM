// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.Map;
import java.util.concurrent.locks.LockSupport;

/**
 * zr z11/nd: natives whose allocation doors make ONE attempt, driven on a
 * near-full heap. Their refusal used to be the {@code OutOfMemoryError}; on
 * ZGC the native funnel now collects and re-runs a restartable native
 * ({@code CRATONVM_ZGC_NATIVE_OOM_REPLAY}, default ON, {@code =0} kill).
 *
 * <p>The live set is grown with 64 KiB chunks until about {@code freeKb} KiB
 * of the heap is left (measured from {@code Runtime} after a
 * {@code System.gc()}, as {@code Zr8NrNativeDoors} does, so HotSpot finishes:
 * a fill to the error costs HotSpot ZGC minutes of allocation stalls). The
 * churn loop then makes its garbage through the door under test:
 *
 * <ul>
 *   <li>{@code dump}: {@code Thread.getAllStackTraces()} every 8th iteration
 *       ({@code Thread.dumpThreads}: one {@code StackTraceElement[]} per
 *       thread, three Strings per frame) -- the z9a jdk-only residual of
 *       {@code Zr8NrNativeDoors threads};
 *   <li>{@code otherstack}: {@code parked.getStackTrace()} of a parked helper
 *       thread every 2nd iteration ({@code Thread.getStackTrace0});
 *   <li>{@code declclasses}: {@code Zr11NdNativeReplay.class.getDeclaredClasses()}
 *       ({@code Class.getDeclaredClasses0}, uncached by the JDK);
 *   <li>{@code tostring}: {@code Long.toString(i)} and {@code Integer.toString(i)}
 *       (a native in {@code --compatible}, JDK bytecode in {@code --jdk-only});
 *   <li>{@code mixed}: all four, in turn.
 * </ul>
 *
 * <p>HotSpot collects for every refused allocation: {@code door_oome=0} on
 * every mode. Usage: {@code java -Xmx64m Zr11NdNativeReplay <mode> [freeKb] [iters]}
 * (defaults 1024 and 20000).
 */
public class Zr11NdNativeReplay {
    static volatile Object sink;
    static volatile boolean stop;

    static final class A {}

    static final class B {}

    static final class C {}

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "dump";
        int freeKb = args.length > 1 ? Integer.parseInt(args[1]) : 1024;
        int iters = args.length > 2 ? Integer.parseInt(args[2]) : 20_000;
        Thread parked = new Thread(() -> {
            while (!stop) {
                LockSupport.parkNanos(1_000_000L);
            }
        }, "zr11nd-parked");
        parked.setDaemon(true);
        parked.start();
        // Warm every door before the heap is full (class init, linkage).
        for (int i = 0; i < 64; i++) {
            sink = Thread.getAllStackTraces();
            sink = parked.getStackTrace();
            sink = Zr11NdNativeReplay.class.getDeclaredClasses();
            sink = Long.toString(i) + Integer.toString(i);
        }
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
        int bad = 0;
        for (int i = 0; i < iters; i++) {
            String m = mode.equals("mixed")
                    ? new String[] {"dump", "otherstack", "declclasses", "tostring"}[i & 3]
                    : mode;
            try {
                switch (m) {
                    case "dump":
                        if ((i & 7) == 0) {
                            Map<Thread, StackTraceElement[]> traces = Thread.getAllStackTraces();
                            if (!traces.containsKey(Thread.currentThread())) {
                                bad++;
                            }
                            sink = traces;
                        }
                        break;
                    case "otherstack":
                        if ((i & 1) == 0) {
                            StackTraceElement[] st = parked.getStackTrace();
                            if (st.length == 0) {
                                bad++;
                            }
                            sink = st;
                        }
                        break;
                    case "declclasses": {
                        Class<?>[] dc = Zr11NdNativeReplay.class.getDeclaredClasses();
                        if (dc.length != 3) {
                            bad++;
                        }
                        sink = dc;
                        break;
                    }
                    case "tostring": {
                        String s = Long.toString(1_000_000_000_000L + i);
                        String t = Integer.toString(1_000_000 + i);
                        if (s.length() != 13 || t.length() != 7) {
                            bad++;
                        }
                        sink = s;
                        sink = t;
                        break;
                    }
                    default:
                        throw new IllegalArgumentException(mode);
                }
            } catch (OutOfMemoryError e) {
                doorOome++;
            }
            done++;
        }
        stop = true;
        System.out.println("mode=" + mode + " door_oome=" + doorOome + " bad=" + bad
                + " iters=" + done + " filled=" + !live.isEmpty());
        sink = live;
    }
}
