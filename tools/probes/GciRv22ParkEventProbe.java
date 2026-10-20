// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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
 * gci i22/rv (finding 4 of gci-i22rv-i21-default-path-review-findings): the
 * `jdk.ThreadPark` event fields of an absolute `Unsafe.park` that returns at
 * once, against HotSpot 25's `Unsafe_Park`, which posts `time == 0` as an
 * untimed park (timeout = until = Long.MIN_VALUE) and any other absolute
 * time as (timeout = MIN, until = time). i21/qh's absolute door posted
 * `until = 0` for `parkUntil(0)`.
 *
 *   GciRv22ParkEventProbe
 *
 * Each park names its own blocker class, so the events are found by
 * `parkedClass` whatever else parks on the main thread.
 *
 * stdout (deterministic, the same on HotSpot):
 *   rv22 zero found=1 timeout=MIN until=MIN
 *   rv22 minus-one found=1 timeout=MIN until=-1
 *   rv22 past found=1 timeout=MIN until=deadline
 * stderr: the raw values.
 */
public class GciRv22ParkEventProbe {
    static final class ZeroBlocker {}
    static final class MinusOneBlocker {}
    static final class PastBlocker {}

    static String word(long v) {
        if (v == Long.MIN_VALUE) {
            return "MIN";
        }
        return v > 0 ? "deadline" : Long.toString(v);
    }

    public static void main(String[] args) throws Exception {
        Path out = Files.createTempFile("rv22park", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.ThreadPark").withThreshold(Duration.ZERO);
            r.start();
            LockSupport.parkUntil(new ZeroBlocker(), 0L);
            LockSupport.parkUntil(new MinusOneBlocker(), -1L);
            LockSupport.parkUntil(new PastBlocker(), System.currentTimeMillis() - 5L);
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
                if (name.endsWith("$ZeroBlocker")) {
                    tag = "zero";
                } else if (name.endsWith("$MinusOneBlocker")) {
                    tag = "minus-one";
                } else if (name.endsWith("$PastBlocker")) {
                    tag = "past";
                } else {
                    continue;
                }
                long timeout = e.getLong("timeout");
                long until = e.getLong("until");
                System.err.println("[rv22] " + tag + " timeout=" + timeout + " until=" + until);
                found.merge(tag, 1, Integer::sum);
                seen.put(tag, "timeout=" + word(timeout) + " until=" + word(until));
            }
        }
        Files.deleteIfExists(out);
        for (String tag : new String[] {"zero", "minus-one", "past"}) {
            System.out.println("rv22 " + tag + " found=" + found.getOrDefault(tag, 0) + " "
                    + seen.getOrDefault(tag, "none"));
        }
    }
}
