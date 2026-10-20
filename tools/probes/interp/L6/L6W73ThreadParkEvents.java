// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 73, lane L6 -- behavioural probe for
// docs/internal/fixed-bugs/interpreter-i72-L6-jfr-thread-park-misses-the-lazy-park-phase-FIXED-20261106.md:
// a jdk.ThreadPark event spans the WHOLE park (permit fast path, the running
// phase of CRATONVM_PARK_LAZY, the blocking wait) and carries the caller's
// timeout, and the recording's threshold filters on that whole duration, as
// HotSpot's Unsafe_Park posts it.
//
// Before the fix, with CRATONVM_PARK_LAZY on (the default since wave 71):
// the parks shorter than the 2 ms budget had no event at all, the 5 ms park's
// event lacked the phase (about 3 ms, timeout field 3 000 000 -- the
// remainder), so the 4 ms-threshold recording dropped it.
//
// Run, each mode, the switch both ways:
//   cratonvm L6W73ThreadParkEvents
//   CRATONVM_PARK_LAZY=0 cratonvm L6W73ThreadParkEvents
//   cratonvm --compatible L6W73ThreadParkEvents
//   CRATONVM_PARK_LAZY=0 cratonvm --compatible L6W73ThreadParkEvents
//
// Interpreter round i4 wave 96, lane L6: the 500 us park's line no longer
// prints whether the event's duration covers the timeout. HotSpot's event
// spans `Parker::park` (`Unsafe_Park` constructs `EventThreadPark` before it
// and commits after), and that answer is the host's: on Linux the park waits
// out an absolute `pthread_cond_timedwait` deadline (never early: true); on
// Windows `Parker::park` coarsens a relative park to whole milliseconds
// (`time /= 1000000`, 0 -> 1) and waits with `WaitForSingleObject`, which
// may time out early when the interval is below the clock's resolution --
// the Windows battery's HotSpot printed `covers-timeout=false`, the Linux
// expectation below `true`. The line now says only that the short park's
// event exists with the caller's fields (the wave-73 defect: it had none).
//
// Interpreter round i4 wave 99, lane L6 (page
// i98-L6-windows-parknanos-whole-milliseconds): no line prints a duration
// against a timeout any more. On Windows, HotSpot's `Parker::park` does ONE
// `WaitForSingleObject` of whole milliseconds at the default timer
// resolution and returns, so a park may end up to a clock tick (~15.6 ms)
// early or late; CratonVM on Windows now waits the same way. The output
// keeps only what both OSes guarantee: each park has one event carrying the
// caller's timeout and until fields (the wave-73 defect: the 5 ms park's
// field was the remainder after the running phase), the permit fast path is
// far shorter than its timeout, and a threshold filters on the whole park.
// The threshold row is 50 ms against parks of 0.5, 5 and 100 ms: a tick
// either way cannot move the 100 ms park below it or the 5 ms one above it.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags, Linux and Windows)
// threshold-0 events=5
// timeout=-9223372036854775808 until=MIN untimed-unparked
// timeout=500000 until=MIN recorded
// timeout=5000000 until=MIN recorded
// timeout=50000000 until=MIN permit-fast-path=true
// timeout=100000000 until=MIN recorded
// threshold-50ms events=1
// timeout=100000000 until=MIN recorded
// done
// HOTSPOT_EXPECTED_END
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.TreeMap;
import java.util.concurrent.locks.LockSupport;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedThread;
import jdk.jfr.consumer.RecordingFile;

public class L6W73ThreadParkEvents {
    static final String PARKER = "w73-parker";
    static volatile boolean ready;

    /** The parks of one recording, all on a thread named PARKER. */
    static void parks(boolean all) throws InterruptedException {
        ready = false;
        Thread t = new Thread(() -> {
            // Shorter than the 2 ms running-phase budget: ends in the phase.
            LockSupport.parkNanos(500_000L);
            // Longer than the budget: phase, then the blocking wait.
            LockSupport.parkNanos(5_000_000L);
            // Well past the 50 ms threshold even if it ends a tick early.
            LockSupport.parkNanos(100_000_000L);
            if (all) {
                // The permit is there: the fast path.
                LockSupport.unpark(Thread.currentThread());
                LockSupport.parkNanos(50_000_000L);
                // Untimed, unparked by main (one park() call, one event).
                ready = true;
                LockSupport.park();
            }
        }, PARKER);
        t.start();
        if (all) {
            // Once it is about to park untimed, unpark until it is done: a
            // permit given before that park ends it at once, one event too.
            while (!ready) {
                Thread.sleep(1);
            }
            while (t.isAlive()) {
                Thread.sleep(1);
                LockSupport.unpark(t);
            }
        }
        t.join();
    }

    static List<RecordedEvent> record(Duration threshold, boolean all) throws Exception {
        Path out = Files.createTempFile("l6w73", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.ThreadPark").withThreshold(threshold).withoutStackTrace();
            r.start();
            parks(all);
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
        return mine;
    }

    static void report(String name, List<RecordedEvent> events) {
        System.out.println(name + " events=" + events.size());
        TreeMap<Long, String> lines = new TreeMap<>();
        for (RecordedEvent e : events) {
            long timeout = e.getLong("timeout");
            long until = e.getLong("until");
            long nanos = e.getDuration().toNanos();
            String u = until == Long.MIN_VALUE ? "MIN" : String.valueOf(until);
            String what;
            if (timeout == Long.MIN_VALUE) {
                what = "untimed-unparked";
            } else if (timeout == 50_000_000L) {
                what = "permit-fast-path=" + (nanos < 50_000_000L);
            } else {
                // The duration of a timed park is the host's (see the header).
                what = "recorded";
            }
            lines.put(timeout, "timeout=" + timeout + " until=" + u + " " + what);
        }
        lines.values().forEach(System.out::println);
    }

    public static void main(String[] args) throws Exception {
        report("threshold-0", record(Duration.ZERO, true));
        report("threshold-50ms", record(Duration.ofMillis(50), false));
        System.out.println("done");
    }
}
