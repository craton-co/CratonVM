// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 105, lane L6 -- the other fields of a
// `jdk.VirtualThreadPinned` event (page
// `i104-L6-virtual-thread-pinned-event-fields`). HotSpot 25's
// `JVM_VirtualThreadPinnedEvent` commits an untimed event that starts at the
// thread's last failed freeze (so `duration` covers the pinned park), and
// `JavaThread::post_vthread_pinned_event` writes `pinnedReason` = "VM call to
// <K>.<clinit> on stack" while a class initializer runs, and `carrierThread` =
// the carrier (a ForkJoinPool worker, a platform thread). CratonVM wrote
// "Native frame or <clinit> on stack", the virtual thread as its own carrier,
// and a zero duration (so the default 20 ms threshold dropped every event).
//
// The second recording keeps the default threshold (20 ms): HotSpot still
// records the 60 ms pinned park.
//
// Run: cratonvm [--compatible] L6W105PinnedEventFields
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// init value=7
// zero threshold: events=true
// pinnedReason=VM call to L6W105PinnedEventFields$Init.<clinit> on stack
// blockingOperation=LockSupport.park
// eventThread virtual=true
// carrierThread virtual=false
// carrierThread is a ForkJoinPool worker=true
// duration>=20ms=true
// default threshold: events=true
// done
// HOTSPOT_EXPECTED_END

import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.List;
import java.util.concurrent.locks.LockSupport;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedThread;
import jdk.jfr.consumer.RecordingFile;

public class L6W105PinnedEventFields {
    static final class Init {
        static final int VALUE;

        static {
            LockSupport.parkNanos(30_000_000L);
            VALUE = 7;
        }
    }

    static final class Init2 {
        static final int VALUE;

        static {
            LockSupport.parkNanos(60_000_000L);
            VALUE = 8;
        }
    }

    static List<RecordedEvent> pinned(Path out) throws Exception {
        return RecordingFile.readAllEvents(out).stream()
                .filter(e -> e.getEventType().getName().equals("jdk.VirtualThreadPinned"))
                .toList();
    }

    public static void main(String[] args) throws Exception {
        Path out = Files.createTempFile("l6w105pin", ".jfr");
        int[] seen = new int[2];
        try (Recording r = new Recording()) {
            r.enable("jdk.VirtualThreadPinned").withThreshold(Duration.ZERO).withoutStackTrace();
            r.start();
            Thread vt = Thread.ofVirtual().start(() -> seen[0] = Init.VALUE);
            vt.join();
            r.stop();
            r.dump(out);
        }
        System.out.println("init value=" + seen[0]);
        List<RecordedEvent> events = pinned(out);
        System.out.println("zero threshold: events=" + !events.isEmpty());
        if (!events.isEmpty()) {
            RecordedEvent e = events.get(0);
            System.out.println("pinnedReason=" + e.getString("pinnedReason"));
            System.out.println("blockingOperation=" + e.getString("blockingOperation"));
            RecordedThread et = e.getThread("eventThread");
            RecordedThread ct = e.getThread("carrierThread");
            System.out.println("eventThread virtual=" + (et != null && et.isVirtual()));
            System.out.println("carrierThread virtual=" + (ct == null ? "null" : ct.isVirtual()));
            String cn = ct == null ? null : ct.getJavaName();
            System.out.println("carrierThread is a ForkJoinPool worker="
                    + (cn != null && cn.startsWith("ForkJoinPool-") && cn.contains("-worker-")));
            System.out.println("duration>=20ms=" + (e.getDuration().toMillis() >= 20));
        }
        Files.deleteIfExists(out);

        Path out2 = Files.createTempFile("l6w105pin2", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.VirtualThreadPinned").withoutStackTrace();
            r.start();
            Thread vt = Thread.ofVirtual().start(() -> seen[1] = Init2.VALUE);
            vt.join();
            r.stop();
            r.dump(out2);
        }
        System.out.println("default threshold: events=" + !pinned(out2).isEmpty());
        Files.deleteIfExists(out2);
        System.out.println("done");
    }
}
