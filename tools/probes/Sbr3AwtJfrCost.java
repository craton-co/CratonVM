import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import jdk.jfr.Event;
import jdk.jfr.FlightRecorder;
import jdk.jfr.consumer.RecordingStream;

/**
 * SBR3 lane awt: cost and answers of the first RecordingStream in a process
 * (the row JDKEvents.initialize was held for). Prints the elapsed ms of the
 * first stream and the JDK event types FlightRecorder lists.
 */
public class Sbr3AwtJfrCost {
    static class Ev extends Event { int n; }
    public static void main(String[] a) throws Exception {
        long t0 = System.nanoTime();
        CountDownLatch got = new CountDownLatch(1);
        try (RecordingStream rs = new RecordingStream()) {
            rs.onEvent("Sbr3AwtJfrCost$Ev", e -> got.countDown());
            rs.startAsync();
            Ev ev = new Ev(); ev.n = 1; ev.commit();
            boolean ok = got.await(2, TimeUnit.SECONDS);
            System.out.println("received=" + ok);
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("TIME first-stream ms=" + ms);
        long jdkTypes = FlightRecorder.getFlightRecorder().getEventTypes().stream().filter(t -> t.getName().startsWith("jdk.")).count();
        System.out.println("jdk-types>0=" + (jdkTypes > 0));
    }
}
