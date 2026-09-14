import java.nio.file.*;
import java.util.*;
import java.util.concurrent.TimeUnit;
import static java.nio.file.StandardWatchEventKinds.*;

/** Minimal WatchService probe: register a directory, touch a file, poll. */
public class WatchProbe {
    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("watchprobe");
        System.out.println("dir = " + dir);

        System.out.println("kind.class  = " + ENTRY_CREATE.getClass().getName());
        System.out.println("kind.name   = " + ENTRY_CREATE.name());
        System.out.println("kind.string = " + ENTRY_CREATE);

        WatchService ws = dir.getFileSystem().newWatchService();
        System.out.println("ws.class = " + ws.getClass().getName());

        WatchKey key = dir.register(ws, ENTRY_CREATE, ENTRY_MODIFY, ENTRY_DELETE);
        System.out.println("registered = " + (key != null));

        // Give the platform watcher a moment to arm, then make a change.
        Thread.sleep(300);
        Files.write(dir.resolve("a.txt"), "x".getBytes());

        // Mirror Spring Boot's FileWatcher.accumulate exactly: look the key up
        // in a map keyed by WatchKey identity, resolve each event's context
        // against key.watchable(), and compare the result with the registered
        // path. Every one of those steps is a place a WatchService stand-in can
        // silently produce nothing.
        Map<WatchKey, String> byKey = new HashMap<>();
        byKey.put(key, "reg");

        long deadline = System.currentTimeMillis() + 8000;
        List<String> seen = new ArrayList<>();
        List<String> resolved = new ArrayList<>();
        while (System.currentTimeMillis() < deadline && seen.isEmpty()) {
            WatchKey k = ws.poll(500, TimeUnit.MILLISECONDS);
            if (k == null) continue;
            System.out.println("keyLookup = " + byKey.get(k));
            Path watched = (Path) k.watchable();
            System.out.println("watchable = " + watched);
            for (WatchEvent<?> e : k.pollEvents()) {
                seen.add(e.kind() + ":" + e.context());
                Path file = watched.resolve((Path) e.context());
                resolved.add(file.toAbsolutePath().toString());
            }
            k.reset();
        }
        System.out.println("events = " + seen);
        System.out.println("resolved = " + resolved);
        System.out.println("resolvedMatches = "
                + resolved.contains(dir.resolve("a.txt").toAbsolutePath().toString()));
        System.out.println(seen.isEmpty() ? "RESULT=NO_EVENTS" : "RESULT=OK");

        ws.close();
    }
}
