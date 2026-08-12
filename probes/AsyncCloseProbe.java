// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Does `close()` from another thread wake a thread blocked in a native
// read/write/accept?
//
// The contract, stated once for every row below: a thread parked in a blocking
// I/O call on a socket, channel or pipe MUST come back when a second thread
// closes it. JDK 25's `java.net.Socket.close()` is unconditional about it —
// "Any thread currently blocked in an I/O operation upon this socket WILL throw
// a SocketException" — and the `java.nio.channels` types say the same with
// `AsynchronousCloseException`. If it does not hold, `close()` from a second
// thread does nothing, the blocked thread stays blocked forever, and shutdown
// hangs.
//
// ---------------------------------------------------------------------------
// Why this file exists at all
// ---------------------------------------------------------------------------
//
// W2-2-blocked-reader-async-close-wakeup.md records a measurement taken on
// 2026-08-11 with an instrument it calls `AsyncCloseProbe`. That file was never
// in the repository — not in probes/, not in regression-suite/src/. The
// measurement is real and the instrument that produced it is not reproducible,
// which is the same thing as not having measured it. This is that instrument,
// written fresh and committed, so the next person can re-run the row rather
// than cite it.
//
// ---------------------------------------------------------------------------
// The two ways a probe like this lies, and what is done about each
// ---------------------------------------------------------------------------
//
// 1. IT CLOSES BEFORE THE READ BLOCKS. Then the read returns immediately for an
//    unrelated reason (the fd is already gone) and the row passes without ever
//    exercising the wakeup. Every row here therefore proves the park FIRST:
//    the worker publishes `entered` immediately before the blocking call, the
//    driver waits for that flag, then sleeps `probe.settleMs` (300 ms by
//    default, which is 12 poll slices at CratonVM's 25 ms close-poll cadence)
//    and asserts the worker has NOT returned. A row whose worker returned
//    inside the settle window is reported INCONCLUSIVE, never PASS — it did not
//    test the wakeup, and saying so is the whole point.
//
// 2. IT HANGS INSTEAD OF FAILING. A probe that hangs on failure is useless in a
//    suite: it converts a red into a stuck job. Every worker here is a DAEMON
//    thread and every wait is bounded — `join(probe.wakeMs)`, 4 s by default.
//    A worker still alive at the end of that join is reported as
//    `outcome=TIMEOUT` and the row FAILS. Nothing is retried, nothing is waited
//    on again, and `System.exit` at the end guarantees the JVM leaves even with
//    workers still parked in the kernel. That is the "a timeout that does not
//    KILL contaminates every later arm" rule applied to a probe: an expired row
//    must not leave anything running that a later row could trip over, so each
//    row's sockets are closed in a finally block whether it passed or not.
//
// ---------------------------------------------------------------------------
// Reading the output
// ---------------------------------------------------------------------------
//
//   PROBE <row> parked=<bool> ms=<n> outcome=<simple-name|none|TIMEOUT|returned:N> want=<...> <PASS|FAIL|INCONCLUSIVE>
//
//   parked=false      the call never blocked; the row tested nothing.
//   outcome=none      the worker returned but recorded nothing (should not
//                     happen; a bug in the row).
//   outcome=TIMEOUT   the worker was still blocked `probe.wakeMs` after the
//                     close. THIS IS THE DEFECT this whole family is about.
//   outcome=returned:N  the call completed normally with N. For a read that is
//                     the pre-fix Linux answer: `shutdown(SHUT_RD)` woke it with
//                     a clean EOF (-1) where the spec mandates an exception.
//                     It means the wakeup path works and the exception mapping
//                     does not — a different, milder defect.
//
// Exit code is 0 only when every row PASSes. INCONCLUSIVE rows fail the run
// too: a row that could not be made to park is a broken instrument, and an
// instrument nobody has seen agree with a control is exactly what this file
// exists to stop citing.
//
// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------
//
//   javac -d <out> probes/AsyncCloseProbe.java
//   java       -cp <out> AsyncCloseProbe            # HotSpot 25 — the oracle
//   cratonvm   -cp <out> AsyncCloseProbe            # Compatible (--real-jdk)
//   cratonvm --jdk-only -cp <out> AsyncCloseProbe   # strict
//   CRATONVM_REAL=-net-sockets cratonvm -cp <out> AsyncCloseProbe
//                                                   # the synthetic java.net.Socket surface
//
// Run the HotSpot arm FIRST and every time. It is the control that says the
// instrument works; a CratonVM row is only evidence once the same row has been
// seen to pass on HotSpot on the same host.
//
// One row can be run alone by naming it: `AsyncCloseProbe socketRead pipeRead`.

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.ByteBuffer;
import java.nio.channels.AsynchronousServerSocketChannel;
import java.nio.channels.AsynchronousSocketChannel;
import java.nio.channels.CompletionHandler;
import java.nio.channels.DatagramChannel;
import java.nio.channels.Pipe;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Set;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

