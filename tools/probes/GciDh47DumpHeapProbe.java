// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.HotSpotDiagnosticMXBean;
import java.lang.management.ManagementFactory;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * gci i47w/dh47 ({@code gci-i47w-j45-hotspotdiagnostic-dumpheap0-unsatisfied-link-20261016}):
 * {@code HotSpotDiagnosticMXBean.dumpHeap(file, live)} against HotSpot 25.
 *
 * <p>Args: {@code [live|all]} (default {@code live}): the {@code live}
 * argument of every dump. The dump file is RELATIVE ({@code dh47_<pid>.hprof}
 * in the working directory; the pid keeps concurrent runs apart and is never
 * printed), removed at the end.
 *
 * <p>stdout (deterministic on both VMs; sizes are never printed):
 * <pre>
 * dh47 live=true
 * dh47 dump ok
 * dh47 exists=true
 * dh47 size_gt_0=true
 * dh47 magic_ok=true                 (the file starts "JAVA PROFILE 1.0.2")
 * dh47 again: java.io.IOException: File exists
 * dh47 untouched=true                (the refused dump left the file as it was)
 * dh47 suffix: java.lang.IllegalArgumentException: heapdump file must have .hprof extension
 * dh47 suffix_created=false
 * dh47 retained=true                 (an object held across the dumps is still reachable)
 * </pre>
 */
public class GciDh47DumpHeapProbe {
    // Something live across the dumps, so the live dump has objects to keep.
    static final List<int[]> HELD = new ArrayList<>();

    public static void main(String[] args) throws Exception {
        boolean live = args.length == 0 || !args[0].equals("all");
        for (int i = 0; i < 64; i++) {
            HELD.add(new int[256]);
        }
        HotSpotDiagnosticMXBean bean =
                ManagementFactory.getPlatformMXBean(HotSpotDiagnosticMXBean.class);
        long pid = ProcessHandle.current().pid();
        Path out = Path.of("dh47_" + pid + ".hprof");
        Path bad = Path.of("dh47_" + pid + ".bin");
        Files.deleteIfExists(out);
        Files.deleteIfExists(bad);
        System.out.println("dh47 live=" + live);
        try {
            try {
                bean.dumpHeap(out.toString(), live);
                System.out.println("dh47 dump ok");
            } catch (Throwable t) {
                System.out.println("dh47 dump: " + t.getClass().getName() + ": " + t.getMessage());
            }
            boolean exists = Files.exists(out);
            long size = exists ? Files.size(out) : -1;
            System.out.println("dh47 exists=" + exists);
            System.out.println("dh47 size_gt_0=" + (size > 0));
            byte[] head = exists ? readHead(out, 18) : new byte[0];
            System.out.println("dh47 magic_ok="
                    + Arrays.equals(head, "JAVA PROFILE 1.0.2".getBytes(StandardCharsets.US_ASCII)));

            try {
                bean.dumpHeap(out.toString(), live);
                System.out.println("dh47 again: no exception");
            } catch (Throwable t) {
                System.out.println("dh47 again: " + t.getClass().getName() + ": " + t.getMessage());
            }
            long after = Files.exists(out) ? Files.size(out) : -1;
            System.out.println("dh47 untouched=" + (exists && after == size
                    && Arrays.equals(readHead(out, 18), head)));

            try {
                bean.dumpHeap(bad.toString(), live);
                System.out.println("dh47 suffix: no exception");
            } catch (Throwable t) {
                System.out.println("dh47 suffix: " + t.getClass().getName() + ": " + t.getMessage());
            }
            System.out.println("dh47 suffix_created=" + Files.exists(bad));
            System.out.println("dh47 retained=" + (HELD.size() == 64 && HELD.get(63).length == 256));
        } finally {
            Files.deleteIfExists(out);
            Files.deleteIfExists(bad);
        }
    }

    private static byte[] readHead(Path p, int n) throws Exception {
        try (var in = Files.newInputStream(p)) {
            return in.readNBytes(n);
        }
    }
}
