import jdk.jfr.*;
import java.util.*;
public class Sbr5JfrProf {
    public static void main(String[] a) throws Exception {
        Thread main = Thread.currentThread();
        Map<String,Integer> self = new HashMap<>(), incl = new HashMap<>();
        int[] n = {0};
        Thread s = new Thread(() -> {
            while (!Thread.interrupted()) {
                StackTraceElement[] st = main.getStackTrace();
                if (st.length > 0) {
                    n[0]++;
                    String top = st[0].getClassName() + "." + st[0].getMethodName();
                    self.merge(top, 1, Integer::sum);
                    Set<String> seen = new HashSet<>();
                    for (StackTraceElement e : st) { String k = e.getClassName() + "." + e.getMethodName(); if (seen.add(k)) incl.merge(k, 1, Integer::sum); }
                }
                try { Thread.sleep(4); } catch (InterruptedException e) { return; }
            }
        }); s.setDaemon(true); s.start();
        long t0 = System.nanoTime();
        FlightRecorder.getFlightRecorder();
        long ms = (System.nanoTime() - t0) / 1_000_000;
        s.interrupt(); s.join();
        System.out.println("init ms=" + ms + " samples=" + n[0]);
        System.out.println("--- inclusive (top 45)");
        incl.entrySet().stream().sorted((x, y) -> y.getValue() - x.getValue()).limit(45).forEach(e -> System.out.println(e.getValue() + " " + e.getKey()));
        System.out.println("--- self (top 15)");
        self.entrySet().stream().sorted((x, y) -> y.getValue() - x.getValue()).limit(15).forEach(e -> System.out.println(e.getValue() + " " + e.getKey()));
    }
}
