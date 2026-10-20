// RJfrRecordingStream — `new RecordingStream()` boots the real JDK
// `PlatformRecorder`, whose `Repository.ensureRepository()` resolves its base
// path from `java.io.tmpdir` in `Repository.<clinit>`. A null or unusable path
// there fails every stream in the process with
// `IllegalStateException: Can't create Flight Recorder`, which is how Netty's
// `JfrEventsTest` (10 tests) and `JfrEventSafeTest` (2) were reported failing.
//
// `java.io.tmpdir` is printed verbatim: the suite runs both VMs in the same
// environment, and HotSpot on Linux answers `/tmp` whatever `$TMPDIR` says —
// CratonVM used to answer `$TMPDIR`, which broke the repository whenever that
// directory did not exist.
//
//   docs/internal/fixed-suite-bugs/netty/jfr-events-repository-basepath-npe-FIXED-20260924.md
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import jdk.jfr.Enabled;
import jdk.jfr.Event;
import jdk.jfr.FlightRecorder;
import jdk.jfr.Name;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingStream;

public class RJfrRecordingStream {

    static int checks = 0;

    static void ck(String key, Object value) {
        checks++;
        System.out.println("CK RJfrRecordingStream " + key + "=" + value);
    }

    @Name("regression.Alloc")
    static final class AllocEvent extends Event {
        int size;
        boolean direct;
        String who;
    }

    @Name("regression.Disabled")
    @Enabled(false)
    static final class DisabledEvent extends Event {
        String who;
    }

    static String stream(int round) {
        try (RecordingStream stream = new RecordingStream()) {
            CompletableFuture<RecordedEvent> got = new CompletableFuture<>();
            CompletableFuture<String> disabled = new CompletableFuture<>();
            stream.enable(AllocEvent.class);
            stream.onEvent("regression.Alloc", got::complete);
            stream.onEvent("regression.Disabled", e -> disabled.complete(e.getString("who")));
            stream.startAsync();
            DisabledEvent d = new DisabledEvent();
            d.who = "never";
            d.commit();
            AllocEvent e = new AllocEvent();
            e.size = 128 + round;
            e.direct = true;
            e.who = "round" + round;
            e.commit();
            RecordedEvent r = got.get(20, TimeUnit.SECONDS);
            return r.getEventType().getName() + " size=" + r.getInt("size")
                    + " direct=" + r.getBoolean("direct") + " who=" + r.getString("who")
                    + " disabledDelivered=" + disabled.isDone();
        } catch (Throwable t) {
            Throwable root = t;
            while (root.getCause() != null && root.getCause() != root) root = root.getCause();
            return "threw " + t.getClass().getName() + " <- " + root.getClass().getName();
        }
    }

    public static void main(String[] args) {
        ck("java.io.tmpdir", System.getProperty("java.io.tmpdir"));
        ck("available", FlightRecorder.isAvailable());
        // Twice: the recorder is created once per process and must serve a
        // second stream after the first one closed.
        ck("stream.first", stream(0));
        ck("stream.second", stream(1));
        ck("initialized", FlightRecorder.isInitialized());

        System.out.println("CK RJfrRecordingStream checks=" + checks);
        System.out.println("PASS RJfrRecordingStream (" + checks + " checks)");
    }
}
