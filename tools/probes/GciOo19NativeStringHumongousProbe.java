// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.Collections;
import java.util.Set;
import java.util.TreeSet;

/**
 * gci i19/oo: humongous Strings built by a String factory (`String.repeat`,
 * whose CratonVM native, when it serves the call, builds the result through
 * the native String door `create_string_from_units_gc_safe`), churned on a
 * small G1 heap until the heap refuses one and the door collects. HotSpot 25
 * G1 names a humongous request's collection {@code G1 Humongous Allocation}
 * (page docs/internal/gc/gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018.md,
 * item 4's residual (a)).
 *
 * <p>Modes (first argument): {@code latin1} ({@code "ab".repeat(400000)}, an
 * 800 000-byte Latin-1 value) and {@code utf16} ({@code "Āb".repeat(200000)},
 * the same bytes as UTF-16). 200 Strings each.
 *
 * <p>stdout (deterministic, the same on HotSpot 25 G1 at -Xmx64m):
 * <pre>
 * mode latin1
 * stringhum length 800000
 * stringhum humongous-cause-seen true
 * PROBE-OK
 * </pre>
 * stderr: {@code [gci-oo19] causes=...}.
 */
public class GciOo19NativeStringHumongousProbe {
    static volatile Object sink;

    static final Set<String> CAUSES = Collections.synchronizedSet(new TreeSet<String>());

    static void listenCauses() {
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            javax.management.NotificationListener l = (n, handback) -> {
                if (com.sun.management.GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION
                        .equals(n.getType())) {
                    com.sun.management.GarbageCollectionNotificationInfo info =
                            com.sun.management.GarbageCollectionNotificationInfo.from(
                                    (javax.management.openmbean.CompositeData) n.getUserData());
                    CAUSES.add(info.getGcCause());
                }
            };
            ((javax.management.NotificationEmitter) b).addNotificationListener(l, null, null);
        }
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "latin1";
        System.out.println("mode " + mode);
        String unit;
        int count;
        if (mode.equals("latin1")) {
            unit = "ab";
            count = 400_000;
        } else if (mode.equals("utf16")) {
            unit = "Āb";
            count = 200_000;
        } else {
            System.out.println("unknown mode " + mode);
            return;
        }
        listenCauses();
        int length = 0;
        for (int i = 0; i < 200; i++) {
            String s = unit.repeat(count);
            length = s.length();
            sink = s;
        }
        sink = null;
        try {
            Thread.sleep(1000); // notifications are delivered asynchronously
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        boolean seen;
        String all;
        synchronized (CAUSES) {
            seen = CAUSES.contains("G1 Humongous Allocation");
            all = CAUSES.toString();
        }
        System.out.println("stringhum length " + (mode.equals("latin1") ? length : length * 2));
        System.out.println("stringhum humongous-cause-seen " + seen);
        System.err.println("[gci-oo19] " + mode + " causes=" + all);
        System.out.println("PROBE-OK");
    }
}
