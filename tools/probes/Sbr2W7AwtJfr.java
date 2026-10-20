import jdk.jfr.*;
import jdk.jfr.consumer.*;
import java.util.concurrent.*;

/** SBR2 W7 AWT: FlightRecorder / RecordingStream / Recording, diffed against HotSpot. */
public class Sbr2W7AwtJfr {
    static void p(String s) { System.out.println(s); }
    static void t(String n, Callable<Object> c) {
        try { p(n + " = " + c.call()); } catch (Throwable e) { p(n + " THROWS " + e.getClass().getName() + ": " + e.getMessage()); }
    }
    @Name("probe.Ev") @Label("Probe") @StackTrace(false)
    static class Ev extends Event { @Label("n") int n; String s; }
    public static void main(String[] a) throws Exception {
        t("available", () -> FlightRecorder.isAvailable());
        t("recorder", () -> FlightRecorder.getFlightRecorder().getClass().getName());
        t("types>0", () -> FlightRecorder.getFlightRecorder().getEventTypes().size() > 0);
        t("evtype", () -> EventType.getEventType(Ev.class).getName() + " " + EventType.getEventType(Ev.class).getLabel());
        t("fields", () -> { StringBuilder sb = new StringBuilder(); for (ValueDescriptor v : EventType.getEventType(Ev.class).getFields()) sb.append(v.getName()).append(':').append(v.getTypeName()).append(' '); return sb; });
        CountDownLatch got = new CountDownLatch(1);
        t("stream", () -> {
            try (RecordingStream rs = new RecordingStream()) {
                rs.enable("probe.Ev");
                rs.onEvent("probe.Ev", e -> { p("event n=" + e.getInt("n") + " s=" + e.getString("s")); got.countDown(); });
                rs.startAsync();
                Ev e = new Ev(); e.n = 7; e.s = "hi"; e.commit();
                boolean ok = got.await(10, TimeUnit.SECONDS);
                return "received=" + ok;
            }
        });
        t("recording", () -> {
            try (Recording r = new Recording()) {
                r.enable("probe.Ev");
                r.start();
                Ev e = new Ev(); e.n = 1; e.commit();
                r.stop();
                java.nio.file.Path f = java.nio.file.Files.createTempFile("p", ".jfr");
                r.dump(f);
                int cnt = 0;
                try (RecordingFile rf = new RecordingFile(f)) { while (rf.hasMoreEvents()) { RecordedEvent ev = rf.readEvent(); if (ev.getEventType().getName().equals("probe.Ev")) cnt++; } }
                java.nio.file.Files.delete(f);
                return "state=" + r.getState() + " events=" + cnt;
            }
        });
    }
}
