// The risk `sun.jnu.encoding` was deferred over, probed directly: that key
// decides how FILE NAMES cross the JNI boundary, so moving it off a pinned
// UTF-8 could change class loading rather than printing.
//
// Create, list, stat and read a file whose name is not ASCII, through both
// java.io.File and java.nio.file, and print what came back. Run on CratonVM
// and HotSpot in the same shell and diff: whatever the platform does, the two
// VMs must do the same thing.
import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;

public class JnuPaths {
    static void p(String k, Object v) { System.out.println("CK " + k + "=" + v); }

    public static void main(String[] a) throws Exception {
        p("sun.jnu.encoding", System.getProperty("sun.jnu.encoding"));
        // Cyrillic + an accented Latin letter: representable in cp1251,
        // representable in UTF-8, NOT representable in US-ASCII.
        String name = "жфайл-é.txt";
        Path dir = Files.createTempDirectory("jnu");
        Path f = dir.resolve(name);

        String wrote;
        try {
            Files.write(f, "payload".getBytes(StandardCharsets.UTF_8));
            wrote = "ok";
        } catch (Throwable t) {
            wrote = t.getClass().getName();
        }
        p("nio.write", wrote);

        // Does the name survive a directory listing?
        List<String> listed = new ArrayList<>();
        try (DirectoryStream<Path> ds = Files.newDirectoryStream(dir)) {
            for (Path e : ds) listed.add(e.getFileName().toString());
        } catch (Throwable t) {
            listed.add("ERR:" + t.getClass().getName());
        }
        p("nio.list", listed);
        p("nio.listRoundTrips", listed.contains(name));

        // The java.io.File half, which is the older JNI path.
        File io = new File(dir.toFile(), name);
        p("io.exists", io.exists());
        p("io.length", io.exists() ? io.length() : -1);
        String[] kids = dir.toFile().list();
        p("io.list", kids == null ? "null" : Arrays.asList(kids));

        // And read it back by the name we wrote.
        String back;
        try {
            back = new String(Files.readAllBytes(f), StandardCharsets.UTF_8);
        } catch (Throwable t) {
            back = t.getClass().getName();
        }
        p("nio.readBack", back);

        // Clean up quietly.
        try { Files.deleteIfExists(f); Files.deleteIfExists(dir); } catch (Throwable ignored) { }
        System.out.println("PASS JnuPaths");
    }
}