public class AsyncCloseProbe {

    /** How long the driver waits, after the worker says it is about to block,
     *  before it believes the worker is really parked. */
    static final long SETTLE_MS = Long.getLong("probe.settleMs", 300L);
    /** Bounded join after the close. Expiry is a FAILURE, never a retry. */
    static final long WAKE_MS = Long.getLong("probe.wakeMs", 4000L);
    /** Bounded wait for the worker to even reach its blocking call. */
    static final long ENTER_MS = Long.getLong("probe.enterMs", 3000L);

    static int passed, failed, inconclusive;
    static Set<String> only;

    // -----------------------------------------------------------------------
    // Harness
    // -----------------------------------------------------------------------

    /** Per-row state the worker publishes and the driver reads. Everything is
     *  volatile because the two threads share it with no other synchronisation
     *  — deliberately, so the probe cannot accidentally rendezvous with the
     *  worker and mask a park that never happened. */
    static final class Row {
        volatile boolean entered;
        volatile boolean returned;
        volatile String outcome = "none";
    }

    interface Body {
        void run(Row row) throws Exception;
    }

    interface Closer {
        void close() throws Exception;
    }

    /** Record what a blocking call actually did, in the vocabulary the output
     *  section above documents. Called by the worker, never by the driver. */
    static void record(Row row, Throwable t) {
        row.outcome = t.getClass().getSimpleName();
    }

    static void recordReturn(Row row, long n) {
        row.outcome = "returned:" + n;
    }

    static void check(String name, String want, Body body, Closer closer, Closer cleanup) {
        check(name, want, true, body, closer, cleanup);
    }

