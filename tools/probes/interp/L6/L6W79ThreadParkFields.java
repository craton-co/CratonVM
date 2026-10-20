// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L6 -- probe for
// docs/internal/fixed-bugs/interpreter-i73-L6-jfr-thread-park-fields-differ-from-hotspot-FIXED-20261111.md:
// the fields HotSpot's Unsafe_Park posts on jdk.ThreadPark
// (post_thread_park_event(event, current_park_blocker(), timeout, until)):
//   * an absolute park (parkUntil): timeout = Long.MIN_VALUE, until = the
//     caller's deadline in ms (was: the remaining time as a relative timeout);
//   * parkedClass / address: the parkBlocker's class and address (were null
//     and 0 always);
//   * an interrupted park returns at once INSIDE the event (was: no event).
//
// Run, each mode:
//   cratonvm L6W79ThreadParkFields
//   cratonvm --compatible L6W79ThreadParkFields
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// events=3
// parkUntil: timeout=MIN until-matches=true parkedClass=null address-zero=true
// parkNanos(blocker): timeout=1000000 until=MIN parkedClass=L6W79ThreadParkFields$Blocker address-zero=false
// interrupted parkNanos: timeout=30000000 until=MIN returned-early=true
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
import jdk.jfr.consumer.RecordedClass;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedThread;
import jdk.jfr.consumer.RecordingFile;

public class L6W79ThreadParkFields {
    static final String PARKER = "w79-parker";
    static volatile long deadline;

    static final class Blocker {
    }

    public static void main(String[] args) throws Exception {
        Path out = Files.createTempFile("l6w79", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.ThreadPark").withThreshold(Duration.ZERO).withoutStackTrace();
            r.start();
            Thread t = new Thread(() -> {
                long d = System.currentTimeMillis() + 20;
                deadline = d;
                LockSupport.parkUntil(d);
                LockSupport.parkNanos(new Blocker(), 1_000_000L);
                Thread.currentThread().interrupt();
                LockSupport.parkNanos(30_000_000L);
                Thread.interrupted();
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
        for (RecordedEvent e : mine) {
            long timeout = e.getLong("timeout");
            long until = e.getLong("until");
            RecordedClass c = e.getValue("parkedClass");
            String cls = c == null ? "null" : c.getName();
            boolean addressZero = e.getLong("address") == 0L;
            String t = timeout == Long.MIN_VALUE ? "MIN" : String.valueOf(timeout);
            String u = until == Long.MIN_VALUE ? "MIN" : String.valueOf(until);
            if (until != Long.MIN_VALUE) {
                System.out.println("parkUntil: timeout=" + t + " until-matches=" + (until == deadline)
                        + " parkedClass=" + cls + " address-zero=" + addressZero);
            } else if (timeout == 30_000_000L) {
                System.out.println("interrupted parkNanos: timeout=" + t + " until=" + u
                        + " returned-early=" + (e.getDuration().toNanos() < 30_000_000L));
            } else {
                System.out.println("parkNanos(blocker): timeout=" + t + " until=" + u
                        + " parkedClass=" + cls + " address-zero=" + addressZero);
            }
        }
        System.out.println("done");
    }
}
