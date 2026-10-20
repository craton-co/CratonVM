import jdk.jfr.*;
import jdk.jfr.consumer.*;
import java.nio.file.*;
import java.util.*;
public class Sbr5JfrX {
    public static void main(String[] a) throws Exception {
        Path f = Files.createTempFile("sbr5x", ".jfr");
        try (Recording r = new Recording(Configuration.getConfiguration("profile"))) {
            r.enable("jdk.JavaExceptionThrow"); r.enable("jdk.JavaErrorThrow");
            r.start();
            for (int i = 0; i < 5; i++) { try { Integer.parseInt("x"); } catch (NumberFormatException e) {} }
            try { throw new StackOverflowError("so"); } catch (Error e) {}
            Thread t = new Thread(() -> { long x = 0; for (int i = 0; i < 3_000_000; i++) x += i; }); t.start(); t.join();
            r.stop(); r.dump(f);
        }
        TreeMap<String,Integer> c = new TreeMap<>();
        try (RecordingFile rf = new RecordingFile(f)) { while (rf.hasMoreEvents()) { RecordedEvent e = rf.readEvent(); String n = e.getEventType().getName(); if (n.equals("jdk.ThreadCPULoad") || n.contains("Throw")) c.merge(n, 1, Integer::sum); } }
        System.out.println(c);
        Files.delete(f);
    }
}