    /** `expectPark == false` inverts the anti-vacuity verdict: the row PASSES
     *  when the call did NOT park. Exactly one row uses it — `selfTestNoPark`,
     *  the negative control that proves the guard in (b) below can fire at all.
     *  Without such a row the guard is a check nobody has seen refuse anything,
     *  which is the same standing as an assertion that cannot fail. */
    static void check(String name, String want, boolean expectPark, Body body, Closer closer,
            Closer cleanup) {
        if (only != null && !only.contains(name)) {
            return;
        }
        Row row = new Row();
        Thread worker = new Thread(() -> {
            try {
                body.run(row);
            } catch (Throwable t) {
                // A throw that escaped the body's own catch is still an
                // outcome, not a harness failure.
                if ("none".equals(row.outcome)) {
                    record(row, t);
                }
            } finally {
                row.returned = true;
            }
        }, "asyncclose-" + name);
        worker.setDaemon(true);

        long ms = -1;
        String verdict;
        boolean parked = false;
        try {
            worker.start();

            // (a) wait for the worker to reach its blocking call, bounded.
            long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(ENTER_MS);
            while (!row.entered && System.nanoTime() < deadline) {
                Thread.sleep(1);
            }
            if (!row.entered) {
                report(name, false, -1, "NEVER-ENTERED", want, "INCONCLUSIVE");
                inconclusive++;
                return;
            }

            // (b) THE ANTI-VACUITY CHECK. Give the call time to actually park,
            // then insist that it did. Closing before this point would test
            // nothing at all, because the call would return for an unrelated
            // reason.
            Thread.sleep(SETTLE_MS);
            parked = !row.returned;
            if (!expectPark) {
                // Negative control. A row that DID park here means the guard
                // would have let a vacuous row through, so that is the failure.
                String v = parked ? "FAIL" : "PASS";
                report(name, parked, 0, row.outcome, want, v);
                if (parked) {
                    failed++;
                } else {
                    passed++;
                }
                return;
            }
            if (!parked) {
                report(name, false, 0, row.outcome, want, "INCONCLUSIVE");
                inconclusive++;
                return;
            }

            // (c) the close, and a BOUNDED wait for the wakeup.
            long t0 = System.nanoTime();
            // `-Dprobe.skipClose=<row>` (or `*`) suppresses the close for a
            // row, which is how anyone can confirm ON THEIR OWN VM that this
            // probe REDs rather than hangs when no wakeup arrives. The expected
            // reading is `outcome=TIMEOUT ... FAIL` after `probe.wakeMs`, and a
            // process that still exits. Verified on HotSpot 25.0.3 / Windows 11
            // 2026-08-12: `-Dprobe.skipClose=socketRead` FAILs that row in
            // ~4 s and the run exits 1. A harness whose failure path has never
            // been executed is not known to have one.
            String skip = System.getProperty("probe.skipClose", "");
            if (!skip.equals("*") && !skip.equals(name)) {
                closer.close();
            }
            worker.join(WAKE_MS);
            ms = (System.nanoTime() - t0) / 1_000_000L;

            if (worker.isAlive()) {
                // The defect. Do not wait again, do not retry — the worker is
                // a daemon and `System.exit` below will leave without it.
                report(name, true, ms, "TIMEOUT", want, "FAIL");
                failed++;
                return;
            }
            verdict = matches(want, row.outcome) ? "PASS" : "FAIL";
            report(name, true, ms, row.outcome, want, verdict);
            if ("PASS".equals(verdict)) {
                passed++;
            } else {
                failed++;
            }
        } catch (Throwable t) {
            report(name, parked, ms, "HARNESS:" + t, want, "INCONCLUSIVE");
            inconclusive++;
        } finally {
            // Every row cleans up whether it passed, failed or expired. An
            // expired row that left a listener bound would contaminate every
            // later row on this port.
            try {
                cleanup.close();
            } catch (Throwable ignored) {
                // best effort
            }
        }
    }

    /** `want` is a `|`-separated list of acceptable outcomes. The token
     *  `returned` matches any normal completion (`returned:N`); every other
     *  token is an exact exception simple-name.
     *
     *  Alternation is used in exactly two rows and for a documented reason, not
     *  to make a red go green: a blocking channel WRITE that a close beats
     *  after some bytes are already out legitimately answers the partial count
     *  with no exception, because `SocketChannelImpl.write` calls
     *  `endWrite(bl, n > 0)` and `end(completed)` suppresses the throw. Both
     *  answers are wakeups; only TIMEOUT is the defect. */
    static boolean matches(String want, String outcome) {
        for (String token : want.split("[|]")) {
            if (token.equals("returned")) {
                if (outcome.startsWith("returned:")) {
                    return true;
                }
            } else if (token.equals(outcome)) {
                return true;
            }
        }
        return false;
    }

    static void report(String name, boolean parked, long ms, String outcome, String want,
            String verdict) {
        System.out.println("PROBE " + name
                + " parked=" + parked
                + " ms=" + ms
                + " outcome=" + outcome
                + " want=" + want
                + " " + verdict);
        System.out.flush();
    }

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    static final List<AutoCloseable> JUNK = new ArrayList<>();

    static void junk(AutoCloseable c) {
        JUNK.add(c);
    }

    static void closeQuietly(AutoCloseable c) {
        if (c != null) {
            try {
                c.close();
            } catch (Throwable ignored) {
                // best effort
            }
        }
    }

    /** A connected loopback TCP pair whose ACCEPTED side never reads. That is
     *  what makes the write rows park: the peer's receive buffer fills, then
     *  ours, and the blocking `send` stops returning. Buffers are shrunk on
     *  both sides so the fill happens in the low hundreds of KB rather than
     *  the low tens of MB. */
    static final class Pair implements AutoCloseable {
        final ServerSocket server;
        final Socket client;
        final Socket accepted;

