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

    /**
     * Run `trigger`, then collect every event the service reports over a fixed
     * window, and answer the DEDUPLICATED, SORTED set of `KIND:context` pairs.
     *
     * The window is what makes this deterministic: a `WatchService` may split
     * one filesystem operation across several keys or coalesce several into
     * one, and either is legal. Draining until the service goes quiet removes
     * that freedom from the answer. `reset()` is called on every key, because
     * a key that is not reset stops reporting.
     */
    static List<String> drainKinds(WatchService ws, Runnable trigger) throws Exception {
        TreeSet<String> seen = new TreeSet<>();
        trigger.run();
        long deadline = System.nanoTime() + 5_000_000_000L;
        int quiet = 0;
        while (System.nanoTime() < deadline && quiet < 4) {
            WatchKey k = ws.poll(250, java.util.concurrent.TimeUnit.MILLISECONDS);
            if (k == null) {
                if (!seen.isEmpty()) quiet++;
                continue;
            }
            quiet = 0;
            for (WatchEvent<?> ev : k.pollEvents()) {
                seen.add(ev.kind().name() + ":" + ev.context());
            }
            k.reset();
        }
        return new ArrayList<>(seen);
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

            // ONE event kind per observation, and the events DRAINED over a
            // window rather than read off the first key that comes back.
            //
            // The obvious shape -- `Files.write` a new file, take the first
            // key, print its events -- is RACY, and this probe shipped that way
            // for one round: `Files.write` is a create AND a write, so the OS
            // may deliver `ENTRY_CREATE` and `ENTRY_MODIFY` on one key or on
            // two, and taking the first key sees either `[CREATE, MODIFY]` or
            // `[CREATE]` depending on how the poll interleaved with inotify.
            // MEASURED: the same binary produced both on consecutive runs. A
            // flaky probe in the tree is worse than no probe, so:
            //   * `createFile` for the CREATE observation, which emits one kind;
            //   * an explicit write to an EXISTING file for MODIFY;
            //   * each drained until the window closes, then reported as a
            //     deduplicated SET of kinds.
            ck("poll.create", drainKinds(ws, () -> {
                try {
                    Files.createFile(dir.resolve("created.txt"));
                } catch (IOException e) {
                    throw new UncheckedIOException(e);
                }
            }));
            ck("poll.modify", drainKinds(ws, () -> {
                try {
                    Files.writeString(dir.resolve("created.txt"), "more");
                } catch (IOException e) {
                    throw new UncheckedIOException(e);
                }
            }));
            ck("poll.delete", drainKinds(ws, () -> {
                try {
                    Files.delete(dir.resolve("created.txt"));
                } catch (IOException e) {
                    throw new UncheckedIOException(e);
                }
            }));

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
