import jdk.jfr.*;
import jdk.jfr.consumer.*;
import java.io.*;
import java.net.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.locks.LockSupport;

/** SBR5 jfr: JDK events committed from Java (jdk.internal.event.*), diffed vs HotSpot. */
public class Sbr5JfrD {
    static void p(String s) { System.out.println(s); }
    public static void main(String[] a) throws Exception {
        Path dir = Files.createTempDirectory("sbr5d");
        Path data = dir.resolve("data.txt"); Files.writeString(data, "hello jfr");
        Path out = dir.resolve("rec.jfr");
        List<String> names = List.of("jdk.FileRead", "jdk.FileWrite", "jdk.SocketRead", "jdk.SocketWrite", "jdk.ThreadSleep", "jdk.ThreadPark",
            "jdk.JavaExceptionThrow", "jdk.JavaErrorThrow", "jdk.Deserialization", "jdk.ExceptionStatistics", "jdk.ProcessStart", "jdk.X509Certificate");
        try (Recording r = new Recording()) {
            for (String n : names) r.enable(n).withThreshold(java.time.Duration.ZERO).withStackTrace();
            r.start();
            try (FileInputStream in = new FileInputStream(data.toFile())) { in.read(new byte[4]); }
            try (FileOutputStream o = new FileOutputStream(dir.resolve("o.txt").toFile())) { o.write(new byte[]{1,2,3}); }
            try (ServerSocket ss = new ServerSocket(0)) {
                Thread t = new Thread(() -> { try (Socket s = ss.accept()) { s.getInputStream().read(new byte[2]); s.getOutputStream().write(7); } catch (IOException e) {} });
                t.start();
                try (Socket c = new Socket("127.0.0.1", ss.getLocalPort())) { c.getOutputStream().write(new byte[]{1,2}); c.getInputStream().read(); }
                t.join();
            }
            Thread.sleep(15);
            LockSupport.parkNanos(2_000_000);
            try { throw new IllegalStateException("probe"); } catch (IllegalStateException e) {}
            try (ObjectInputStream ois = new ObjectInputStream(new ByteArrayInputStream(ser("x")))) { ois.readObject(); }
            r.stop(); r.dump(out);
        }
        Map<String,Integer> counts = new TreeMap<>(); List<String> detail = new ArrayList<>();
        try (RecordingFile rf = new RecordingFile(out)) {
            while (rf.hasMoreEvents()) {
                RecordedEvent e = rf.readEvent();
                String n = e.getEventType().getName();
                if (!names.contains(n)) continue;
                counts.merge(n, 1, Integer::sum);
                if (n.equals("jdk.FileRead") && e.getString("path").endsWith("data.txt")) detail.add("FileRead bytes=" + e.getLong("bytesRead") + " stack=" + (e.getStackTrace() != null));
                if (n.equals("jdk.FileWrite") && e.getString("path").endsWith("o.txt")) detail.add("FileWrite bytes=" + e.getLong("bytesWritten"));
                if (n.equals("jdk.ThreadSleep")) detail.add("ThreadSleep time=" + e.getDuration("time").toMillis());
                if (n.equals("jdk.JavaExceptionThrow") && e.getString("message") != null && e.getString("message").equals("probe")) detail.add("ExThrow " + e.getClass("thrownClass").getName());
                if (n.equals("jdk.Deserialization")) detail.add("Deserialization type=" + e.getClass("type"));
            }
        }
        counts.forEach((k, v) -> p(k + " " + (v > 0 ? ">0" : "0")));
        new TreeSet<>(detail).forEach(d -> p(d));
    }
    static byte[] ser(Object o) throws IOException { ByteArrayOutputStream b = new ByteArrayOutputStream(); try (ObjectOutputStream s = new ObjectOutputStream(b)) { s.writeObject(o); } return b.toByteArray(); }
}