        Pair(boolean smallBuffers) throws IOException {
            server = new ServerSocket();
            server.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            client = new Socket();
            if (smallBuffers) {
                client.setSendBufferSize(4096);
                client.setReceiveBufferSize(4096);
            }
            client.connect(server.getLocalSocketAddress(), 2000);
            accepted = server.accept();
            if (smallBuffers) {
                accepted.setReceiveBufferSize(4096);
            }
        }

        @Override
        public void close() {
            closeQuietly(accepted);
            closeQuietly(client);
            closeQuietly(server);
        }
    }

    /** The channel equivalent of {@link Pair}. */
    static final class ChannelPair implements AutoCloseable {
        final ServerSocketChannel server;
        final SocketChannel client;
        final SocketChannel accepted;

        ChannelPair() throws IOException {
            server = ServerSocketChannel.open();
            server.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            client = SocketChannel.open();
            client.connect(server.getLocalAddress());
            accepted = server.accept();
            client.configureBlocking(true);
            accepted.configureBlocking(true);
        }

        @Override
        public void close() {
            closeQuietly(accepted);
            closeQuietly(client);
            closeQuietly(server);
        }
    }

    // -----------------------------------------------------------------------
    // Rows
    // -----------------------------------------------------------------------

    /** `Socket.getInputStream().read()` — `net.rs::net_read0` on the real-JDK
     *  surface, `net_phase_e.rs::re1_socket_read_stream` on the synthetic one.
     *  This is the original W2-2 row (`RJdkNet:234`). */
    static void socketRead() throws Exception {
        Pair p = new Pair(false);
        junk(p);
        check("socketRead", "SocketException",
                row -> {
                    InputStream in = p.client.getInputStream();
                    row.entered = true;
                    try {
                        int n = in.read();
                        recordReturn(row, n);
                    } catch (Throwable t) {
                        record(row, t);
                    }
                },
                () -> p.client.close(),
                p::close);
    }

    /** `Socket.getOutputStream().write()` into a peer that never reads — the
     *  write twin of the row above. `net.rs::net_write0`. */
    static void socketWrite() throws Exception {
        Pair p = new Pair(true);
        junk(p);
        byte[] payload = new byte[1 << 20];
        check("socketWrite", "SocketException",
                row -> {
                    OutputStream out = p.client.getOutputStream();
                    row.entered = true;
                    try {
                        // Loop so the row parks on a host whose buffers are
                        // larger than one payload; the peer never reads, so
                        // this cannot drain.
                        for (int i = 0; i < 64; i++) {
                            out.write(payload);
                            out.flush();
                        }
                        recordReturn(row, 64L * payload.length);
                    } catch (Throwable t) {
                        record(row, t);
                    }
                },
                () -> p.client.close(),
                p::close);
    }

    /** `ServerSocket.accept()` on an idle port. */
    static void serverSocketAccept() throws Exception {
        ServerSocket ss = new ServerSocket();
        ss.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
        junk(ss);
        check("serverSocketAccept", "SocketException",
                row -> {
                    row.entered = true;
                    try {
                        Socket s = ss.accept();
                        closeQuietly(s);
                        recordReturn(row, 0);
                    } catch (Throwable t) {
                        record(row, t);
                    }
                },
                ss::close,
                ss::close);
    }

    /** Blocking `SocketChannel.read(ByteBuffer)` — `socket_channel.rs::sc_read`.
     *  The other original W2-2 row (`RJdkNio:347`). */
    static void channelRead() throws Exception {
        ChannelPair p = new ChannelPair();
        junk(p);
        check("channelRead", "AsynchronousCloseException",
                row -> {
                    ByteBuffer bb = ByteBuffer.allocate(64);
                    row.entered = true;
                    try {
                        int n = p.client.read(bb);
                        recordReturn(row, n);
                    } catch (Throwable t) {
                        record(row, t);
                    }
                },
                () -> p.client.close(),
                p::close);
    }

