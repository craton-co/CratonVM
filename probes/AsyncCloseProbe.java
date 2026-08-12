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
import java.security.KeyStore;
import java.security.cert.X509Certificate;
import java.util.Base64;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLServerSocket;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.TrustManager;
import javax.net.ssl.X509TrustManager;

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
            // BOUNDED, and on its own daemon thread (W7-61). The close used to
            // run inline, which silently assumed `close()` returns. It does not
            // always: measured on HotSpot 25.0.3 / Windows 11 2026-08-12,
            // `SSLSocket.close()` called while another thread is parked inside
            // `SSLSocket.getOutputStream().write()` BLOCKS — JSSE serialises
            // `duplexCloseOutput()` against the write it would have to
            // interrupt — so an inline close turned this probe into the hang it
            // measures. The 13 pre-existing rows are unaffected: their closers
            // return immediately, so the thread hop costs nothing and the
            // timings are unchanged.
            Thread closerThread = null;
            if (!skip.equals("*") && !skip.equals(name)) {
                closerThread = new Thread(() -> {
                    try {
                        closer.close();
                    } catch (Throwable ignored) {
                        // A close that throws is still a close.
                    }
                }, "asyncclose-closer-" + name);
                closerThread.setDaemon(true);
                closerThread.start();
            }
            worker.join(WAKE_MS);
            ms = (System.nanoTime() - t0) / 1_000_000L;

            if (worker.isAlive()) {
                // Distinguish "the wakeup never arrived" from "the close itself
                // never ran". They look identical from the worker's side and
                // mean opposite things: the first is the defect this file is
                // about; the second means the row never delivered its stimulus,
                // so it tested nothing and must not be scored as a failure of
                // the VM under test. Do not wait again, do not retry — every
                // thread here is a daemon and `System.exit` below leaves
                // without them.
                if (closerThread != null && closerThread.isAlive()) {
                    // A row whose `want` lists CLOSE_BLOCKED is asserting that
                    // this IS the reference behaviour — see `tlsWrite`, where
                    // HotSpot 25 measurably serialises `close()` behind a
                    // parked TLS write. Anywhere else it means the stimulus was
                    // never delivered, so the row tested nothing.
                    if (matches(want, "CLOSE_BLOCKED")) {
                        report(name, true, ms, "CLOSE_BLOCKED", want, "PASS");
                        passed++;
                        return;
                    }
                    report(name, true, ms, "CLOSE_BLOCKED", want, "INCONCLUSIVE");
                    inconclusive++;
                    return;
                }
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
            // Bounded for the same reason the close is (W7-61): a cleanup
            // that closes a TLS socket a worker is still parked in a write on
            // can itself block, and an unbounded cleanup in a `finally` is a
            // hang no verdict can be printed past.
            Thread cleanupThread = new Thread(() -> {
                try {
                    cleanup.close();
                } catch (Throwable ignored) {
                    // best effort
                }
            }, "asyncclose-cleanup-" + name);
            cleanupThread.setDaemon(true);
            cleanupThread.start();
            try {
                cleanupThread.join(WAKE_MS);
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
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

    // =======================================================================
    // TLS rows (W7-61)
    // =======================================================================
    //
    // W7-53 fixed 19 blocking sites with one loop — park in `poll` on a bounded
    // slice, re-ask the registry AFTER the poll, return `Interrupted` once the
    // slot is gone — and deliberately left FOUR TLS sites alone, because a
    // close-aware loop must not abandon a read MID-RECORD. TLS is framed: a
    // reader that returns between two of the `recv`s that make up one record
    // hands its caller a fragment and desynchronises the stream for good, which
    // is worse than the hang it fixes.
    //
    // The four sites are `servlet.rs::s2_tls_read_direct` / `s2_tls_write`
    // (native-tls) and `t27_tls.rs::rustls_stream_read` / `rustls_stream_write`
    // (rustls). All four share one shape: they clone an `Arc` on the stream out
    // of a registry, release the registry lock, and block. `close()` removes
    // the registry entry and `try_lock`s the stream — which a parked reader
    // holds — so nothing closes the socket and nothing wakes the reader.
    //
    // These rows measure exactly that, and NOT more than that. What they cannot
    // measure, said here so nobody reads a green as more than it is:
    //
    //   * They do not prove a record arrived WHOLE. Asserting that a TLS read
    //     returned something is not a test that it returned a complete record —
    //     that is the vacuous shape this family is prone to. `tlsReadIntegrity`
    //     below is the row that does test it, by comparing the exact bytes of a
    //     multi-record payload, and it is the row that would go red if a future
    //     close-aware loop were bolted on at the wrong level.
    //   * On Windows they exercise only half the fix. Winsock has no `shutdown`
    //     that aborts a pending blocking call, so a reader ALREADY parked
    //     cannot be woken at all; `tlsRead` is expected to report TIMEOUT on
    //     Windows CratonVM and that is the row W7-61 leaves open, not a
    //     surprise. On Unix `shutdown(SHUT_RDWR)` on the registry-held
    //     duplicate ends the byte stream and the reader returns.

    /** A self-signed PKCS12 (CN=localhost, SAN dns:localhost + ip:127.0.0.1,
     *  RSA 2048, 36500 days) generated with JDK 25's own `keytool` on
     *  2026-08-12. Embedded rather than generated at runtime on purpose: making
     *  a certificate at runtime needs `sun.security.x509` internals, and a
     *  probe that needs `--add-exports` is a probe that gets skipped. The
     *  password is `probepass` for both store and key; it protects nothing. */
    static final String PROBE_P12_BASE64 =
            "MIIKcgIBAzCCChwGCSqGSIb3DQEHAaCCCg0EggoJMIIKBTCCBawGCSqGSIb3DQEHAaCCBZ0EggWZ" +
            "MIIFlTCCBZEGCyqGSIb3DQEMCgECoIIFQDCCBTwwZgYJKoZIhvcNAQUNMFkwOAYJKoZIhvcNAQUM" +
            "MCsEFFW6SVT5Y6wROnXE2QeEU+MuObqvAgInEAIBIDAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQB" +
            "KgQQ6T6pQ0YdKhQyuDH1/SxTVwSCBNB3dZtxkUy89HHK86TN0GxSMDQJMG+OptOYP4KdZG/7IDCL" +
            "AEVJ7wdPby70AwO4eKoNT5rI3xvozUC1HQRCZ0Xw4FWOzAQhP64Q/8ZdYlOa1afkFu38PkY7ivbR" +
            "szq7MG1nhpX1w6Yo6ypaHbpROlTZZoG2w86SKfhe1cASpSSPYAvR5LTzNTY7bYX1cxufJGG5YNyu" +
            "hwyYVuUlqlVioYVaDo+lXIhoDp7WI7+dUnrSwxMn8MQzCNgQm+Ok+nTK+LGIUhwE0T4aJqrsRM57" +
            "qNXSDMJK+0N0A4Ye/zr8w8EoRtF5opC1eRxj9btkeDjDybruGxazRV1GXcVsBvc0OURACU6bcQvj" +
            "rElFeRN7zS+++Zi97ZMigBn4kamML5VRrq2ldaXozJah5oGe8tQki+fhtfqxSb9FQC/1wCYroNWR" +
            "UXV5WAACcPp0PS45R5u+6Ip4rt6dKTWxNfvK/8jrVw9qo7aFVIU8gAtklEUURNF7LWjOdT9bMKQ/" +
            "bXxXx6OcY41EghoNjKza9SuCff6birmKgEVIo/iOyQxq7TM2zvy3hEShemh14KTKUsEzelg738Tt" +
            "H69RAZPK7GGb/O6EXOrxsrjdFWX8pkvpJTOO3Ut3aWuRsgosvL2vALAGfKxs2EXbqWCFYrVpPQAa" +
            "HXVnTm5ArsmTccqCpyxdKdBX/4DlJObk90jAbqScZkUZ4qZryG9j5ZyjHvxaHVs9dx3EL9y14CQe" +
            "vVPxtR4p+ZLRbF26DETh5FL6TVS1ylQkhN+PSqyQm1JheLQT0+nsRLAK0fZUyvugCMvFt7OJ45v3" +
            "f0cAgto2JIiKz+Wa68J8vE9Y5sKzilm8Lm3bSAuXOrCjkY/aGP0NA9DsYPf8SI2gEDGOl3RmR042" +
            "iosjsWc/Qs2W6LOCTs9ALMIh0rxsH3nNez1WNv9KZZguQGleSID+40fBCGO9KjEOF1MhZRE1BZ8y" +
            "wQpdPrmUl3By2nffrSddbTvpP9GnCPRKo1XFI4eV0OmRIgx8UmHg3+YZXa+HTx5TxBZNO0/DgiQp" +
            "SfbCBUWdTa29f3xU8rPWrq5nKv3DHL/1FDVFIqrrssZbDJpQ6F0J/7EHMVqexA9u0vH8VhWu4TrY" +
            "WLWr9JL6rSll7CHfWNBcWa3b7ZrNVD2MPOsSd0GmgDkF4tlWZkWCKeePw8L7mKvZ2b+wbl+w2HqG" +
            "Ok+aXydKfUyGp7wsjxuy9EGIsNfEO+EKkUmiST8TXd/mEKhfNgOTCbCGhbUQ6/ualFFT8q8UskSZ" +
            "zMZPjzPEHrEOnKMx1wkRfo2f1y8WdE8R+43w2EXSYqA22GYN1qrXLY/h0JTeBbA5gNjurlKfSSr1" +
            "j3077e+FSxbuZV6IsbtuUDgBt9kRqaWG8ZckIcJL8vt5MCTDG+qSUDJPTYELugPb7CQBnXktKfGy" +
            "RFUL+BCwg/KkxwWViv57wvkwnRZ/7MsrgAdnw6RZ2AFHqV9iwq5HK0BkYfyXgYHnZPNGqTxHN6lB" +
            "kGfOfiy8lD03YFQBMEbFJQ7YWGEgCyJbEZnfDYsFF4IRwn7aMSuTNUAJn774F35UuqKAOV5NfMVa" +
            "AavHOCqmMQjiMe+AwgUij25dBLdnVVCyI1sbZzHkJbYIwZBOkzmNBYhpxHRX4+PTTTDrvEG6rqb9" +
            "wTE+MBkGCSqGSIb3DQEJFDEMHgoAcAByAG8AYgBlMCEGCSqGSIb3DQEJFTEUBBJUaW1lIDE3ODY1" +
            "MTgwMDE0MzUwggRRBgkqhkiG9w0BBwagggRCMIIEPgIBADCCBDcGCSqGSIb3DQEHATBmBgkqhkiG" +
            "9w0BBQ0wWTA4BgkqhkiG9w0BBQwwKwQUsJOcPrutcY9/BVVCi6GJRdY4WNICAicQAgEgMAwGCCqG" +
            "SIb3DQIJBQAwHQYJYIZIAWUDBAEqBBA/UUhlF0RtElz39q55/qWjgIIDwA61UQu+PPa9QFnciYDV" +
            "93bTLEyxPDvhMrYgBNXUY/A/DkVyWhkwO2GR01YzWPv0FkS2E+bMc5wuoviYCTEDmYrddnoWSKfT" +
            "tl4pmN3S6zGte8hXtn4PME7anvxpN1gchnT8Kw01MsRnk8uC865j6MeMCxL/LgefSYGQ3b1QVFWH" +
            "zks3ILtzm0wSV90EpfpC2QiV9aH2MXjEbEwO34SfDMKvEFR+WBGYflPw6Qwf4D0jnvACJXGZL806" +
            "62sRY++p4asG9jHn3P0Skg/SeQn0rmUpSGkxR3w0eWyYeN3ODvFcutjkWj/RXGZyKo19JWbNNM3+" +
            "WEA+dfVJ7olbJbpfroRo7Dbs7N4javHihogW80SA8cH5hFjQcxLA+FawMdguoiK4YLjNuq0ZdKdF" +
            "XMSADVF7RFad/NjS1wnA330DFtsOvs+V27MBoACCA7PBlQmweIC+a2ZdYixZuY4RhGmWKlSqo/Ad" +
            "zBC5DRW37xvFHOKk+Hu6OM6PTW2ZWBuVKYJ3lBw8AWfx3eH7D8HR5xVBqIMSut9DC30MmMBIIria" +
            "49U6B0C2JYsN4npN8saWBorKyAj769Tw8nxZyudVbGVE9EHYUOFu6pvjlgmUuEItsyGSbvZ6hImJ" +
            "Swv9an+1cwbvOY2tZXbhu8v4hWxGd8cefIEoKahJwhXFM7FsMIiFq7fPcaRu2OAWMNA9FNnM3521" +
            "g9uxBDUqPLesHo5ouKtbFi1hqvcReQkRGrAdb0G/eK/qutdSW5g/rBUijLihxMgzQE0mXE+yYVwX" +
            "e//ifxpzgh268jKlitC2TUgFCvn5cFkiaq6uXhaHspAZIAz/5phZjBa3BYrsX3eB28/oMSpMJ48u" +
            "3nYQm1Bow/uRiHB8/lv1JU2K4RjUHXSAUVfwrT3oNs62soSKQbShnByh+js89JjHewsEwSxRaFpQ" +
            "sxB5huv5/I3RxaE6bd2+PbrgIBolDwzw+3f9zNAe0+OcX+6cmbu85viK4JwkEHii2f4Ieu/wL8Qd" +
            "ue8AERb4S0V9i8jjP9RuI4wXUfImcq/Me8uUASXWbeOCp0shqwt3PVOxnPkrliIXOvCsADrB6KqQ" +
            "QW15z1zwmNMxnPhjZKhx6GwEz2eTDa4EgpB6S5zXvEgofyN+7tyA0oPMiQoHfs7oymtyfFmBx0sq" +
            "oFiW8kAqDZuMvVTk00ePE+K3xE1FMo7LTNpndkTrK1pee8BLc/wyki+fjmFjjZ9O7VlKpKf7R5Fw" +
            "HJX2JkxbyajV9dVL0TdQAixMrek5LwIVIxkobcKoJFgz4DBNMDEwDQYJYIZIAWUDBAIBBQAEIJ9s" +
            "2toD2oneEaemuGcXDVSiYJHPLAlYHngfo0D3WdfzBBTYXvNVdLHjy4oYd2Y9HqukF07EUwICJxA=";

    /** Trusts anything. These rows are about close-awareness, not PKI: a real
     *  trust decision would only add a second way for them to fail. Supplying
     *  an application trust manager is also what switches endpoint
     *  identification off in JSSE, so no hostname check runs — which is what
     *  keeps the rows from depending on how the host resolves `localhost`. */
    static final TrustManager[] TRUST_ALL = {
        new X509TrustManager() {
            public void checkClientTrusted(X509Certificate[] chain, String authType) {}

            public void checkServerTrusted(X509Certificate[] chain, String authType) {}

            public X509Certificate[] getAcceptedIssuers() {
                return new X509Certificate[0];
            }
        }
    };

    /** A connected, handshaken TLS pair on loopback. Both ends are real
     *  `SSLSocket`s; on CratonVM the client end is what routes into
     *  `s2_tls_read` / `s2_tls_write`. */
    static final class TlsPair implements AutoCloseable {
        final SSLServerSocket server;
        final SSLSocket client;
        volatile SSLSocket accepted;

        TlsPair() throws Exception {
            char[] pw = "probepass".toCharArray();
            KeyStore ks = KeyStore.getInstance("PKCS12");
            ks.load(new java.io.ByteArrayInputStream(
                    Base64.getDecoder().decode(PROBE_P12_BASE64)), pw);
            KeyManagerFactory kmf =
                    KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
            kmf.init(ks, pw);

            SSLContext serverCtx = SSLContext.getInstance("TLS");
            serverCtx.init(kmf.getKeyManagers(), TRUST_ALL, null);
            SSLContext clientCtx = SSLContext.getInstance("TLS");
            clientCtx.init(null, TRUST_ALL, null);

            server = (SSLServerSocket) serverCtx.getServerSocketFactory().createServerSocket();
            server.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));

            CountDownLatch up = new CountDownLatch(1);
            Thread acceptor = new Thread(() -> {
                try {
                    SSLSocket s = (SSLSocket) server.accept();
                    s.startHandshake();
                    accepted = s;
                } catch (Throwable ignored) {
                    // Reported by the constructor's own bounded wait below;
                    // never allowed to take the process down.
                } finally {
                    up.countDown();
                }
            }, "asyncclose-tls-accept");
            acceptor.setDaemon(true);
            acceptor.start();

            client = (SSLSocket) clientCtx.getSocketFactory()
                    .createSocket(InetAddress.getLoopbackAddress(), server.getLocalPort());
            client.startHandshake();
            up.await(10, TimeUnit.SECONDS);
            if (accepted == null) {
                throw new IOException("TLS handshake did not complete on the server side");
            }
        }

        @Override
        public void close() {
            closeQuietly(accepted);
            closeQuietly(client);
            closeQuietly(server);
        }
    }

    /** The negative control for the TLS rows specifically. `selfTestNoPark`
     *  proves the park guard can say `false` on a PLAIN socket; it says nothing
     *  about TLS, where a read can fail to park for a reason unique to the
     *  record layer — a whole record already sitting decrypted in the engine's
     *  buffer. Without this row, `parked=true` on `tlsRead` would rest on a
     *  check nobody has seen refuse a TLS read.
     *
     *  It PASSES by not parking. */
    static void tlsSelfTestNoPark() throws Exception {
        TlsPair p = new TlsPair();
        junk(p);
        p.accepted.getOutputStream().write(42);
        p.accepted.getOutputStream().flush();
        // Give the record time to arrive and be decrypted, so the read below
        // genuinely cannot block.
        Thread.sleep(300);
        check("tlsSelfTestNoPark", "must-not-park", false,
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

    /** THE ANTI-VACUITY ROW FOR THE RECORD LAYER.
     *
     *  `tlsRead` returning an exception proves a wakeup; it does not prove the
     *  stream was intact up to that point. This row proves the second thing,
     *  which is the property W7-53 said a close-aware TLS loop must not break.
     *  A multi-record payload is written by the peer and read back
     *  byte-for-byte: every byte delivered must be the right byte at the right
     *  offset. A loop that woke a reader mid-record and handed back a partial
     *  buffer would corrupt the tail here even while `tlsRead` stayed green —
     *  exactly the shape "removes a site from the census while leaving the
     *  defect" describes.
     *
     *  It does not park, by design (the peer has already written everything),
     *  so it is registered with `expectPark == false`. Its verdict is the
     *  payload comparison, carried in `outcome`: `CORRUPT@<offset>` names the
     *  first wrong byte. */
    static void tlsReadIntegrity() throws Exception {
        TlsPair p = new TlsPair();
        junk(p);
        // 200 KiB is many TLS records — the maximum plaintext fragment is
        // 16 KiB — so a reader that abandoned at a record boundary truncates
        // and one that abandoned INSIDE a record corrupts.
        final int n = 200 * 1024;
        byte[] payload = new byte[n];
        for (int i = 0; i < n; i++) {
            payload[i] = (byte) (i * 31 + 7);
        }
        Thread writer = new Thread(() -> {
            try {
                OutputStream out = p.accepted.getOutputStream();
                out.write(payload);
                out.flush();
            } catch (Throwable ignored) {
                // The reader's comparison is the verdict.
            }
        }, "asyncclose-tls-integrity-writer");
        writer.setDaemon(true);
        writer.start();
        check("tlsReadIntegrity", "returned:" + n, false,
                row -> {
                    InputStream in = p.client.getInputStream();
                    row.entered = true;
                    try {
                        byte[] got = new byte[n];
                        int off = 0;
                        while (off < n) {
                            int r = in.read(got, off, n - off);
                            if (r < 0) {
                                break;
                            }
                            off += r;
                        }
                        for (int i = 0; i < off; i++) {
                            if (got[i] != payload[i]) {
                                row.outcome = "CORRUPT@" + i;
                                return;
                            }
                        }
                        recordReturn(row, off);
                    } catch (Throwable t) {
                        record(row, t);
                    }
                },
                () -> p.client.close(),
                p::close);
    }

    /** `SSLSocket.getInputStream().read()` parked with nothing on the wire,
     *  then closed from another thread — `servlet.rs::s2_tls_read_direct`, or
     *  `t27_tls.rs::rustls_stream_read` for a rustls-backed client socket. */
    static void tlsRead() throws Exception {
        TlsPair p = new TlsPair();
        junk(p);
        check("tlsRead", "SocketException|SSLException|IOException",
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

    /** `SSLSocket.getOutputStream().write()` into a peer that never reads —
     *  `servlet.rs::s2_tls_write` / `t27_tls.rs::rustls_stream_write`. */
    static void tlsWrite() throws Exception {
        TlsPair p = new TlsPair();
        junk(p);
        byte[] payload = new byte[1 << 16];
        // `CLOSE_BLOCKED` is on this row's `want` because it is what the
        // ORACLE does, measured not assumed: on HotSpot 25.0.3 / Windows 11,
        // 2026-08-12, `SSLSocket.close()` issued from another thread while a
        // writer is parked inside `getOutputStream().write()` does not return —
        // JSSE serialises `duplexCloseOutput()` behind the write. There is
        // therefore NO reference behaviour in which a close wakes a parked TLS
        // write, which is the measured reason W7-61 leaves
        // `servlet.rs::s2_tls_write` and `t27_tls.rs::rustls_stream_write`
        // alone: a close-aware write loop here would not be parity with
        // HotSpot, it would be a behaviour HotSpot does not have.
        //
        // The row is still a real gate. It fails on exactly one reading —
        // `TIMEOUT`, i.e. the close DID return and the writer is still parked.
        // That is the genuine defect, and it is the reading a half-repair
        // (unregister the stream, wake nobody) would produce.
        check("tlsWrite", "CLOSE_BLOCKED|SocketException|SSLException|IOException|returned",
                row -> {
                    OutputStream out = p.client.getOutputStream();
                    row.entered = true;
                    try {
                        for (int i = 0; i < 256; i++) {
                            out.write(payload);
                            out.flush();
                        }
                        recordReturn(row, 256L * payload.length);
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
                // TLS rows (W7-61). `tlsSelfTestNoPark` first, for the same
                // reason `selfTestNoPark` is first: if the TLS park guard is
                // broken, no TLS row below it means anything.
                wrap("tlsSelfTestNoPark", AsyncCloseProbe::tlsSelfTestNoPark),
                wrap("tlsReadIntegrity", AsyncCloseProbe::tlsReadIntegrity),
                wrap("tlsRead", AsyncCloseProbe::tlsRead),
                wrap("tlsWrite", AsyncCloseProbe::tlsWrite),
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
