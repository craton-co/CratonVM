import java.io.IOException;
import java.io.InputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketException;
import java.nio.ByteBuffer;
import java.nio.channels.ClosedByInterruptException;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * Regression: BUG-NIO-NULL-INTERRUPTOR-20260726 (part 1) and the
 * blocked-reader-never-wakes defect in the `java.net.Socket` stream path
 * (part 2).
 *
 * <h2>Part 1 — FileChannel and the null interruptor</h2>
 *
 * `AbstractInterruptibleChannel.interruptor` is a FINAL field the JDK
 * constructor always assigns; `begin()` dereferences it unconditionally once
 * `Thread.currentThread().isInterrupted()` is true. CratonVM builds
 * `FileChannelImpl` through a native bridge that never runs that constructor and
 * used to leave the slot null, so ANY channel operation on a thread whose
 * interrupt flag happened to be set died with
 * `NullPointerException: Cannot invoke "sun.nio.ch.Interruptible.interrupt(...)"
 * because "this.interruptor" is null` instead of performing the specified
 * asynchronous close. (Found via H2 `TestStreamStore`, where the interrupt
 * itself was a second bug — see `RExecutorShutdown`.)
 *
 * The specified behaviour, asserted here against both VMs: the operation fails
 * with `ClosedByInterruptException`, the channel ends up closed, and the
 * thread's interrupt status is preserved.
 *
 * <h2>Part 2 — a blocked Socket read must WAKE on asynchronous close</h2>
 *
 * Added 2026-08-07 by the vacuous-test sweep. Part 1 alone never parks a
 * reader, so it could not see the wave-2 defect in which a thread blocked in
 * `Socket.getInputStream().read()` was never woken by another thread's
 * `Socket.close()` — the reader simply stayed parked forever. A suite that only
 * checks "the field is non-null" or "an already-interrupted call throws"
 * reports such a hang as OK, because it never creates one.
 *
 * `Socket.getInputStream().read()` and `SocketChannel.read()` are SEPARATE
 * implementations behind SEPARATE close registries in CratonVM
 * (`native-io/src/net.rs` vs `native-io/src/socket_channel.rs`), so covering one
 * says nothing about the other. This class owns the `java.net` stream half; the
 * NIO half lives in `RSocketChannelInterrupt`.
 *
 * Measured on HotSpot 25.0.3 (Microsoft build 25.0.3+9-LTS), 3/3 runs:
 * the parked reader wakes ~404 ms after entering `read()` with
 * `java.net.SocketException: Socket closed`.
 *
 * EVERY wait in part 2 is bounded. A defect must produce a named
 * `AssertionError`, never an `rc=124` from the suite's 120 s per-class timeout,
 * because a timeout is indistinguishable from a VM hang.
 */
public class RChannelInterrupt {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    /** How long the closer waits after the reader signals it is entering read(). */
    static final long PARK_MS = 400;
    /**
     * Lower bound on how long the reader must have been inside `read()` for the
     * scenario to have proven anything. The closer sleeps {@link #PARK_MS}
     * first, so on a SLOW machine this only grows — it is a floor we create
     * ourselves, not a wall-clock guess about the host.
     */
    static final long PARKED_PROOF_MS = 250;
    /** Bounded join. A reader still alive after this is the defect, not slowness. */
    static final long WAKE_BUDGET_MS = 15000;

    /** Outcome of a bounded, parked blocking operation on a helper thread. */
    static final class Parked {
        Thread thread;
        final CountDownLatch entering = new CountDownLatch(1);
        volatile String outcome = "never-ran";
        volatile long parkedMs = -1;
    }

    /** Classify an outcome to a stable, VM-independent token. */
    static String nameOf(Throwable t) {
        if (t instanceof SocketException) {
            return "SocketException";
        }
        return t.getClass().getName();
    }

    /**
     * Park a helper thread inside `in.read()`. Daemon on purpose: if the read
     * never wakes, the assertion below must be what ends the process, and a
     * non-daemon thread stuck in a native read would keep the VM alive past it.
     */
    static Parked parkReader(final InputStream in) {
        final Parked p = new Parked();
        p.thread = new Thread(new Runnable() {
            public void run() {
                p.entering.countDown();
                long t0 = System.nanoTime();
                try {
                    int n = in.read();
                    p.outcome = "returned:" + n;
                } catch (Throwable t) {
                    p.outcome = nameOf(t);
                }
                p.parkedMs = (System.nanoTime() - t0) / 1000000L;
            }
        }, "RChannelInterrupt-parked-reader");
        p.thread.setDaemon(true);
        p.thread.start();
        return p;
    }