    /** Blocking `SocketChannel.write(ByteBuffer)` into a peer that never reads.
     *
     *  THREE answers are legal here, and which one arrives is decided by where
     *  in the write loop the close lands rather than by whether the wakeup
     *  worked:
     *
     *  * `AsynchronousCloseException` — the close beat the parked write with
     *    nothing transferred.
     *  * a partial count — `SocketChannelImpl.write` calls `endWrite(bl, n > 0)`,
     *    so `end(completed)` suppresses the throw once any byte is out.
     *  * `ClosedChannelException` — the parked write returned its partial count
     *    and the NEXT iteration of the loop below found the channel already
     *    closed, so `ensureOpen()` threw. This is what HotSpot 25.0.3 actually
     *    produced on Windows 11 (measured 2026-08-12, ms=0), and the `want` was
     *    corrected to it rather than the row being reshaped until it agreed
     *    with a guess.
     *
     *  All three are wakeups. The only outcome that is the defect is TIMEOUT,
     *  and `parked=true` is what makes that discrimination mean anything. */
    static void channelWrite() throws Exception {
        ChannelPair p = new ChannelPair();
        junk(p);
        check("channelWrite", "AsynchronousCloseException|ClosedChannelException|returned",
                row -> {
                    ByteBuffer bb = ByteBuffer.allocate(1 << 20);
                    row.entered = true;
                    try {
                        long total = 0;
                        for (int i = 0; i < 64; i++) {
                            bb.clear();
                            total += p.client.write(bb);
                        }
                        recordReturn(row, total);
                    } catch (Throwable t) {
                        record(row, t);
                    }
                },
                () -> p.client.close(),
                p::close);
    }

    /** Blocking `ServerSocketChannel.accept()`. */
    static void channelAccept() throws Exception {
        ServerSocketChannel ssc = ServerSocketChannel.open();
        ssc.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
        ssc.configureBlocking(true);
        junk(ssc);
        check("channelAccept", "AsynchronousCloseException",
                row -> {
                    row.entered = true;
                    try {
                        SocketChannel sc = ssc.accept();
                        closeQuietly(sc);
                        recordReturn(row, 0);
                    } catch (Throwable t) {
                        record(row, t);
                    }
                },
                ssc::close,
                ssc::close);
    }

    /** `DatagramSocket.receive` with nobody sending — the multicast/datagram
     *  receive chokepoint (`fd_table.rs::udp_recv`). */
    static void datagramReceive() throws Exception {
        DatagramSocket ds = new DatagramSocket(0, InetAddress.getLoopbackAddress());
        junk(ds);
        check("datagramReceive", "SocketException",
                row -> {
                    DatagramPacket pkt = new DatagramPacket(new byte[64], 64);
                    row.entered = true;
                    try {
                        ds.receive(pkt);
                        recordReturn(row, pkt.getLength());
                    } catch (Throwable t) {
                        record(row, t);
                    }
                },
                ds::close,
                ds::close);
    }

    /** Blocking `DatagramChannel.receive` with nobody sending. */
    static void datagramChannelReceive() throws Exception {
        DatagramChannel dc = DatagramChannel.open();
        dc.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
        dc.configureBlocking(true);
        junk(dc);
        check("datagramChannelReceive", "AsynchronousCloseException",
                row -> {
                    ByteBuffer bb = ByteBuffer.allocate(64);
                    row.entered = true;
                    try {
                        dc.receive(bb);
                        recordReturn(row, bb.position());
                    } catch (Throwable t) {
                        record(row, t);
                    }
                },
                dc::close,
                dc::close);
    }

    /** `Pipe.SourceChannel.read` on an empty pipe — `pipe.rs::source_read_buffer`.
     *  The pipe rows are the ones the registry re-ask could not reach even in
     *  principle, so they are the ones worth watching most closely. */
    static void pipeRead() throws Exception {
        Pipe pipe = Pipe.open();
        Pipe.SourceChannel src = pipe.source();
        Pipe.SinkChannel sink = pipe.sink();
        src.configureBlocking(true);
        junk(src);
        junk(sink);
        check("pipeRead", "AsynchronousCloseException",
                row -> {
                    ByteBuffer bb = ByteBuffer.allocate(64);
                    row.entered = true;
                    try {
                        int n = src.read(bb);
                        recordReturn(row, n);
                    } catch (Throwable t) {
                        record(row, t);
                    }
                },
                src::close,
                () -> {
                    closeQuietly(src);
                    closeQuietly(sink);
                });
    }

