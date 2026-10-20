import jdk.jfr.*;
import jdk.jfr.consumer.*;
import java.nio.file.*;
import java.time.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

/** SBR5 jfr: broader API surface, diffed vs HotSpot. */
public class Sbr5JfrC {
    @Name("probe.C") @StackTrace(false) static class C extends Event { int n; String s; }
    @Name("probe.Per") @jdk.jfr.Period("100 ms") @StackTrace(false) static class Per extends Event { int n; }
    static void p(String s) { System.out.println(s); }
    static int count(Path f, String name) throws Exception {
        int c = 0;
        try (RecordingFile rf = new RecordingFile(f)) { while (rf.hasMoreEvents()) if (rf.readEvent().getEventType().getName().equals(name)) c++; }
        return c;
    }
    public static void main(String[] a) throws Exception {
        Path dir = Files.createTempDirectory("sbr5c");
        // 1. memory recording, destination + dumpOnExit semantics
        try (Recording r = new Recording()) {
            r.setToDisk(false); r.setName("mem"); r.enable("probe.C");
            Path dest = dir.resolve("mem.jfr"); r.setDestination(dest);
            r.start();
            for (int i = 0; i < 5; i++) { C c = new C(); c.n = i; c.s = "m" + i; c.commit(); }
            r.stop();
            p("mem count=" + count(dest, "probe.C") + " dest exists=" + Files.exists(dest) + " name=" + r.getName() + " toDisk=" + r.isToDisk());
        }
        // 2. two overlapping recordings, one disk one memory
        try (Recording r1 = new Recording(); Recording r2 = new Recording()) {
            r1.enable("probe.C"); r2.enable("probe.C"); r2.setToDisk(false);
            Path f1 = dir.resolve("r1.jfr"), f2 = dir.resolve("r2.jfr"); r2.setDestination(f2); r1.start(); new C().commit(); r2.start(); new C().commit(); new C().commit();
            r1.stop(); new C().commit(); r2.stop();
            
            r1.dump(f1);
            p("overlap r1=" + count(f1, "probe.C") + " r2=" + count(f2, "probe.C"));
        }
        // 3. periodic event via annotation hook
        AtomicInteger ticks = new AtomicInteger();
        Runnable hook = () -> { Per e = new Per(); e.n = ticks.incrementAndGet(); e.commit(); };
        FlightRecorder.addPeriodicEvent(Per.class, hook);
        try (Recording r = new Recording()) {
            r.enable("probe.Per").withPeriod(Duration.ofMillis(100));
            r.start(); Thread.sleep(650); r.stop();
            Path f = dir.resolve("per.jfr"); r.dump(f);
            int c = count(f, "probe.Per");
            p("periodic >=3=" + (c >= 3) + " <=12=" + (c <= 12));
        }
        FlightRecorder.removePeriodicEvent(hook);
        // 4. virtual threads committing
        try (Recording r = new Recording()) {
            r.enable("probe.C"); r.start();
            try (var ex = Executors.newVirtualThreadPerTaskExecutor()) {
                for (int i = 0; i < 20; i++) { int k = i; ex.submit(() -> { C c = new C(); c.n = k; c.commit(); }); }
            }
            r.stop(); Path f = dir.resolve("vt.jfr"); r.dump(f);
            p("virtual count=" + count(f, "probe.C"));
        }
        // 5. many threads concurrently
        try (Recording r = new Recording()) {
            r.enable("probe.C"); r.start();
            Thread[] ts = new Thread[8];
            for (int t = 0; t < ts.length; t++) { ts[t] = new Thread(() -> { for (int i = 0; i < 500; i++) { C c = new C(); c.n = i; c.commit(); } }); ts[t].start(); }
            for (Thread t : ts) t.join();
            r.stop(); Path f = dir.resolve("mt.jfr"); r.dump(f);
            p("multithread count=" + count(f, "probe.C"));
        }
        // 6. configuration + settings
        Configuration cfg = Configuration.getConfiguration("default");
        p("config name=" + cfg.getName() + " settings>0=" + (cfg.getSettings().size() > 0));
        try (Recording r = new Recording(cfg)) {
            r.start(); Thread.sleep(300); r.stop(); Path f = dir.resolve("cfg.jfr"); r.dump(f);
            p("config recording has ActiveRecording=" + (count(f, "jdk.ActiveRecording") >= 1) + " ActiveSetting>0=" + (count(f, "jdk.ActiveSetting") > 0));
        }
        // 7. EventStream.openFile + onMetadata/onFlush with RecordingStream, setMaxAge
        try (RecordingStream rs = new RecordingStream()) {
            rs.enable("probe.C");
            CountDownLatch flush = new CountDownLatch(1); AtomicInteger got = new AtomicInteger();
            rs.onEvent("probe.C", e -> got.incrementAndGet()); rs.onFlush(flush::countDown); rs.setSettings(Map.of("probe.C#enabled", "true"));
            rs.startAsync();
            for (int i = 0; i < 10; i++) new C().commit();
            p("flush seen=" + flush.await(10, TimeUnit.SECONDS));
            Thread.sleep(800);
            p("stream got=" + got.get());
        }
        // 8. file stream
        Path ff = dir.resolve("fs.jfr");
        try (Recording r = new Recording()) { r.enable("probe.C"); r.start(); for (int i = 0; i < 3; i++) new C().commit(); r.stop(); r.dump(ff); }
        AtomicInteger fsn = new AtomicInteger();
        try (EventStream es = EventStream.openFile(ff)) { es.onEvent("probe.C", e -> fsn.incrementAndGet()); es.start(); }
        p("openFile count=" + fsn.get());
        // 9. FlightRecorder.getRecordings and snapshot
        try (Recording r = new Recording()) {
            r.enable("probe.C"); r.start(); new C().commit();
            p("recordings=" + FlightRecorder.getFlightRecorder().getRecordings().size());
            try (Recording snap = FlightRecorder.getFlightRecorder().takeSnapshot()) {
                Path f = dir.resolve("snap.jfr"); snap.dump(f); p("snapshot count=" + count(f, "probe.C") + " state=" + snap.getState());
            }
            r.stop();
        }
        p("state check done");
    }
}
