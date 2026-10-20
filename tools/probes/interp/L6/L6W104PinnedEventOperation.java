// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 104, lane L6 -- the `blockingOperation` of a
// `jdk.VirtualThreadPinned` event. JDK 25's `VirtualThread.parkOnCarrierThread`
// (a park whose yield was pinned) calls `postPinnedEvent("LockSupport.park")`,
// and HotSpot's `JVM_VirtualThreadPinnedEvent` commits the event with that
// string as `blockingOperation`. CratonVM's `postPinnedEvent` native mapped
// every string but three to the literal "VirtualThread pinned".
//
// The pin: a virtual thread parks inside a class initializer (HotSpot cannot
// freeze a VM call to `<clinit>`; neither does CratonVM unmount there). The
// event's `pinnedReason` is not printed (HotSpot: "VM call to
// L6W104PinnedEventOperation$Init.<clinit> on stack"; CratonVM's text
// differs, see the page).
//
// Run: cratonvm [--compatible] L6W104PinnedEventOperation
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// init value=7
// pinned events seen: true
// blockingOperation=LockSupport.park
// done
// HOTSPOT_EXPECTED_END

import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.concurrent.locks.LockSupport;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

public class L6W104PinnedEventOperation {
    static final class Init {
        static final int VALUE;

        static {
            LockSupport.parkNanos(30_000_000L);
            VALUE = 7;
        }
    }

    public static void main(String[] args) throws Exception {
        Path out = Files.createTempFile("l6w104pin", ".jfr");
        int[] seen = new int[1];
        try (Recording r = new Recording()) {
            r.enable("jdk.VirtualThreadPinned").withThreshold(Duration.ZERO).withoutStackTrace();
            r.start();
            Thread vt = Thread.ofVirtual().start(() -> seen[0] = Init.VALUE);
            vt.join();
            r.stop();
            r.dump(out);
        }
        System.out.println("init value=" + seen[0]);
        String op = null;
        int n = 0;
        for (RecordedEvent e : RecordingFile.readAllEvents(out)) {
            if (e.getEventType().getName().equals("jdk.VirtualThreadPinned")) {
                n++;
                if (op == null && e.hasField("blockingOperation")) {
                    op = e.getString("blockingOperation");
                }
            }
        }
        Files.deleteIfExists(out);
        System.out.println("pinned events seen: " + (n > 0));
        System.out.println("blockingOperation=" + op);
        System.out.println("done");
    }
}
