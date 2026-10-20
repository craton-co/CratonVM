// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.Map;
import java.util.TreeMap;
import java.util.concurrent.locks.LockSupport;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedClass;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

/**
 * gci i23/pk (finding 6 of gci-i22rv-i21-default-path-review-findings): the
 * `jdk.ThreadPark` `timeout` of a RELATIVE `Unsafe.park(false, time)` that
 * returns at once. HotSpot 25's `Unsafe_Park` posts a relative park as
 * (timeout = time, until = Long.MIN_VALUE), so a negative time is reported
 * as is; this VM reported 0 (its `Duration` could not carry the sign).
 *
 *   GciPk23ParkRawTimeoutProbe
 *
 * `sun.misc.Unsafe.park` is reached by reflection; each park names its own
 * blocker class (`LockSupport.setCurrentBlocker`), so the events are found by
 * `parkedClass`.
 *
 * stdout (deterministic, the same on HotSpot):
 *   pk23 neg found=1 timeout=-1 until=MIN
 *   pk23 min found=1 timeout=MIN until=MIN
 *   pk23 pos found=1 timeout=1000 until=MIN
 * stderr: the raw values.
 */
public class GciPk23ParkRawTimeoutProbe {
    static final class NegBlocker {}
    static final class MinBlocker {}
    static final class PosBlocker {}

    static String word(long v) {
        return v == Long.MIN_VALUE ? "MIN" : Long.toString(v);
    }

    public static void main(String[] args) throws Exception {
        Class<?> c = Class.forName("sun.misc.Unsafe");
        Field f = c.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        Object u = f.get(null);
        Method park = c.getMethod("park", boolean.class, long.class);
        Path out = Files.createTempFile("pk23park", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.ThreadPark").withThreshold(Duration.ZERO);
            r.start();
            LockSupport.setCurrentBlocker(new NegBlocker());
            park.invoke(u, false, -1L);
            LockSupport.setCurrentBlocker(new MinBlocker());
            park.invoke(u, false, Long.MIN_VALUE);
            LockSupport.setCurrentBlocker(new PosBlocker());
            park.invoke(u, false, 1_000L);
            LockSupport.setCurrentBlocker(null);
            r.stop();
            r.dump(out);
        }
        Map<String, String> seen = new TreeMap<>();
        Map<String, Integer> found = new TreeMap<>();
        try (RecordingFile rf = new RecordingFile(out)) {
            while (rf.hasMoreEvents()) {
                RecordedEvent e = rf.readEvent();
                if (!e.getEventType().getName().equals("jdk.ThreadPark")) {
                    continue;
                }
                RecordedClass parked = e.getClass("parkedClass");
                if (parked == null) {
                    continue;
                }
                String name = parked.getName();
                String tag;
                if (name.endsWith("$NegBlocker")) {
                    tag = "neg";
                } else if (name.endsWith("$MinBlocker")) {
                    tag = "min";
                } else if (name.endsWith("$PosBlocker")) {
                    tag = "pos";
                } else {
                    continue;
                }
                long timeout = e.getLong("timeout");
                long until = e.getLong("until");
                System.err.println("[pk23] " + tag + " timeout=" + timeout + " until=" + until);
                found.merge(tag, 1, Integer::sum);
                seen.put(tag, "timeout=" + word(timeout) + " until=" + word(until));
            }
        }
        Files.deleteIfExists(out);
        for (String tag : new String[] {"neg", "min", "pos"}) {
            System.out.println("pk23 " + tag + " found=" + found.getOrDefault(tag, 0) + " "
                    + seen.getOrDefault(tag, "none"));
        }
    }
}
