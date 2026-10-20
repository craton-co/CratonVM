// RJfrEventPipeline -- the whole JFR event pipeline end to end, as the JDK's own
// jdk.jfr bytecode drives it (SBR5 lane jfr; docs/internal/jdk-only/SBR5-JFR-event-pipeline-RETIRED-20261006.md):
// a user event class is extended at definition, registered at initialization,
// instrumented through the retransformation upcall, committed through the
// EventWriter into a native buffer, written into a .jfr chunk with its constant
// pools and read back by RecordingFile / RecordingStream.
//
// Only values that are the same on HotSpot are printed (no thread ids, no
// timestamps, no VM name).
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import jdk.jfr.Enabled;
import jdk.jfr.Event;
import jdk.jfr.EventType;
import jdk.jfr.FlightRecorder;
import jdk.jfr.Label;
import jdk.jfr.Name;
import jdk.jfr.Recording;
import jdk.jfr.Registered;
import jdk.jfr.StackTrace;
import jdk.jfr.Threshold;
import jdk.jfr.ValueDescriptor;
import jdk.jfr.consumer.RecordedClass;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedStackTrace;
import jdk.jfr.consumer.RecordedThread;
import jdk.jfr.consumer.RecordingFile;
import jdk.jfr.consumer.RecordingStream;

public class RJfrEventPipeline {

    static int checks = 0;

    static void ck(String key, Object value) {
        checks++;
        System.out.println("CK RJfrEventPipeline " + key + "=" + value);
    }

    @Name("regression.All")
    @Label("All")
    @StackTrace(true)
    static class All extends Event {
        boolean z = true;
        byte b = 5;
        short s = -300;
        char c = 'x';
        int i = 123456;
        long l = 1L << 40;
        float f = 1.5f;
        double d = 2.25;
        String str = "short";
        String longStr = "this string is definitely longer than sixteen chars";
        String nul = null;
        String empty = "";
        Class<?> cls = java.util.ArrayList.class;
        Thread thr = Thread.currentThread();
    }

    @Name("regression.Timed")
    @StackTrace(false)
    @Threshold("0 ns")
    static class Timed extends Event {
        int n;
    }

    @Name("regression.Off")
    @Enabled(false)
    static class Off extends Event {
        int n;
    }

    @Name("regression.NoReg")
    @Registered(false)
    static class NoReg extends Event {
        int n;
    }

    static String describe(RecordedEvent e) {
        StringBuilder sb = new StringBuilder(e.getEventType().getName());
        for (ValueDescriptor v : e.getEventType().getFields()) {
            String n = v.getName();
            if (n.equals("startTime") || n.equals("duration")) continue;
            Object val = e.getValue(n);
            if (val instanceof RecordedStackTrace st) {
                val = st.getFrames().isEmpty() ? "empty" : "top=" + st.getFrames().get(0).getMethod().getType().getName() + "." + st.getFrames().get(0).getMethod().getName();
            } else if (val instanceof RecordedThread th) {
                val = "thread:" + th.getJavaName();
            } else if (val instanceof RecordedClass rc) {
                val = "class:" + rc.getName();
            }
            sb.append(' ').append(n).append('=').append(val);
        }
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        Path f = Files.createTempFile("rjfrpipeline", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("regression.All");
            r.enable("regression.Timed");
            r.enable("regression.NoReg");
            r.start();
            new All().commit();
            Timed t = new Timed();
            t.begin();
            Thread.sleep(20);
            t.n = 1;
            t.end();
            t.commit();
            Off off = new Off();
            ck("off.isEnabled", off.isEnabled());
            ck("off.shouldCommit", off.shouldCommit());
            off.commit();
            Thread th = new Thread(() -> {
                Timed x = new Timed();
                x.n = 2;
                x.commit();
            }, "worker-1");
            th.start();
            th.join();
            for (int k = 0; k < 2000; k++) {
                Timed x = new Timed();
                x.n = 100 + k;
                x.commit();
            }
            ck("state.running", r.getState());
            r.stop();
            ck("state.stopped", r.getState());
            r.dump(f);
        }
        List<String> lines = new ArrayList<>();
        int timed = 0;
        try (RecordingFile rf = new RecordingFile(f)) {
            while (rf.hasMoreEvents()) {
                RecordedEvent e = rf.readEvent();
                String n = e.getEventType().getName();
                if (!n.startsWith("regression.")) continue;
                if (n.equals("regression.Timed")) {
                    timed++;
                    if (e.getInt("n") >= 100) continue;
                }
                String extra = n.equals("regression.Timed") && e.getInt("n") == 1
                        ? " dur>=20ms=" + (e.getDuration().toMillis() >= 20) : "";
                lines.add(describe(e) + extra + " thread=" + (e.getThread() == null ? null : e.getThread().getJavaName()));
            }
        }
        Collections.sort(lines);
        for (String l : lines) ck("event", l);
        ck("timedEvents", timed);
        EventType et = EventType.getEventType(All.class);
        StringBuilder fields = new StringBuilder();
        for (ValueDescriptor v : et.getFields()) fields.append(v.getName()).append(':').append(v.getTypeName()).append(' ');
        ck("type", et.getName() + " label=" + et.getLabel() + " fields=" + fields);
        ck("noreg.listed", FlightRecorder.getFlightRecorder().getEventTypes().stream().anyMatch(x -> x.getName().equals("regression.NoReg")));
        try {
            EventType.getEventType(NoReg.class);
            ck("noreg", "ok");
        } catch (Throwable x) {
            ck("noreg", x.getClass().getName());
        }
        Files.delete(f);

        CountDownLatch got = new CountDownLatch(3);
        List<String> seen = java.util.Collections.synchronizedList(new ArrayList<>());
        try (RecordingStream rs = new RecordingStream()) {
            rs.enable("regression.Timed").withoutStackTrace();
            rs.onEvent("regression.Timed", e -> {
                seen.add("n=" + e.getInt("n"));
                got.countDown();
            });
            rs.startAsync();
            for (int k = 1; k <= 3; k++) {
                Timed x = new Timed();
                x.n = k;
                x.commit();
            }
            ck("stream.received", got.await(20, TimeUnit.SECONDS));
        }
        Collections.sort(seen);
        ck("stream.seen", seen);

        System.out.println("CK RJfrEventPipeline checks=" + checks);
        System.out.println("PASS RJfrEventPipeline (" + checks + " checks)");
    }
}
