import jdk.jfr.*;
import jdk.jfr.consumer.*;
import java.nio.file.*;
import java.util.*;
public class Sbr5JfrE {
    public static void main(String[] a) throws Exception {
        Path f = Files.createTempFile("sbr5e", ".jfr");
        try (Recording r = new Recording(Configuration.getConfiguration(a.length > 0 ? a[0] : "profile"))) {
            r.start();
            List<Object> keep = new ArrayList<>();
            Thread t = new Thread(() -> { try { Thread.sleep(50); } catch (Exception e) {} }); t.start();
            for (int i = 0; i < 200000; i++) { keep.add(new byte[256]); if (keep.size() > 5000) keep.clear(); }
            synchronized (keep) { keep.wait(30); }
            System.gc();
            Thread.sleep(1500);
            t.join();
            r.stop(); r.dump(f);
        }
        TreeMap<String,Integer> c = new TreeMap<>();
        try (RecordingFile rf = new RecordingFile(f)) { while (rf.hasMoreEvents()) c.merge(rf.readEvent().getEventType().getName(), 1, Integer::sum); }
        for (var e : c.entrySet()) System.out.println(e.getKey());
        Files.delete(f);
    }
}
