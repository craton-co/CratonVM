import jdk.jfr.*;
import jdk.jfr.consumer.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;

/** SBR5 jfr milestone A: custom events through Recording / dump / RecordingStream, diffed vs HotSpot. */
public class Sbr5JfrA {
    @Name("probe.All") @Label("All") @StackTrace(true)
    static class All extends Event {
        boolean z = true; byte b = 5; short s = -300; char c = 'x'; int i = 123456; long l = 1L << 40;
        float f = 1.5f; double d = 2.25;
        String str = "short"; String longStr = "this string is definitely longer than sixteen chars";
        String nul = null; String empty = "";
        Class<?> cls = java.util.ArrayList.class; Thread thr = Thread.currentThread();
    }
    @Name("probe.Timed") @StackTrace(false) @Threshold("0 ns")
    static class Timed extends Event { int n; }
    @Name("probe.Off") @Enabled(false)
    static class Off extends Event { int n; }
    @Name("probe.NoReg") @Registered(false)
    static class NoReg extends Event { int n; }
    static void p(String s) { System.out.println(s); }
    static String describe(RecordedEvent e) {
        StringBuilder sb = new StringBuilder(e.getEventType().getName());
        for (ValueDescriptor v : e.getEventType().getFields()) {
            String n = v.getName();
            if (n.equals("startTime") || n.equals("duration")) continue;
            Object val;
            try { val = e.getValue(n); } catch (Throwable t) { val = "ERR " + t; }
            if (val instanceof RecordedStackTrace st) {
                val = st.getFrames().isEmpty() ? "empty" : "top=" + st.getFrames().get(0).getMethod().getType().getName() + "." + st.getFrames().get(0).getMethod().getName() + " depth>0";
            } else if (val instanceof RecordedThread th) {
                val = "thread:" + th.getJavaName();
            } else if (val instanceof RecordedClass rc) {
                val = "class:" + rc.getName();
            }
            sb.append(' ').append(n).append('=').append(val);
        }
        return sb.toString();
    }
    public static void main(String[] a) throws Exception {
        Path f = Files.createTempFile("sbr5a", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("probe.All"); r.enable("probe.Timed"); r.enable("probe.NoReg");
            r.start();
            new All().commit();
            Timed t = new Timed(); t.begin(); Thread.sleep(20); t.n = 1; t.end(); t.commit();
            Off off = new Off(); p("off.isEnabled=" + off.isEnabled() + " shouldCommit=" + off.shouldCommit()); off.commit();
            Thread th = new Thread(() -> { Timed x = new Timed(); x.n = 2; x.commit(); }, "worker-1"); th.start(); th.join();
            for (int k = 0; k < 2000; k++) { Timed x = new Timed(); x.n = 100 + k; x.commit(); }
            p("state=" + r.getState());
            r.stop();
            p("state=" + r.getState());
            r.dump(f);
        }
        List<String> lines = new ArrayList<>();
        int timed = 0; long minDur = Long.MAX_VALUE;
        try (RecordingFile rf = new RecordingFile(f)) {
            while (rf.hasMoreEvents()) {
                RecordedEvent e = rf.readEvent();
                String n = e.getEventType().getName();
                if (!n.startsWith("probe.")) continue;
                if (n.equals("probe.Timed")) { timed++; if (e.getInt("n") == 1) minDur = e.getDuration().toMillis(); if (e.getInt("n") >= 100) continue; }
                lines.add(describe(e) + (n.equals("probe.Timed") && e.getInt("n") == 1 ? " dur>=20ms=" + (minDur >= 20) : "")
                          + " thread=" + (e.getThread() == null ? null : e.getThread().getJavaName()));
            }
        }
        Collections.sort(lines);
        for (String l : lines) p(l);
        p("timedEvents=" + timed);
        EventType et = EventType.getEventType(All.class);
        p("type " + et.getName() + " label=" + et.getLabel() + " fields=" + et.getFields().stream().map(v -> v.getName() + ":" + v.getTypeName()).toList());
        p("types NoReg registered=" + FlightRecorder.getFlightRecorder().getEventTypes().stream().anyMatch(x -> x.getName().equals("probe.NoReg")));
        try { EventType.getEventType(NoReg.class); p("noreg ok"); } catch (Throwable x) { p("noreg " + x); }
        p("jfrFileSize>0=" + (Files.size(f) > 0));
        if (a.length > 0) Files.copy(f, Path.of(a[0]), StandardCopyOption.REPLACE_EXISTING);
        Files.delete(f);

        CountDownLatch got = new CountDownLatch(3);
        List<String> seen = Collections.synchronizedList(new ArrayList<>());
        try (RecordingStream rs = new RecordingStream()) {
            rs.enable("probe.Timed").withoutStackTrace();
            rs.onEvent("probe.Timed", e -> { seen.add("n=" + e.getInt("n")); got.countDown(); });
            rs.startAsync();
            for (int k = 1; k <= 3; k++) { Timed x = new Timed(); x.n = k; x.commit(); }
            p("stream received=" + got.await(15, TimeUnit.SECONDS));
        }
        Collections.sort(seen);
        p("stream seen=" + seen);
    }
}
