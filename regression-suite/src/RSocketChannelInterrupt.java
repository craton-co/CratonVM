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
 *   <li>the OTHER race direction: interrupt status already set BEFORE the
 *       blocking {@code read()} is entered &rarr; the same
 *       {@code ClosedByInterruptException}, immediately. An implementation that
 *       only polls the flag from inside its park loop loses every interrupt
 *       that lands while the reader is still on its way in;</li>
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
 * The pre-set-interrupt case is the one exception to the timing: it throws in
 * 0 ms, without parking, and does so even when bytes are already buffered on
 * the channel — {@code AbstractInterruptibleChannel.begin()} runs ahead of the
 * transfer, so an interrupted blocking read never delivers a byte. (Also
 * measured, and deliberately NOT asserted here because it belongs to the
 * selector suite: a NON-blocking read on a thread whose interrupt flag is set
 * returns 0 and leaves the channel OPEN. Blocking-only interruptibility is why
 * a reactor thread carrying an interrupt flag does not lose its channels.)
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
     * The other race direction. The interrupt is already pending when the
     * blocking `read()` is ENTERED — no park to poll out of. HotSpot answers
     * `ClosedByInterruptException` immediately, because
     * `SocketChannelImpl.beginRead` calls `AbstractInterruptibleChannel.begin()`
     * before the transfer and `begin()` closes the channel the moment it sees
     * the flag.
     *
     * A VM that checks the flag only from inside its park loop passes
     * {@link #readWakesOnInterrupt} and fails here, which is the whole reason
     * this scenario is separate: those are two different checks, not one.
     *
     * Bounded like every other scenario — if the flag is ignored the reader
     * parks forever with no data coming, and {@link #awaitWake} turns that into
     * a named `AssertionError` rather than a suite timeout.
     */
    static void readWithPreSetInterruptFailsImmediately() throws Exception {
        final String what = "SocketChannel.read/pre-set-interrupt";
        SocketChannel[] pair = connectedPair(what);
        final SocketChannel reader = pair[0];
        try {
            Parked p = park("RSocketChannelInterrupt-preinterrupt", new Blocking() {
                public void run() throws Exception {
                    // Set BEFORE entering the blocking call, on this very thread.
                    Thread.currentThread().interrupt();
                    reader.read(ByteBuffer.allocate(16));
                }
            });
            // Nothing else acts on this reader; the interrupt it carries in is
            // the only thing that can end the call.
            awaitWake(p, what);
            check(p.outcome.equals("ClosedByInterruptException"),
                    what + ": the read reported " + p.outcome
                            + "; an interrupt status already set on entry must raise "
                            + "ClosedByInterruptException before any transfer, not be "
                            + "noticed only by a park loop that this call never reaches");
            check(p.interruptFlagAfter,
                    what + ": interrupt status must survive ClosedByInterruptException");
            check(!reader.isOpen(),
                    what + ": the channel must be closed after ClosedByInterruptException");
            System.out.println("CK SocketChannel-read pre-set-interrupt "
                    + "ClosedByInterruptException-before-transfer");
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

    // ==================================================================
    // Part 3 — the REST of the interruptible-channel family (2026-08-07)
    //
    // Part 2 covers `read`. `write`, `accept` and `connect` are the same
    // `AbstractInterruptibleChannel` contract and were all three
    // uninterruptible in CratonVM, and blocking `write` additionally had no
    // close-aware loop at all — `try_write_nb` parked inside `send`, where
    // (measured on Windows 11 / JDK 25.0.3) a `shutdown(SHUT_WR)` — which is
    // the whole of what `sc_close` issues — does NOT wake it.
    //
    // Measured on HotSpot 25.0.3 (Microsoft build 25.0.3+9-LTS), 2/2 runs:
    //
    //   blocking write, flag pre-set                 ClosedByInterruptException, 0 ms
    //   blocking write, interrupt while parked       ClosedByInterruptException
    //   blocking write, close while parked, n>0      returns the PARTIAL count, NO exception
    //   blocking write, close while parked, n==0     AsynchronousCloseException
    //   blocking write on an already-closed channel  ClosedChannelException (even with the flag set)
    //   blocking accept, flag pre-set (+ pending)    ClosedByInterruptException, 0 ms, conn NOT accepted
    //   blocking accept, interrupt while parked      ClosedByInterruptException
    //   blocking connect, flag pre-set, live target  ClosedByInterruptException, 0 ms, isConnected()==false
    //   NON-blocking write / accept / connect, flag set   normal return, channel OPEN, flag still set
    //
    // The non-blocking rows are asserted here and are the load-bearing ones:
    // a `ServerSocketChannel` acceptor that lost its listener to a stale
    // interrupt flag on a pooled thread would never come back, so
    // "blocking-only" is a property this class has to be able to go RED for.
    //
    // NOT asserted, deliberately: anything delivered to a thread ALREADY
    // parked in a blocking `connect()`. That is two scenarios, not one, and
    // both are open:
    //
    //   blocking connect, interrupt while parked  HotSpot ClosedByInterruptException, ~0 ms
    //   blocking connect, close while parked      HotSpot AsynchronousCloseException  (measured
    //                                             2026-08-07, W8-3, same host/JDK as the rows above)
    //
    // CratonVM observes neither until the dial returns, because the dial lives
    // in `outbound_policy::policy_connect` behind a single blocking OS connect
    // — there is no close-aware poll loop there, unlike read/write/accept. See
    // the MEASURED GAP note in `sc_connect_inner` (native-io/src/socket_channel.rs).
    // Asserting either would make this class red for a gap that is named and
    // deferred rather than fixed — see the lane report.
    // ==================================================================

    /** Payload big enough to overrun any plausible socket buffer pair. */
    static final int STALL_BYTES = 4 * 1024 * 1024;

    /** Park in a big blocking `write()` to a peer that never reads. */
    static void writeWakesOnAsyncClose() throws Exception {
        final String what = "SocketChannel.write/close";
        SocketChannel[] pair = connectedPair(what);
        final SocketChannel writer = pair[0];
        final long[] wrote = { Long.MIN_VALUE };
        try {
            Parked p = park("RSocketChannelInterrupt-write-close", new Blocking() {
                public void run() throws Exception {
                    wrote[0] = writer.write(ByteBuffer.allocate(STALL_BYTES));
                }
            });
            awaitEntry(p, what);
            writer.close();
            awaitWake(p, what);
            assertReallyParked(p, what);
            // HotSpot: `SocketChannelImpl.write` retries `while (okayToRetry(n)
            // && isOpen())` and then `endWrite(bl, n > 0)`, so once ANY byte has
            // gone out `end()` sees completed==true and the partial count is
            // returned instead of an exception. A 4 MiB write into a loopback
            // socket always gets some bytes out, so this is the deterministic
            // arm; the zero-transferred arm (AsynchronousCloseException) cannot
            // be produced portably without shrinking SO_SNDBUF, which would
            // make the scenario about buffer sizing instead of wake-up.
            check(p.outcome.equals("no-exception"),
                    what + ": reported " + p.outcome + "; a close that lands after a PARTIAL "
                            + "transfer returns the byte count, it does not throw");
            check(wrote[0] > 0 && wrote[0] < STALL_BYTES,
                    what + ": returned " + wrote[0] + " of " + STALL_BYTES
                            + "; a partial count is the proof the write was parked mid-transfer "
                            + "when the close landed");
            check(!writer.isOpen(), what + ": the channel must be closed afterwards");
            System.out.println("CK SocketChannel-write async-close wakes-with-partial-count");
        } finally {
            closeQuietly(pair[0]);
            closeQuietly(pair[1]);
        }
    }

    /** Park in a big blocking `write()`, interrupt the writer. */
    static void writeWakesOnInterrupt() throws Exception {
        final String what = "SocketChannel.write/interrupt";
        SocketChannel[] pair = connectedPair(what);
        final SocketChannel writer = pair[0];
        try {
            Parked p = park("RSocketChannelInterrupt-write-interrupt", new Blocking() {
                public void run() throws Exception {
                    writer.write(ByteBuffer.allocate(STALL_BYTES));
                }
            });
            awaitEntry(p, what);
            p.thread.interrupt();
            awaitWake(p, what);
            assertReallyParked(p, what);
            check(p.outcome.equals("ClosedByInterruptException"),
                    what + ": the woken write reported " + p.outcome
                            + "; the specified outcome is ClosedByInterruptException. Note this "
                            + "is the one case where the interrupt OUTRANKS the bytes already "
                            + "transferred — end()'s interrupt arm has no `completed` guard, "
                            + "unlike its AsynchronousCloseException arm");
            check(p.interruptFlagAfter,
                    what + ": interrupt status must survive ClosedByInterruptException");
            check(!writer.isOpen(),
                    what + ": the channel must be closed after ClosedByInterruptException");
            System.out.println("CK SocketChannel-write interrupt wakes-with-ClosedByInterruptException");
        } finally {
            closeQuietly(pair[0]);
            closeQuietly(pair[1]);
        }
    }

    /** The other race direction: the flag is already set on entry to write(). */
    static void writeWithPreSetInterruptFailsImmediately() throws Exception {
        final String what = "SocketChannel.write/pre-set-interrupt";
        SocketChannel[] pair = connectedPair(what);
        final SocketChannel writer = pair[0];
        try {
            Parked p = park("RSocketChannelInterrupt-write-preinterrupt", new Blocking() {
                public void run() throws Exception {
                    Thread.currentThread().interrupt();
                    // Small and instantly writable: nothing about this write
                    // would block, so only the pre-set flag can fail it.
                    writer.write(ByteBuffer.wrap("hello".getBytes("UTF-8")));
                }
            });
            awaitWake(p, what);
            check(p.outcome.equals("ClosedByInterruptException"),
                    what + ": the write reported " + p.outcome
                            + "; an interrupt already set on entry must raise "
                            + "ClosedByInterruptException before any transfer, not be noticed "
                            + "only by a park loop this write never reaches");
            check(p.interruptFlagAfter,
                    what + ": interrupt status must survive ClosedByInterruptException");
            check(!writer.isOpen(),
                    what + ": the channel must be closed after ClosedByInterruptException");
            System.out.println("CK SocketChannel-write pre-set-interrupt "
                    + "ClosedByInterruptException-before-transfer");
        } finally {
            closeQuietly(pair[0]);
            closeQuietly(pair[1]);
        }
    }

    /** Negative control for write, mirroring the read one. */
    static void writeOnAlreadyClosedIsPlainClosedChannel() throws Exception {
        final String what = "SocketChannel.write-on-already-closed";
        SocketChannel[] pair = connectedPair(what);
        try {
            pair[0].close();
            String outcome;
            try {
                pair[0].write(ByteBuffer.wrap("x".getBytes("UTF-8")));
                outcome = "no-exception";
            } catch (Throwable t) {
                outcome = nameOf(t);
            }
            check(outcome.equals("ClosedChannelException"),
                    what + ": reported " + outcome + "; a write on a channel closed BEFORE the "
                            + "call must be the plain ClosedChannelException");
            System.out.println("CK SocketChannel-write already-closed ClosedChannelException");
        } finally {
            closeQuietly(pair[0]);
            closeQuietly(pair[1]);
        }
    }

    /**
     * THE GUARD. A NON-blocking write on a thread whose interrupt flag is set
     * must transfer normally and leave the channel OPEN.
     *
     * `SocketChannelImpl.beginWrite` calls `begin()` only `if (blocking)`, so
     * interruptibility is a property of blocking mode alone. An implementation
     * that probed the flag unconditionally would close a reactor's channels
     * the moment any pooled thread carried a stale interrupt — which is the
     * same defect class as an acceptor losing its listener, and is why this
     * check exists next to the positive ones rather than in some other class.
     */
    static void nonBlockingWriteWithInterruptStaysOpen() throws Exception {
        final String what = "SocketChannel.write/non-blocking+interrupt";
        SocketChannel[] pair = connectedPair(what);
        final SocketChannel writer = pair[0];
        final long[] wrote = { Long.MIN_VALUE };
        try {
            writer.configureBlocking(false);
            Parked p = park("RSocketChannelInterrupt-write-nb", new Blocking() {
                public void run() throws Exception {
                    Thread.currentThread().interrupt();
                    wrote[0] = writer.write(ByteBuffer.wrap("hello".getBytes("UTF-8")));
                }
            });
            awaitWake(p, what);
            check(p.outcome.equals("no-exception"),
                    what + ": reported " + p.outcome + "; a NON-blocking write is not an "
                            + "interruptible operation and must not throw");
            check(wrote[0] == 5,
                    what + ": wrote " + wrote[0] + " of 5 bytes; the transfer must happen");
            check(writer.isOpen(),
                    what + ": the channel must stay OPEN — closing a non-blocking channel "
                            + "because its thread carries an interrupt flag is a fabricated "
                            + "failure, and it is how a reactor loses every connection it owns");
            check(p.interruptFlagAfter,
                    what + ": the interrupt status must be left alone, not consumed");
            System.out.println("CK SocketChannel-write non-blocking+interrupt stays-open");
        } finally {
            closeQuietly(pair[0]);
            closeQuietly(pair[1]);
        }
    }

    /** Park in blocking `accept()`, interrupt the acceptor. */
    static void acceptWakesOnInterrupt() throws Exception {
        final String what = "ServerSocketChannel.accept/interrupt";
        final ServerSocketChannel ssc = ServerSocketChannel.open();
        try {
            ssc.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            ssc.configureBlocking(true);
            Parked p = park("RSocketChannelInterrupt-accept-interrupt", new Blocking() {
                public void run() throws Exception {
                    ssc.accept();
                }
            });
            awaitEntry(p, what);
            p.thread.interrupt();
            awaitWake(p, what);
            assertReallyParked(p, what);
            check(p.outcome.equals("ClosedByInterruptException"),
                    what + ": the woken accept reported " + p.outcome
                            + "; the specified outcome is ClosedByInterruptException");
            check(p.interruptFlagAfter,
                    what + ": interrupt status must survive ClosedByInterruptException");
            check(!ssc.isOpen(),
                    what + ": the listener must be closed after ClosedByInterruptException");
            System.out.println("CK ServerSocketChannel-accept interrupt "
                    + "wakes-with-ClosedByInterruptException");
        } finally {
            closeQuietly(ssc);
        }
    }

    /**
     * The other race direction for accept, with a connection ALREADY PENDING.
     * HotSpot refuses to hand it over: `begin()` closes the listener before
     * `accept()` is reached, so an implementation that only probed the flag on
     * the not-ready path would serve the connection and lose the interrupt.
     */
    static void acceptWithPreSetInterruptFailsImmediately() throws Exception {
        final String what = "ServerSocketChannel.accept/pre-set-interrupt";
        final ServerSocketChannel ssc = ServerSocketChannel.open();
        SocketChannel pending = null;
        try {
            ssc.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            ssc.configureBlocking(true);
            pending = SocketChannel.open((InetSocketAddress) ssc.getLocalAddress());
            Thread.sleep(150); // let the connection land in the backlog
            final Object[] accepted = { null };
            Parked p = park("RSocketChannelInterrupt-accept-preinterrupt", new Blocking() {
                public void run() throws Exception {
                    Thread.currentThread().interrupt();
                    accepted[0] = ssc.accept();
                }
            });
            awaitWake(p, what);
            check(p.outcome.equals("ClosedByInterruptException"),
                    what + ": the accept reported " + p.outcome
                            + "; an interrupt already set on entry must raise "
                            + "ClosedByInterruptException even with a connection waiting");
            check(accepted[0] == null,
                    what + ": the pending connection must NOT be accepted by an interrupted "
                            + "accept — that is the accept twin of \"an interrupted read does "
                            + "not deliver buffered bytes\"");
            check(p.interruptFlagAfter,
                    what + ": interrupt status must survive ClosedByInterruptException");
            check(!ssc.isOpen(), what + ": the listener must be closed afterwards");
            System.out.println("CK ServerSocketChannel-accept pre-set-interrupt "
                    + "ClosedByInterruptException-no-connection-served");
        } finally {
            closeQuietly(pending);
            closeQuietly(ssc);
        }
    }

    /**
     * THE ACCEPTOR GUARD. A NON-blocking `accept()` on a thread carrying an
     * interrupt flag must leave the listening socket OPEN.
     *
     * This is the check that goes red if the blocking-mode gate is ever
     * dropped. An `Acceptor` thread's `ServerSocketChannel` is the one channel
     * in a server that never gets reopened: close it once by mistake and the
     * port is gone for the life of the process.
     */
    static void nonBlockingAcceptWithInterruptKeepsTheListenerOpen() throws Exception {
        final String what = "ServerSocketChannel.accept/non-blocking+interrupt";
        final ServerSocketChannel ssc = ServerSocketChannel.open();
        try {
            ssc.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            ssc.configureBlocking(false);
            final Object[] accepted = { new Object() };
            Parked p = park("RSocketChannelInterrupt-accept-nb", new Blocking() {
                public void run() throws Exception {
                    Thread.currentThread().interrupt();
                    accepted[0] = ssc.accept();
                }
            });
            awaitWake(p, what);
            check(p.outcome.equals("no-exception"),
                    what + ": reported " + p.outcome + "; a NON-blocking accept is not an "
                            + "interruptible operation and must not throw");
            check(accepted[0] == null,
                    what + ": nothing was connecting, so accept() must answer null");
            check(ssc.isOpen(),
                    what + ": THE LISTENER MUST STAY OPEN. A selector-driven acceptor whose "
                            + "thread happens to carry an interrupt flag must not lose the "
                            + "listening socket — it is never reopened, so the port is gone "
                            + "for the life of the process");
            check(p.interruptFlagAfter,
                    what + ": the interrupt status must be left alone, not consumed");
            // The listener really is still usable, not merely reporting open.
            SocketChannel probe = SocketChannel.open((InetSocketAddress) ssc.getLocalAddress());
            SocketChannel served = null;
            for (int i = 0; i < 400 && served == null; i++) {
                served = ssc.accept();
                if (served == null) {
                    Thread.sleep(5);
                }
            }
            check(served != null,
                    what + ": the listener reported isOpen() but accepted nothing within 2 s — "
                            + "isOpen() is not enough, the port has to still work");
            closeQuietly(served);
            closeQuietly(probe);
            System.out.println("CK ServerSocketChannel-accept non-blocking+interrupt "
                    + "listener-still-serving");
        } finally {
            closeQuietly(ssc);
        }
    }

    /** The flag is already set on entry to a blocking `connect()`. */
    static void connectWithPreSetInterruptFailsImmediately() throws Exception {
        final String what = "SocketChannel.connect/pre-set-interrupt";
        final ServerSocketChannel ssc = ServerSocketChannel.open();
        final SocketChannel sc = SocketChannel.open();
        try {
            ssc.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            ssc.configureBlocking(false);
            final InetSocketAddress target = (InetSocketAddress) ssc.getLocalAddress();
            sc.configureBlocking(true);
            Parked p = park("RSocketChannelInterrupt-connect-preinterrupt", new Blocking() {
                public void run() throws Exception {
                    Thread.currentThread().interrupt();
                    // A LIVE listener: this connect would succeed instantly, so
                    // the only thing that can fail it is the pre-set flag.
                    sc.connect(target);
                }
            });
            awaitWake(p, what);
            check(p.outcome.equals("ClosedByInterruptException"),
                    what + ": the connect reported " + p.outcome
                            + "; an interrupt already set on entry must raise "
                            + "ClosedByInterruptException before the dial, even against a live "
                            + "local listener");
            check(p.interruptFlagAfter,
                    what + ": interrupt status must survive ClosedByInterruptException");
            check(!sc.isOpen(), what + ": the channel must be closed afterwards");
            check(!sc.isConnected(), what + ": an interrupted connect must not report connected");
            System.out.println("CK SocketChannel-connect pre-set-interrupt "
                    + "ClosedByInterruptException-before-dial");
        } finally {
            closeQuietly(sc);
            closeQuietly(ssc);
        }
    }

    /** The connect half of the non-blocking guard. */
    static void nonBlockingConnectWithInterruptStaysOpen() throws Exception {
        final String what = "SocketChannel.connect/non-blocking+interrupt";
        final ServerSocketChannel ssc = ServerSocketChannel.open();
        final SocketChannel sc = SocketChannel.open();
        try {
            ssc.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            ssc.configureBlocking(false);
            final InetSocketAddress target = (InetSocketAddress) ssc.getLocalAddress();
            sc.configureBlocking(false);
            Parked p = park("RSocketChannelInterrupt-connect-nb", new Blocking() {
                public void run() throws Exception {
                    Thread.currentThread().interrupt();
                    sc.connect(target);
                }
            });
            awaitWake(p, what);
            check(p.outcome.equals("no-exception"),
                    what + ": reported " + p.outcome + "; a NON-blocking connect is not an "
                            + "interruptible operation and must not throw");
            check(sc.isOpen(),
                    what + ": the channel must stay OPEN — see the accept guard for why this "
                            + "half of the contract is the one that protects servers");
            check(p.interruptFlagAfter,
                    what + ": the interrupt status must be left alone, not consumed");
            System.out.println("CK SocketChannel-connect non-blocking+interrupt stays-open");
        } finally {
            closeQuietly(sc);
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
        readWithPreSetInterruptFailsImmediately();
        readOnAlreadyClosedIsPlainClosedChannel();
        acceptWakesOnAsyncClose();

        // ---- Part 3: write / accept / connect, both race directions, plus
        // the non-blocking negative controls that keep the gate honest.
        writeWakesOnAsyncClose();
        writeWakesOnInterrupt();
        writeWithPreSetInterruptFailsImmediately();
        writeOnAlreadyClosedIsPlainClosedChannel();
        nonBlockingWriteWithInterruptStaysOpen();
        acceptWakesOnInterrupt();
        acceptWithPreSetInterruptFailsImmediately();
        nonBlockingAcceptWithInterruptKeepsTheListenerOpen();
        connectWithPreSetInterruptFailsImmediately();
        nonBlockingConnectWithInterruptStaysOpen();

        System.out.println("CK RSocketChannelInterrupt checks=" + checks);
        System.out.println("PASS RSocketChannelInterrupt");
    }
}
