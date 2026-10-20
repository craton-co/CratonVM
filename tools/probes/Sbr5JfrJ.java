import jdk.jfr.*;
import jdk.jfr.consumer.*;
import java.io.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.file.*;
import java.util.*;

/** Items: file events from FileChannel / RandomAccessFile / Files.write in both modes. */
public class Sbr5JfrJ {
    public static void main(String[] a) throws Exception {
        Path dir = Files.createTempDirectory("sbr5j");
        Path f1 = dir.resolve("a.bin"), f2 = dir.resolve("b.bin"), f3 = dir.resolve("c.bin");
        Path out = dir.resolve("rec.jfr");
        try (Recording r = new Recording()) {
            for (String n : List.of("jdk.FileRead", "jdk.FileWrite", "jdk.FileForce")) r.enable(n).withThreshold(java.time.Duration.ZERO);
            r.start();
            try (FileChannel c = FileChannel.open(f1, StandardOpenOption.CREATE, StandardOpenOption.WRITE)) { c.write(ByteBuffer.wrap(new byte[100])); c.force(true); }
            try (RandomAccessFile raf = new RandomAccessFile(f2.toFile(), "rw")) { raf.write(new byte[50]); raf.seek(0); raf.read(new byte[10]); }
            Files.write(f3, new byte[30]);
            Files.readAllBytes(f3);
            try (FileChannel c = FileChannel.open(f1, StandardOpenOption.READ)) { c.read(ByteBuffer.allocate(20)); }
            r.stop(); r.dump(out);
        }
        TreeMap<String, List<Long>> m = new TreeMap<>();
        try (RecordingFile rf = new RecordingFile(out)) {
            while (rf.hasMoreEvents()) {
                RecordedEvent e = rf.readEvent();
                String n = e.getEventType().getName();
                if (!n.startsWith("jdk.File")) continue;
                String p = e.getString("path");
                if (p == null || !p.contains("sbr5j")) continue;
                long bytes = n.equals("jdk.FileRead") ? e.getLong("bytesRead") : n.equals("jdk.FileWrite") ? e.getLong("bytesWritten") : 0;
                m.computeIfAbsent(n + " " + p.substring(p.lastIndexOf('/') + 1), k -> new ArrayList<>()).add(bytes);
            }
        }
        for (var e : m.entrySet()) System.out.println(e.getKey() + " count=" + e.getValue().size() + " bytes=" + e.getValue().stream().mapToLong(Long::longValue).sum());
    }
}