    /** `Pipe.SinkChannel.write` into a pipe nobody drains — `pipe.rs::
     *  sink_write_buffer`. The Windows arm of this one is the row W7-53 leaves
     *  OPEN, so a TIMEOUT here on Windows CratonVM is the expected reading of
     *  that record rather than a surprise. */
    static void pipeWrite() throws Exception {
        Pipe pipe = Pipe.open();
        Pipe.SourceChannel src = pipe.source();
        Pipe.SinkChannel sink = pipe.sink();
        sink.configureBlocking(true);
        junk(src);
        junk(sink);
        check("pipeWrite", "AsynchronousCloseException|ClosedChannelException|returned",
                row -> {
                    ByteBuffer bb = ByteBuffer.allocate(1 << 16);
                    row.entered = true;
                    try {
                        long total = 0;
                        // The default pipe buffer is small (4 KiB on Windows),
                        // and nobody reads `src`, so this parks quickly.
                        for (int i = 0; i < 64; i++) {
                            bb.clear();
                            total += sink.write(bb);
                        }
                        recordReturn(row, total);
                    } catch (Throwable t) {
                        record(row, t);
                    }
                },
                sink::close,
                () -> {
                    closeQuietly(src);
                    closeQuietly(sink);
                });
    }

    /** `AsynchronousServerSocketChannel.accept` with a CompletionHandler — the
     *  `async_socket.rs` `Job::Accept` worker arm. The "blocked thread" here is
     *  a pool thread rather than this one, so the row waits on the handler's
     *  latch instead of on the call. */
    static void asyncAccept() throws Exception {
        AsynchronousServerSocketChannel assc = AsynchronousServerSocketChannel.open();
        assc.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
        junk(assc);
        CountDownLatch done = new CountDownLatch(1);
        Row shared = new Row();
        check("asyncAccept", "AsynchronousCloseException",
                row -> {
                    assc.accept(null, new CompletionHandler<AsynchronousSocketChannel, Void>() {
                        @Override
                        public void completed(AsynchronousSocketChannel ch, Void att) {
                            closeQuietly(ch);
                            recordReturn(shared, 0);
                            done.countDown();
                        }

                        @Override
                        public void failed(Throwable t, Void att) {
                            record(shared, t);
                            done.countDown();
                        }
                    });
                    row.entered = true;
                    // Bounded, like every other wait in this file: expiry
                    // leaves `row.outcome` as "none" and the driver's own
                    // join is what decides the row.
                    if (done.await(WAKE_MS + SETTLE_MS, TimeUnit.MILLISECONDS)) {
                        row.outcome = shared.outcome;
                    }
                },
                assc::close,
                assc::close);
    }

    /** `AsynchronousSocketChannel.read` with a CompletionHandler — the
     *  `Job::ReadFd` worker arm, whose private `try_clone`d handle is why the
     *  registry re-ask does not reach it. */
    static void asyncRead() throws Exception {
        AsynchronousServerSocketChannel assc = AsynchronousServerSocketChannel.open();
        assc.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
        AsynchronousSocketChannel client = AsynchronousSocketChannel.open();
        client.connect(assc.getLocalAddress()).get(2, TimeUnit.SECONDS);
        AsynchronousSocketChannel accepted = assc.accept().get(2, TimeUnit.SECONDS);
        junk(assc);
        junk(client);
        junk(accepted);
        CountDownLatch done = new CountDownLatch(1);
        Row shared = new Row();
        check("asyncRead", "AsynchronousCloseException",
                row -> {
                    ByteBuffer bb = ByteBuffer.allocate(64);
                    client.read(bb, null, new CompletionHandler<Integer, Void>() {
                        @Override
                        public void completed(Integer n, Void att) {
                            recordReturn(shared, n);
                            done.countDown();
                        }

                        @Override
                        public void failed(Throwable t, Void att) {
                            record(shared, t);
                            done.countDown();
                        }
                    });
                    row.entered = true;
                    if (done.await(WAKE_MS + SETTLE_MS, TimeUnit.MILLISECONDS)) {
                        row.outcome = shared.outcome;
                    }
                },
                client::close,
                () -> {
                    closeQuietly(client);
                    closeQuietly(accepted);
                    closeQuietly(assc);
                });
    }

