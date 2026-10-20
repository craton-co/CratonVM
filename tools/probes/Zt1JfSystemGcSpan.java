// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayList;
import java.util.List;

import jdk.jfr.Configuration;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

/**
 * zt t1/jf (residual R1, proposal
 * {@code zgc-zt1jf-proposal-system-gc-row-from-the-door-20261019}): does
 * {@code jdk.SystemGC} span the collection only, as HotSpot 25's
 * {@code JVM_GC} commits it, or also the work this VM's {@code System.gc()}
 * door does after it (finalizers run by the door)? Each round makes 10 000
 * finalizable objects whose {@code finalize} sleeps a little in total, then
 * calls {@code System.gc()}.
 *
 * <pre>
 *   java -XX:+UseZGC -Xmx256m Zt1JfSystemGcSpan
 *   cratonvm         -Xmx256m Zt1JfSystemGcSpan
 * </pre>
 *
 * <p>stdout: per explicit request, whether its {@code jdk.SystemGC} ends no
 * later than 1 ms after the end of the last {@code jdk.GarbageCollection}
 * (cause {@code System.gc()}) that started inside it. stderr: the gaps.
 */
public class Zt1JfSystemGcSpan {
    static final class Fin {
        static volatile int finalized;
        final byte[] pad = new byte[32];

        @Override
        @SuppressWarnings({"deprecation", "removal"})
        protected void finalize() {
            if ((++finalized & 1023) == 0) {
                try {
                    Thread.sleep(2);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            }
        }
    }

    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        Recording r = new Recording(Configuration.getConfiguration("default"));
        r.enable("jdk.SystemGC").withThreshold(Duration.ZERO);
        r.enable("jdk.GarbageCollection").withThreshold(Duration.ZERO);
        r.start();
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < 10_000; i++) {
                sink = new Fin();
            }
            sink = null;
            System.gc();
        }
        r.stop();
        Path p = Files.createTempFile("zt1jfsgc", ".jfr");
        r.dump(p);
        r.close();
        List<RecordedEvent> events = RecordingFile.readAllEvents(p);
        Files.deleteIfExists(p);
        List<RecordedEvent> requests = new ArrayList<>();
        List<RecordedEvent> gcs = new ArrayList<>();
        for (RecordedEvent e : events) {
            String t = e.getEventType().getName();
            if (t.equals("jdk.SystemGC")) {
                requests.add(e);
            } else if (t.equals("jdk.GarbageCollection") && "System.gc()".equals(e.getString("cause"))) {
                gcs.add(e);
            }
        }
        boolean all = !requests.isEmpty();
        for (RecordedEvent s : requests) {
            long sEnd = nanos(s.getEndTime());
            long gEnd = Long.MIN_VALUE;
            for (RecordedEvent g : gcs) {
                long gs = nanos(g.getStartTime());
                if (gs >= nanos(s.getStartTime()) - 1_000_000 && gs <= sEnd) {
                    gEnd = Math.max(gEnd, nanos(g.getEndTime()));
                }
            }
            boolean ok = gEnd != Long.MIN_VALUE && sEnd <= gEnd + 1_000_000;
            all &= ok;
            System.err.println("info request gap_us=" + (gEnd == Long.MIN_VALUE ? "none" : (sEnd - gEnd) / 1_000));
        }
        System.out.println("system_gc requests=" + requests.size() + " ends_with_collection=" + all);
    }

    static long nanos(Instant i) {
        return i.getEpochSecond() * 1_000_000_000L + i.getNano();
    }
}
