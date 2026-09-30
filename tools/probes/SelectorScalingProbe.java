// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.IOException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Iterator;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * F3's acceptance test, which is a CURVE rather than a delta.
 *
 * <h2>The claim under test</h2>
 *
 * `apply_ready_ops` used to push EVERY key of a selector into the
 * process-global `sk_table` after every `select()`, under that table's write
 * lock, recomputing an identity hash per key. The cost was therefore driven by
 * how many connections were REGISTERED, not by how many were ready — so a
 * server holding thousands of idle keep-alive connections paid for all of them
 * to report one ready socket.
 *
 * A delta between two arms cannot show that; the shape can. This probe holds
 * the REQUEST RATE fixed at one in flight and sweeps the number of registered
 * but permanently IDLE connections. The signature of the defect is a
 * round-trip time that rises with the idle count; the signature of the fix is
 * a flat line.
 *
 * <pre>
 *   CRATONVM_SEL_READY_CACHE=1  (default)  mirror only what changed — expect flat
 *   CRATONVM_SEL_READY_CACHE=0             mirror every key every tick — expect rising
 *   CRATONVM_SC_IO_STATS=1                 census: read `select ticks` and `keys walked`
 * </pre>
 *
 * The census is the arbiter of engagement, and it reports the mechanism
 * directly: `keys mirrored` against `keys walked` is the O(ready)/O(registered)
 * ratio, and `select ticks clean` counts the ticks that never took the global
 * lock at all. A timing curve without those rows is not attributable.
 *
 * <h2>Why the idle connections must be genuinely idle</h2>
 *
 * They are registered for OP_READ and never written to, so they are never
 * ready. That is exactly the keep-alive population an HTTP server carries, and
 * it is the population the old code paid for on every tick. Any traffic on them
 * would turn this into a throughput test and lose the isolation.
 */
public class SelectorScalingProbe {

    /** Idle registered connections to sweep. The axis the defect scales on. */
    static final int[] IDLE_COUNTS = {256};

    static final int WARMUP = 20;
    static final int ROUNDS = 3;
    static final int REQUESTS = 200;

    static long checksum = 0;

    /** Echo server on its own thread: select, read one byte, write it back. */
    static final class EchoServer implements Runnable {
        final Selector sel;
        final AtomicBoolean stop = new AtomicBoolean();
        volatile long ticks = 0;

        EchoServer(Selector sel) {
            this.sel = sel;
        }

        @Override
        public void run() {
            ByteBuffer buf = ByteBuffer.allocate(64);
            while (!stop.get()) {
                try {
                    // A short timeout rather than a blocking select: the probe
                    // has to be able to shut the loop down, and a spurious
                    // zero-key return costs a tick, which is what we want
                    // counted anyway.
                    if (sel.select(50) == 0) {
                        continue;
                    }
                    ticks++;
                    Iterator<SelectionKey> it = sel.selectedKeys().iterator();
                    while (it.hasNext()) {
                        SelectionKey k = it.next();
                        it.remove();
                        if (!k.isValid() || !k.isReadable()) {
                            continue;
                        }
                        SocketChannel ch = (SocketChannel) k.channel();
                        buf.clear();
                        int n = ch.read(buf);
                        if (n > 0) {
                            buf.flip();
                            while (buf.hasRemaining()) {
                                ch.write(buf);
                            }
                        }
                    }
                } catch (IOException e) {
                    // A closed channel during teardown is expected.
                }
            }
        }
    }

    /** ns per request round trip, with `idle` idle connections registered. */
    static double round(SocketChannel active, int requests) throws IOException {
        ByteBuffer out = ByteBuffer.allocate(1);
        ByteBuffer in = ByteBuffer.allocate(1);
        long start = System.nanoTime();
        for (int i = 0; i < requests; i++) {
            out.clear();
            out.put(0, (byte) i);
            out.position(0).limit(1);
            while (out.hasRemaining()) {
                active.write(out);
            }
            in.clear();
            int got = 0;
            while (got < 1) {
                int n = active.read(in);
                if (n < 0) {
                    throw new IOException("server closed");
                }
                got += n;
            }
            checksum += in.get(0);
        }
        return (double) (System.nanoTime() - start) / requests;
    }

    public static void main(String[] args) throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();
        System.out.println("requests=" + REQUESTS + " rounds=" + ROUNDS);

        // TWO full sweeps; only the second is reported. Rows run in one JVM,
        // so an unwarmed first sweep leaves every later row warmer than the
        // one before it — which on HotSpot produced a 146 -> 94 us DECREASE
        // across the sweep and would read as "the curve is flat, or better
        // than flat" on a VM where it is not. The discarded sweep is the fix.
        for (int sweep = 0; sweep < 1; sweep++) {
        boolean report = true;
        for (int idle : IDLE_COUNTS) {
            try (ServerSocketChannel server = ServerSocketChannel.open();
                    Selector sel = Selector.open()) {
                server.bind(new InetSocketAddress(lo, 0), 4096);
                int port = ((InetSocketAddress) server.getLocalAddress()).getPort();

                List<SocketChannel> clients = new ArrayList<>();
                List<SocketChannel> served = new ArrayList<>();
                // One active connection plus `idle` that never send anything.
                for (int i = 0; i <= idle; i++) {
                    if (i % 32 == 0) {
                        System.err.println("[probe] connecting " + i + "/" + idle);
                    }
                    SocketChannel c = SocketChannel.open(new InetSocketAddress(lo, port));
                    c.configureBlocking(true);
                    clients.add(c);
                    SocketChannel s = server.accept();
                    s.configureBlocking(false);
                    s.register(sel, SelectionKey.OP_READ);
                    served.add(s);
                }

                System.err.println("[probe] setup complete: " + clients.size()
                        + " connections established and registered");
                EchoServer echo = new EchoServer(sel);
                Thread t = new Thread(echo, "echo-" + idle);
                t.setDaemon(true);
                t.start();

                SocketChannel active = clients.get(0);
                round(active, WARMUP);

                double[] samples = new double[ROUNDS];
                for (int r = 0; r < ROUNDS; r++) {
                    samples[r] = round(active, REQUESTS);
                }
                Arrays.sort(samples);
                if (report) {
                    System.out.printf("idle=%5d  registered=%5d  median=%9.1f ns/request  ticks=%d%n",
                            idle, idle + 1, samples[ROUNDS / 2], echo.ticks);
                }

                echo.stop.set(true);
                sel.wakeup();
                t.join(2000);
                for (SocketChannel c : clients) {
                    try {
                        c.close();
                    } catch (IOException ignored) {
                        // teardown
                    }
                }
                for (SocketChannel s : served) {
                    try {
                        s.close();
                    } catch (IOException ignored) {
                        // teardown
                    }
                }
            }
        }
        }
        System.out.println("checksum=" + checksum);
    }
}
