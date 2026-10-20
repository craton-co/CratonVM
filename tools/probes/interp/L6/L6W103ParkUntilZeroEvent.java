// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 103, lane L6 -- the `jdk.ThreadPark` event of a
// `time == 0` park (page `i102-L6-park-until-zero-parks-forever`, residue).
//
// HotSpot 25 `Unsafe_Park`: `if (time == 0) post_thread_park_event(&event,
// obj, min_jlong, min_jlong)`, else an absolute park posts (MIN, time) and a
// relative one (time, MIN). So `parkUntil(0)` reports timeout = until =
// Long.MIN_VALUE; CratonVM reported timeout 0 (its zero wait's duration).
// The rows: parkUntil(0), parkUntil(1) (a past deadline), and park() with a
// pending permit (a relative 0).
//
// Run: cratonvm [--compatible] L6W103ParkUntilZeroEvent
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// events=3
// parkUntil(0): timeout=MIN until=MIN
// parkUntil(1): timeout=MIN until=1
// park() with permit: timeout=MIN until=MIN
// done
// HOTSPOT_EXPECTED_END

import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.concurrent.locks.LockSupport;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedThread;
import jdk.jfr.consumer.RecordingFile;

public class L6W103ParkUntilZeroEvent {
    static final String PARKER = "w103-parker";

    static String f(long v) {
        return v == Long.MIN_VALUE ? "MIN" : String.valueOf(v);
    }

    public static void main(String[] args) throws Exception {
        Path out = Files.createTempFile("l6w103park", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.ThreadPark").withThreshold(Duration.ZERO).withoutStackTrace();
            r.start();
            Thread t = new Thread(() -> {
                LockSupport.parkUntil(0L);
                LockSupport.parkUntil(1L);
                LockSupport.unpark(Thread.currentThread());
                LockSupport.park();
            }, PARKER);
            t.start();
            t.join();
            r.stop();
            r.dump(out);
        }
        List<RecordedEvent> mine = new ArrayList<>();
        for (RecordedEvent e : RecordingFile.readAllEvents(out)) {
            if (!e.getEventType().getName().equals("jdk.ThreadPark")) {
                continue;
            }
            RecordedThread th = e.getThread("eventThread");
            if (th != null && PARKER.equals(th.getJavaName())) {
                mine.add(e);
            }
        }
        Files.deleteIfExists(out);
        mine.sort(Comparator.comparing(RecordedEvent::getStartTime));
        System.out.println("events=" + mine.size());
        String[] labels = {"parkUntil(0)", "parkUntil(1)", "park() with permit"};
        for (int i = 0; i < mine.size(); i++) {
            RecordedEvent e = mine.get(i);
            String label = i < labels.length ? labels[i] : "extra";
            System.out.println(label + ": timeout=" + f(e.getLong("timeout"))
                    + " until=" + f(e.getLong("until")));
        }
        System.out.println("done");
    }
}
