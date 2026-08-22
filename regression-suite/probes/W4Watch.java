import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;

/**
 * Drive `WatchService` end to end, so the question "does anything reach
 * `native-io/src/watch.rs`'s registrations" has an answer instead of a
 * zero from a run that never asked.
 *
 * `WORKER-4-2` N5 measured those 27 (now 54) rows at `invocations: 0` and
 * refused to delete them, because clearing them needs a `--features
 * synthetic-jdk` build and that lane did not make one. This probe is the other
 * half: run it under BOTH builds with `--dump-native-registry` and look at the
 * counts.
 *
 * Deliberately modest about timing — a `WatchService` is at the mercy of the
 * filesystem, so the assertions are "a key came back and it names the file",
 * not "within N milliseconds".
 */
public class W4Watch {

    static void ck(String tag, Object got) { System.out.println("CK " + tag + " " + got); }

    interface Thunk { Object call() throws Exception; }

    static void ckT(String tag, Thunk t) {
        try {
            ck(tag, t.call());
        } catch (Throwable e) {
            ck(tag, "threw:" + e.getClass().getName());
        }
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("w4watch");

        ckT("newWatchService", () -> {
            try (WatchService ws = FileSystems.getDefault().newWatchService()) {
                return ws != null ? "opened" : "null";
            }
        });

        try (WatchService ws = FileSystems.getDefault().newWatchService()) {
            WatchKey key = dir.register(ws,
                    StandardWatchEventKinds.ENTRY_CREATE,
                    StandardWatchEventKinds.ENTRY_MODIFY,
                    StandardWatchEventKinds.ENTRY_DELETE);
            ck("register.keyNonNull", key != null);
            ck("register.isValid", key.isValid());
            ck("register.watchable", key.watchable().equals(dir));

            // Nothing has happened yet.
            ck("poll.empty", ws.poll() == null);

            Files.write(dir.resolve("created.txt"), "x".getBytes(StandardCharsets.UTF_8));

            // Give the OS a moment; a WatchService is not synchronous.
            WatchKey got = null;
            for (int i = 0; i < 40 && got == null; i++) {
                got = ws.poll(250, java.util.concurrent.TimeUnit.MILLISECONDS);
            }
            ck("poll.gotKey", got != null);
            if (got != null) {
                List<String> names = new ArrayList<>();
                for (WatchEvent<?> ev : got.pollEvents()) {
                    names.add(ev.kind().name() + ":" + ev.context());
                }
                Collections.sort(names);
                ck("poll.events", names);
                ck("poll.reset", got.reset());
            }

            ck("key.cancelThenInvalid", (Object) (new Object() {
                boolean go() {
                    key.cancel();
                    return key.isValid();
                }
            }).go());
        }

        // A closed service must refuse, and the type is fixed.
        WatchService closed = FileSystems.getDefault().newWatchService();
        closed.close();
        ckT("closed.poll", () -> closed.poll());
        ckT("closed.take", () -> closed.take());
        ckT("closed.register", () -> dir.register(closed, StandardWatchEventKinds.ENTRY_CREATE));

        System.out.println("PASS W4Watch");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
