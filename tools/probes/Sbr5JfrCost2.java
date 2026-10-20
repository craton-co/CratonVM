import jdk.jfr.*;
import jdk.jfr.consumer.*;
import java.util.concurrent.*;
public class Sbr5JfrCost2 {
    static class Ev extends Event { int n; }
    static long t0 = System.nanoTime();
    static void lap(String s) { long t = System.nanoTime(); System.out.println(s + " +" + (t - t0) / 1_000_000 + "ms"); t0 = t; }
    public static void main(String[] a) throws Exception {
        FlightRecorder.getFlightRecorder(); lap("FlightRecorder.getFlightRecorder");
        CountDownLatch got = new CountDownLatch(1);
        RecordingStream rs = new RecordingStream(); lap("new RecordingStream");
        rs.onEvent("Sbr5JfrCost2$Ev", e -> got.countDown()); lap("onEvent");
        rs.startAsync(); lap("startAsync");
        Ev ev = new Ev(); ev.n = 1; ev.commit(); lap("first commit");
        got.await(10, TimeUnit.SECONDS); lap("event delivered");
        rs.close(); lap("close");
    }
}
