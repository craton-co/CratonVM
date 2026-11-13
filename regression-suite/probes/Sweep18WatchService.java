import java.io.IOException;
import java.nio.file.*;
import java.util.concurrent.TimeUnit;

/**
 * Sweep 18: `java.nio.file.WatchService` CONTRACTS.
 *
 * G88-1 §9 censused `native-io/src/watch.rs` — 27 `Bridge` registrations, ZERO
 * of them `ACC_NATIVE`, so all are mistagged by the same standard every retag
 * this session used. It was NOT retagged, because invocations = 0: nothing in
 * 101 vectors exercises a `WatchService`, so the tag would move, the arms would
 * stay green, and neither fact would say whether file watching still works.
 * That fails verification (3) of the three-part rule in G85-1 §3b.
 *
 * This is the missing exercise.
 *
 * DELIBERATELY NOT EVENT-TIMING. A probe that creates a file and waits for the
 * event is at the mercy of filesystem notification latency, and a flaky vector
 * is worse than no vector. Every row below is a CONTRACT the specification
 * fixes regardless of timing:
 *
 *   * a default FileSystem yields a non-null WatchService;
 *   * registering a directory yields a valid, watchable key;
 *   * `poll()` with nothing pending returns null rather than blocking;
 *   * `poll(timeout)` with nothing pending returns null within the timeout;
 *   * a key reports the directory it watched as its watchable;
 *   * cancelling a key makes it invalid;
 *   * closing the service makes `take()`/`poll()` throw
 *     `ClosedWatchServiceException`, and closing twice is a no-op;
 *   * registering a FILE (not a directory) throws `NotDirectoryException`;
 *   * registering with no event kinds yields a key that is still valid.
 *
 * Those exercise construction, registration, key lifecycle, the closed-state
 * transitions and the error contracts — the whole surface `watch.rs` shims —
 * without once depending on when the OS decides to deliver a notification.
 */
public class Sweep18WatchService {
    interface C { Object g() throws Exception; }
    static void t(String l, C c) {
        try { System.out.println("W " + l + " = " + c.g()); }
        catch (Throwable x) {
            System.out.println("W " + l + " = " + x.getClass().getName());
        }
    }

    public static void main(String[] a) throws Exception {
        Path dir = Files.createTempDirectory("cratonvm-watch");
        Path file = Files.createFile(dir.resolve("f.txt"));
        dir.toFile().deleteOnExit();
        file.toFile().deleteOnExit();

        t("newWatchService_nonnull", () ->
                FileSystems.getDefault().newWatchService() != null);

        t("register_key_valid", () -> {
            try (WatchService ws = FileSystems.getDefault().newWatchService()) {
                WatchKey k = dir.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
                return k != null && k.isValid();
            }
        });

        t("key_watchable_is_dir", () -> {
            try (WatchService ws = FileSystems.getDefault().newWatchService()) {
                WatchKey k = dir.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
                return k.watchable().equals(dir);
            }
        });

        // poll() must NOT block when nothing is pending.
        t("poll_empty_is_null", () -> {
            try (WatchService ws = FileSystems.getDefault().newWatchService()) {
                dir.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
                return ws.poll() == null;
            }
        });

        t("poll_timeout_is_null", () -> {
            try (WatchService ws = FileSystems.getDefault().newWatchService()) {
                dir.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
                return ws.poll(50, TimeUnit.MILLISECONDS) == null;
            }
        });

        t("cancel_makes_invalid", () -> {
            try (WatchService ws = FileSystems.getDefault().newWatchService()) {
                WatchKey k = dir.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
                k.cancel();
                return k.isValid();
            }
        });

        t("closed_take_throws", () -> {
            WatchService ws = FileSystems.getDefault().newWatchService();
            dir.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
            ws.close();
            ws.take();
            return "no-throw";
        });

        t("closed_poll_throws", () -> {
            WatchService ws = FileSystems.getDefault().newWatchService();
            ws.close();
            ws.poll();
            return "no-throw";
        });

        t("double_close_ok", () -> {
            WatchService ws = FileSystems.getDefault().newWatchService();
            ws.close();
            ws.close();
            return "ok";
        });

        // A FILE is not registrable — the JDK throws NotDirectoryException.
        t("register_file_throws", () -> {
            try (WatchService ws = FileSystems.getDefault().newWatchService()) {
                file.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
                return "no-throw";
            }
        });

        t("register_no_kinds_valid", () -> {
            try (WatchService ws = FileSystems.getDefault().newWatchService()) {
                WatchKey k = dir.register(ws);
                return k.isValid();
            }
        });

        t("key_pollEvents_empty", () -> {
            try (WatchService ws = FileSystems.getDefault().newWatchService()) {
                WatchKey k = dir.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
                return k.pollEvents().size();
            }
        });

        System.out.println("W done = 1");
    }
}
