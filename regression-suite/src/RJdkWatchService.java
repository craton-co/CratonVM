import java.nio.file.ClosedWatchServiceException;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.NotDirectoryException;
import java.nio.file.Path;
import java.nio.file.StandardWatchEventKinds;
import java.nio.file.WatchKey;
import java.nio.file.WatchService;
import java.util.concurrent.TimeUnit;

/**
 * JDK-only corpus: {@code java.nio.file.WatchService} CONTRACTS.
 *
 * This vector exists to make a retag verifiable. G88-1 §9 censused
 * {@code native-io/src/watch.rs} and found 27 {@code Bridge} registrations of
 * which ZERO target an {@code ACC_NATIVE} method — mistagged by the same
 * standard every other retag used. It was deliberately NOT retagged, because
 * invocations were 0: the tag would move, the arms would stay green, and
 * neither fact would say whether file watching still worked. That is
 * verification (3) of the three-part rule in G85-1 §3b, and this supplies it.
 *
 * DELIBERATELY NOT EVENT-TIMING. A vector that creates a file and waits for the
 * notification is at the mercy of filesystem latency; a flaky vector is worse
 * than no vector. Every assertion below is a contract the specification fixes
 * regardless of when the OS delivers anything:
 *
 *   construction, registration, the key's watchable and validity, poll()
 *   returning null rather than blocking, cancellation, the closed-state
 *   transitions, and the two argument-validation refusals.
 *
 * Those last two were DEFECTS when this was written, found by the probe this
 * vector grew out of: registering a regular file returned a live key instead of
 * throwing {@code NotDirectoryException}, and registering with an empty kind
 * set returned a valid key instead of throwing {@code IllegalArgumentException}.
 * Both were fabricated successes where the JDK refuses.
 */
public class RJdkWatchService {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RJdkWatchService: " + m);
        }
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("cratonvm-watch-vec");
        Path file = Files.createFile(dir.resolve("f.txt"));
        file.toFile().deleteOnExit();
        dir.toFile().deleteOnExit();

        // ---- construction and registration ---------------------------------
        try (WatchService ws = FileSystems.getDefault().newWatchService()) {
            check(ws != null, "newWatchService must not return null");
            WatchKey k = dir.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
            check(k != null, "register must return a key");
            check(k.isValid(), "a fresh key is valid");
            check(k.watchable().equals(dir), "the key reports the directory it watched");
            check(k.pollEvents().isEmpty(), "a fresh key has no pending events");
        }

        // ---- poll must not block when nothing is pending --------------------
        try (WatchService ws = FileSystems.getDefault().newWatchService()) {
            dir.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
            check(ws.poll() == null, "poll() with nothing pending returns null");
            check(ws.poll(50, TimeUnit.MILLISECONDS) == null,
                    "poll(timeout) with nothing pending returns null");
        }

        // ---- cancellation ---------------------------------------------------
        try (WatchService ws = FileSystems.getDefault().newWatchService()) {
            WatchKey k = dir.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
            k.cancel();
            check(!k.isValid(), "a cancelled key is invalid");
        }

        // ---- the closed-state transitions -----------------------------------
        WatchService closed = FileSystems.getDefault().newWatchService();
        dir.register(closed, StandardWatchEventKinds.ENTRY_CREATE);
        closed.close();
        check(threw(() -> closed.take(), ClosedWatchServiceException.class),
                "take() on a closed service throws ClosedWatchServiceException");
        check(threw(() -> closed.poll(), ClosedWatchServiceException.class),
                "poll() on a closed service throws ClosedWatchServiceException");
        closed.close();
        check(true, "closing twice is a no-op");

        // ---- the two argument refusals (both were defects) -------------------
        // A WatchService watches DIRECTORIES. Registering a regular file used to
        // return a live key for a watch that could never fire.
        try (WatchService ws = FileSystems.getDefault().newWatchService()) {
            check(threw(() -> file.register(ws, StandardWatchEventKinds.ENTRY_CREATE),
                            NotDirectoryException.class),
                    "registering a FILE throws NotDirectoryException");
        }
        // An empty kind set is a watch that can never fire; the JDK rejects it
        // before it reaches the OS.
        try (WatchService ws = FileSystems.getDefault().newWatchService()) {
            check(threw(() -> dir.register(ws), IllegalArgumentException.class),
                    "registering with NO event kinds throws IllegalArgumentException");
        }

        System.out.println("CK RJdkWatchService checks=" + checks);
        System.out.println("PASS RJdkWatchService (" + checks + " checks)");
    }

    interface Act { Object run() throws Exception; }

    static boolean threw(Act a, Class<? extends Throwable> want) {
        try {
            a.run();
            return false;
        } catch (Throwable t) {
            return want.isInstance(t);
        }
    }
}
