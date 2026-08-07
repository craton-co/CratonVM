import java.io.IOException;
import java.lang.reflect.Field;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.AsynchronousCloseException;
import java.nio.channels.Channel;
import java.nio.channels.ClosedByInterruptException;
import java.nio.channels.ClosedChannelException;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * Regression: BUG-NIO-NULL-INTERRUPTOR-20260726, socket half, PLUS the
 * blocked-reader-never-wakes defect in the NIO socket path.
 *
 * <h2>Part 1 — the seeded interruptor (2026-07-27)</h2>
 *
 * `RChannelInterrupt` closed the `FileChannelImpl` half of this on 2026-07-26.
 * `SocketChannel` / `ServerSocketChannel` are built by the same bridge and were
 * left with the same null `AbstractInterruptibleChannel.interruptor`, which
 * `begin()` dereferences unconditionally once the calling thread's interrupt
 * flag is set. They were deliberately left open at the time: seeding the field
 * needs an allocation, and the shared `init_channel_locks` carries an
 * empirically-earned warning against allocating on that path (an allocation
 * there once relocated the channel under concurrent load and left `closeLock`
 * null, killing the Apache httpasyncclient reactor).
 *
 * Fixed 2026-07-27 by sequencing the allocation AFTER the three monitor fields
 * and pinning the channel across it.
 *
 * The part-1 assertion is on the FIELD, because reading a `java.base` internal
 * needs `--add-opens java.base/java.nio.channels.spi=ALL-UNNAMED`, which the
 * suite does not pass; where reflection is refused the check degrades to
 * "non-null". HotSpot WITH the flags was checked by hand and reports
 * `AbstractInterruptibleChannel$1` on all three channels.
 *
 * <h2>Part 2 — BEHAVIOUR (added 2026-08-07 by the vacuous-test sweep)</h2>
 *
 * Part 1 alone was a VACUOUS regression guard for anything except the null
 * field. Its own header used to say so: it never parked a reader and never
 * closed or interrupted one from another thread, so — in its own words — "a
 * blocked read is reported as OK". When a wave-2 lane fixed a real
 * blocked-reader-never-wakes defect in `SocketChannel` asynchronous close, this
 * class was green BEFORE and AFTER the fix. A test that cannot go red for the
 * defect it names is not coverage.
 *
 * Part 2 therefore parks a real reader and asserts the specified wake-up:
 *
 * <ul>
 *   <li>blocking {@code SocketChannel.read()} + {@code close()} from another
 *       thread &rarr; {@code AsynchronousCloseException}, channel closed;</li>
 *   <li>blocking {@code SocketChannel.read()} + {@code Thread.interrupt()}
 *       &rarr; {@code ClosedByInterruptException}, channel closed, interrupt
 *       status preserved — this is the behavioural counterpart to the part-1
 *       field check, since it is exactly the path that used to NPE;</li>
 *   <li>{@code read()} on an already-closed channel &rarr; the PLAIN
 *       {@code ClosedChannelException} (the negative control: it separates "the
 *       close was seen" from "the parked read was woken");</li>
 *   <li>blocking {@code ServerSocketChannel.accept()} + {@code close()} &rarr;
 *       {@code AsynchronousCloseException}.</li>
 * </ul>
 *
 * `SocketChannel.read()` and `Socket.getInputStream().read()` are SEPARATE
 * implementations behind SEPARATE close registries in CratonVM
 * (`native-io/src/socket_channel.rs` vs `native-io/src/net.rs`), so this class
 * covers only the NIO half. `RChannelInterrupt` owns the `java.net` stream half.
 *
 * Measured on HotSpot 25.0.3 (Microsoft build 25.0.3+9-LTS), 3/3 runs: every
 * parked operation wakes ~403 ms after entry with the exception named above.
 *
 * EVERY wait here is bounded. A defect must produce a named `AssertionError`,
 * never an `rc=124` from the suite's 120 s per-class timeout, because a timeout
 * is indistinguishable from a VM hang. Helper threads are daemons so a reader
 * that genuinely never wakes cannot keep the VM alive past the failure.
 */
public class RSocketChannelInterrupt {
    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RSocketChannelInterrupt: " + m);
        }
    }

    /** How long the closer/interrupter waits after the reader signals entry. */
    static final long PARK_MS = 400;
    /**
     * Lower bound on time spent inside the blocking call for the scenario to
     * have proven anything. The actor sleeps {@link #PARK_MS} first, so a SLOW
     * machine only makes the observed value larger — this is a floor this test
     * creates itself, not a wall-clock guess about the host.
     */
    static final long PARKED_PROOF_MS = 250;
    /** Bounded join. Still alive after this is the defect, not slowness. */
    static final long WAKE_BUDGET_MS = 15000;

    /** Sentinel: the field exists but this VM will not let us read it. */
    static final Object BLOCKED = new Object();

    /** Read a field declared anywhere in the channel's ancestry. */
    static Object peek(Object o, String name) {
        for (Class<?> c = o.getClass(); c != null; c = c.getSuperclass()) {
            Field f;
            try {
                f = c.getDeclaredField(name);
            } catch (NoSuchFieldException ignored) {
                continue; // keep walking up
            }
            try {
                f.setAccessible(true);
                return f.get(o);
            } catch (RuntimeException | IllegalAccessException blocked) {
                return BLOCKED;
            }
        }
        return null;
    }

    static void assertSeeded(Object ch, String what) {
        Object interruptor = peek(ch, "interruptor");
        check(interruptor != null,
                what + ".interruptor is null — AbstractInterruptibleChannel.begin() "
                        + "NPEs on any thread whose interrupt flag is set");
        // Not counted: it only runs where reflection is permitted, and the
        // check tally is part of the cross-VM output diff.
        if (interruptor != BLOCKED) {
            String owner = interruptor.getClass().getName();
            if (!owner.startsWith("java.nio.channels.spi.AbstractInterruptibleChannel$")) {
                throw new AssertionError("RSocketChannelInterrupt: " + what
                        + ".interruptor is not the JDK's own Interruptible: " + owner);
            }
        }
        check(peek(ch, "closeLock") != null, what + ".closeLock is null");
        System.out.println("CK " + what + " interruptor-seeded closeLock-seeded");
    }

    // ------------------------------------------------------------------
    // Part 2 scaffolding
    // ------------------------------------------------------------------

    /** A blocking channel operation whose exception is the thing under test. */
    interface Blocking {
        void run() throws Exception;
    }

    /** Outcome of a bounded, parked blocking operation on a helper thread. */
    static final class Parked {
        Thread thread;
        final CountDownLatch entering = new CountDownLatch(1);
        volatile String outcome = "never-ran";
        volatile long parkedMs = -1;
        volatile boolean interruptFlagAfter;
    }

    /**
     * Classify an outcome to a stable token. Ordered most-derived first:
     * `ClosedByInterruptException` extends `AsynchronousCloseException` extends
     * `ClosedChannelException`, so a catch chain in the wrong order would report
     * every one of them as the base class and quietly stop discriminating.
     */
    static String nameOf(Throwable t) {
        if (t instanceof ClosedByInterruptException) {
            return "ClosedByInterruptException";
        }
        if (t instanceof AsynchronousCloseException) {
            return "AsynchronousCloseException";
        }
        if (t instanceof ClosedChannelException) {
            return "ClosedChannelException";
        }
        return t.getClass().getName();
    }

    /** Start a daemon thread parked in {@code body} and return its record. */
    static Parked park(String tag, final Blocking body) {
        final Parked p = new Parked();
        p.thread = new Thread(new Runnable() {
            public void run() {
                p.entering.countDown();
                long t0 = System.nanoTime();
                try {
                    body.run();
                    p.outcome = "no-exception";
                } catch (Throwable t) {
                    p.outcome = nameOf(t);
                }
                p.parkedMs = (System.nanoTime() - t0) / 1000000L;
                p.interruptFlagAfter = Thread.currentThread().isInterrupted();
            }
        }, tag);
        p.thread.setDaemon(true);
        p.thread.start();
        return p;
    }

    /** Wait for the helper to reach its blocking call, then let it settle in. */
    static void awaitEntry(Parked p, String what) throws InterruptedException {
        check(p.entering.await(10, TimeUnit.SECONDS), what + ": helper thread never started");
        Thread.sleep(PARK_MS);
    }

    /** Bounded wake-up assertion — the whole point of part 2. */
    static void awaitWake(Parked p, String what) throws InterruptedException {
        p.thread.join(WAKE_BUDGET_MS);
        check(!p.thread.isAlive(),
                what + ": the parked operation was STILL BLOCKED " + WAKE_BUDGET_MS
                        + " ms after the close/interrupt. It must be woken. This is the "
                        + "blocked-reader-never-wakes defect "
                        + "(native-io/src/socket_channel.rs).");
    }

    /** The reader must have genuinely been inside the blocking call. */
    static void assertReallyParked(Parked p, String what) {
        check(p.parkedMs >= PARKED_PROOF_MS,
                what + ": the call returned after only " + p.parkedMs + " ms, but nothing acts on "
                        + "it for " + PARK_MS + " ms — it never actually parked, so this scenario "
                        + "proved nothing about asynchronous close");
    }

    /** A connected loopback `SocketChannel` pair; [0] is the reader side. */
    static SocketChannel[] connectedPair(String what) throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();
        ServerSocketChannel ssc = ServerSocketChannel.open();
        try {
            ssc.bind(new InetSocketAddress(lo, 0));
            ssc.configureBlocking(false);
            int port = ((InetSocketAddress) ssc.getLocalAddress()).getPort();
            SocketChannel client = SocketChannel.open();
            client.configureBlocking(true);
            client.connect(new InetSocketAddress(lo, port));
            SocketChannel peer = null;
            // Bounded: 400 * 5 ms = 2 s, then a named failure rather than a hang.
            for (int i = 0; i < 400 && peer == null; i++) {
                peer = ssc.accept();
                if (peer == null) {
                    Thread.sleep(5);
                }
            }
            check(peer != null, what + ": loopback accept() produced no channel within 2 s");
            return new SocketChannel[] { client, peer };
        } finally {
            closeQuietly(ssc);
        }
    }

    static void closeQuietly(Channel c) {
        if (c != null) {
            try {
                c.close();
            } catch (IOException ignored) {
                // teardown only
            }
        }
    }

    /** Park in `read()`, close from another thread: AsynchronousCloseException. */
    static void readWakesOnAsyncClose() throws Exception {
        final String what = "SocketChannel.read/close";
        SocketChannel[] pair = connectedPair(what);
        final SocketChannel reader = pair[0];
        try {
            Parked p = park("RSocketChannelInterrupt-close", new Blocking() {
                public void run() throws Exception {
                    reader.read(ByteBuffer.allocate(16));
                }
            });
            awaitEntry(p, what);
            reader.close();
            awaitWake(p, what);
            check(p.outcome.equals("AsynchronousCloseException"),
                    what + ": the woken read reported " + p.outcome
                            + "; the specified outcome is AsynchronousCloseException");
            assertReallyParked(p, what);
            check(!reader.isOpen(), what + ": the channel must be closed afterwards");
            System.out.println("CK SocketChannel-read async-close wakes-with-AsynchronousCloseException");
        } finally {
            closeQuietly(pair[0]);
            closeQuietly(pair[1]);
        }
    }

    /** Park in `read()`, interrupt the reader: ClosedByInterruptException. */
    static void readWakesOnInterrupt() throws Exception {
        final String what = "SocketChannel.read/interrupt";
        SocketChannel[] pair = connectedPair(what);
        final SocketChannel reader = pair[0];
        try {
            Parked p = park("RSocketChannelInterrupt-interrupt", new Blocking() {
                public void run() throws Exception {
                    reader.read(ByteBuffer.allocate(16));
                }
            });
            awaitEntry(p, what);
            p.thread.interrupt();
            awaitWake(p, what);
            check(p.outcome.equals("ClosedByInterruptException"),
                    what + ": the woken read reported " + p.outcome
                            + "; the specified outcome is ClosedByInterruptException "
                            + "(a NullPointerException here means interruptor is null and the "
                            + "part-1 field check above is lying)");
            check(p.interruptFlagAfter,
                    what + ": interrupt status must survive ClosedByInterruptException");
            check(!reader.isOpen(),
                    what + ": the channel must be closed after ClosedByInterruptException");
            System.out.println("CK SocketChannel-read interrupt wakes-with-ClosedByInterruptException");
        } finally {
            closeQuietly(pair[0]);
            closeQuietly(pair[1]);
        }
    }

    /**
     * Negative control. A read that never parked, on a channel closed BEFORE the
     * call, must raise the PLAIN `ClosedChannelException` — not the
     * asynchronous-close subclass. Without this, a VM that simply threw
     * `AsynchronousCloseException` from every closed-channel read would satisfy
     * both scenarios above without ever waking anything.
     */
    static void readOnAlreadyClosedIsPlainClosedChannel() throws Exception {
        final String what = "SocketChannel.read-on-already-closed";
        SocketChannel[] pair = connectedPair(what);
        try {
            pair[0].close();
            String outcome;
            try {
                pair[0].read(ByteBuffer.allocate(8));
                outcome = "no-exception";
            } catch (Throwable t) {
                outcome = nameOf(t);
            }
            check(outcome.equals("ClosedChannelException"),
                    what + ": reported " + outcome + "; a read on a channel closed BEFORE the call "
                            + "must be the plain ClosedChannelException, so that "
                            + "AsynchronousCloseException keeps meaning \"a PARKED operation was "
                            + "woken\"");
            System.out.println("CK SocketChannel-read already-closed ClosedChannelException");
        } finally {
            closeQuietly(pair[0]);
            closeQuietly(pair[1]);
        }
    }

    /** Park in `accept()`, close from another thread: AsynchronousCloseException. */
    static void acceptWakesOnAsyncClose() throws Exception {
        final String what = "ServerSocketChannel.accept/close";
        final ServerSocketChannel ssc = ServerSocketChannel.open();
        try {
            ssc.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            ssc.configureBlocking(true);
            Parked p = park("RSocketChannelInterrupt-accept", new Blocking() {
                public void run() throws Exception {
                    ssc.accept();
                }
            });
            awaitEntry(p, what);
            ssc.close();
            awaitWake(p, what);
            check(p.outcome.equals("AsynchronousCloseException"),
                    what + ": the woken accept reported " + p.outcome
                            + "; the specified outcome is AsynchronousCloseException");
            assertReallyParked(p, what);
            System.out.println("CK ServerSocketChannel-accept async-close "
                    + "wakes-with-AsynchronousCloseException");
        } finally {
            closeQuietly(ssc);
        }
    }

    public static void main(String[] args) throws Exception {
        // ---- Part 1: the seeded interruptor / closeLock fields.
        try (SocketChannel sc = SocketChannel.open()) {
            check(sc.isOpen(), "SocketChannel.open() must return an open channel");
            assertSeeded(sc, "SocketChannel");
        }

        try (ServerSocketChannel ssc = ServerSocketChannel.open()) {
            check(ssc.isOpen(), "ServerSocketChannel.open() must return an open channel");
            assertSeeded(ssc, "ServerSocketChannel");

            // bind + accept builds a THIRD channel through the same bridge path.
            ssc.bind(new InetSocketAddress("127.0.0.1", 0));
            ssc.configureBlocking(false);
            int port = ((InetSocketAddress) ssc.getLocalAddress()).getPort();
            check(port > 0, "bound ServerSocketChannel must report a local port");

            try (SocketChannel client = SocketChannel.open()) {
                client.configureBlocking(true);
                client.connect(new InetSocketAddress("127.0.0.1", port));
                SocketChannel accepted = null;
                for (int i = 0; i < 400 && accepted == null; i++) {
                    accepted = ssc.accept();
                    if (accepted == null) {
                        Thread.sleep(5);
                    }
                }
                check(accepted != null, "accept() produced no channel");
                assertSeeded(accepted, "accepted-SocketChannel");
                accepted.close();
            }
        }

        // ---- Part 2: the behaviour those fields exist to make possible.
        readWakesOnAsyncClose();
        readWakesOnInterrupt();
        readOnAlreadyClosedIsPlainClosedChannel();
        acceptWakesOnAsyncClose();

        System.out.println("CK RSocketChannelInterrupt checks=" + checks);
        System.out.println("PASS RSocketChannelInterrupt");
    }
}