    /** NEGATIVE CONTROL. A read that cannot block, because the peer has
     *  already sent a byte.
     *
     *  This row exists to make the anti-vacuity guard falsifiable. Every other
     *  row's verdict rests on `parked=true`, and a `parked` check that has
     *  never been seen to say `false` is an instrument nobody has calibrated —
     *  precisely the standing the missing `AsyncCloseProbe` left W2-2's
     *  measurement in. If this row ever reports `parked=true`, the guard is
     *  broken and NO other row in this file means anything, however green.
     *
     *  It PASSES by not parking. */
    static void selfTestNoPark() throws Exception {
        Pair p = new Pair(false);
        junk(p);
        p.accepted.getOutputStream().write(42);
        p.accepted.getOutputStream().flush();
        // Give the byte time to arrive so the read below genuinely cannot block.
        Thread.sleep(100);
        check("selfTestNoPark", "must-not-park", false,
                row -> {
                    InputStream in = p.client.getInputStream();
                    row.entered = true;
                    try {
                        int n = in.read();
                        recordReturn(row, n);
                    } catch (Throwable t) {
                        record(row, t);
                    }
                },
                () -> p.client.close(),
                p::close);
    }

    // -----------------------------------------------------------------------

    public static void main(String[] args) throws Exception {
        if (args.length > 0) {
            only = new LinkedHashSet<>(Arrays.asList(args));
        }
        System.out.println("PROBE settleMs=" + SETTLE_MS + " wakeMs=" + WAKE_MS
                + " os=" + System.getProperty("os.name")
                + " java=" + System.getProperty("java.version")
                + " vm=" + System.getProperty("java.vm.name"));

        // Each row is independent and each cleans up after itself, so a row
        // that throws while being SET UP (an unsupported channel type, a bound
        // port) is reported and skipped rather than taking the run down.
        for (Runnable r : new Runnable[] {
                wrap("selfTestNoPark", AsyncCloseProbe::selfTestNoPark),
                wrap("socketRead", AsyncCloseProbe::socketRead),
                wrap("socketWrite", AsyncCloseProbe::socketWrite),
                wrap("serverSocketAccept", AsyncCloseProbe::serverSocketAccept),
                wrap("channelRead", AsyncCloseProbe::channelRead),
                wrap("channelWrite", AsyncCloseProbe::channelWrite),
                wrap("channelAccept", AsyncCloseProbe::channelAccept),
                wrap("datagramReceive", AsyncCloseProbe::datagramReceive),
                wrap("datagramChannelReceive", AsyncCloseProbe::datagramChannelReceive),
                wrap("pipeRead", AsyncCloseProbe::pipeRead),
                wrap("pipeWrite", AsyncCloseProbe::pipeWrite),
                wrap("asyncAccept", AsyncCloseProbe::asyncAccept),
                wrap("asyncRead", AsyncCloseProbe::asyncRead),
        }) {
            r.run();
        }

        for (AutoCloseable c : JUNK) {
            closeQuietly(c);
        }
        System.out.println("SUMMARY pass=" + passed + " fail=" + failed
                + " inconclusive=" + inconclusive);
        System.out.flush();
        // Hard exit. Some workers may still be parked in the kernel on a VM
        // that has the defect, and this probe must not become the hang it is
        // measuring.
        System.exit(failed == 0 && inconclusive == 0 ? 0 : 1);
    }

    interface Setup {
        void run() throws Exception;
    }

    static Runnable wrap(String name, Setup s) {
        return () -> {
            if (only != null && !only.contains(name)) {
                return;
            }
            try {
                s.run();
            } catch (Throwable t) {
                report(name, false, -1, "SETUP:" + t, "-", "INCONCLUSIVE");
                inconclusive++;
            }
        };
    }
}