    public static void main(String[] args) throws Exception {
        Path p = Files.createTempFile("rchanint", ".dat");
        try {
            // Baseline: a normal, non-interrupted write works.
            try (FileChannel fc = FileChannel.open(p, StandardOpenOption.WRITE)) {
                check(fc.write(ByteBuffer.wrap(new byte[64]), 0) == 64, "plain write");
                check(fc.isOpen(), "channel should still be open");
            }

            // Interrupted thread: the write must raise ClosedByInterruptException
            // (asynchronous close), not NullPointerException.
            FileChannel fc = FileChannel.open(p, StandardOpenOption.WRITE);
            String outcome;
            boolean stillOpen;
            Thread.currentThread().interrupt();
            try {
                fc.write(ByteBuffer.wrap(new byte[64]), 0);
                outcome = "no-exception";
            } catch (ClosedByInterruptException e) {
                outcome = "ClosedByInterruptException";
            } catch (IOException e) {
                outcome = "IOException:" + e.getClass().getName();
            } catch (RuntimeException e) {
                outcome = "RuntimeException:" + e.getClass().getName();
            }
            // Interrupt status survives the operation; clear it so the rest of
            // this test (and the VM's own shutdown) is not affected.
            boolean flagKept = Thread.interrupted();
            stillOpen = fc.isOpen();
            try {
                fc.close();
            } catch (IOException ignored) {
                // already closed by the interrupt machinery
            }

            check(outcome.equals("ClosedByInterruptException"),
                    "interrupted write outcome was " + outcome
                            + " (a NullPointerException here means interruptor is null)");
            check(flagKept, "interrupt status must survive ClosedByInterruptException");
            check(!stillOpen, "the channel must be closed after an interrupted operation");
            System.out.println("CK FileChannel interrupted-write ClosedByInterruptException");

            socketReadWakesOnAsyncClose();

            System.out.println("CK RChannelInterrupt checks=" + checks);
            System.out.println("PASS RChannelInterrupt");
        } finally {
            Files.deleteIfExists(p);
        }
    }

    /**
     * A thread parked in `Socket.getInputStream().read()` must be woken by
     * another thread's `Socket.close()` and must see a `SocketException`.
     *
     * This is the assertion the field-presence checks could not make. Before the
     * wave-2 fix in `native-io/src/net.rs` the reader stayed parked and this
     * method's {@code WAKE_BUDGET_MS} join expired.
     */
    static void socketReadWakesOnAsyncClose() throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();
        ServerSocket listener = new ServerSocket(0, 4, lo);
        Socket client = null;
        Socket accepted = null;
        try {
            client = new Socket();
            client.connect(new InetSocketAddress(lo, listener.getLocalPort()), 10000);
            // Held open only so the peer never sends EOF: with the far end alive
            // and silent, read() can only return by being woken.
            accepted = listener.accept();

            Parked r = parkReader(client.getInputStream());
            check(r.entering.await(10, TimeUnit.SECONDS),
                    "Socket read/close: the reader thread never started");
            Thread.sleep(PARK_MS);
            client.close();

            r.thread.join(WAKE_BUDGET_MS);
            check(!r.thread.isAlive(),
                    "Socket read/close: a thread parked in Socket.getInputStream().read() was "
                            + "STILL BLOCKED " + WAKE_BUDGET_MS + " ms after another thread called "
                            + "Socket.close(). Asynchronous close must wake it. This is the "
                            + "blocked-reader-never-wakes defect (native-io/src/net.rs).");
            check(r.outcome.equals("SocketException"),
                    "Socket read/close: the woken read reported " + r.outcome
                            + "; the specified outcome is java.net.SocketException "
                            + "(HotSpot 25: \"Socket closed\")");
            check(r.parkedMs >= PARKED_PROOF_MS,
                    "Socket read/close: read() returned after only " + r.parkedMs + " ms, but the "
                            + "closer does not act for " + PARK_MS + " ms — the reader never "
                            + "actually parked, so this scenario proved nothing about "
                            + "asynchronous close");
            System.out.println("CK Socket-read async-close wakes-with-SocketException");
        } finally {
            closeQuietly(accepted);
            closeQuietly(client);
            closeQuietly(listener);
        }
    }

    static void closeQuietly(Socket s) {
        if (s != null) {
            try {
                s.close();
            } catch (IOException ignored) {
                // teardown only
            }
        }
    }

    static void closeQuietly(ServerSocket s) {
        if (s != null) {
            try {
                s.close();
            } catch (IOException ignored) {
                // teardown only
            }
        }
    }
}
