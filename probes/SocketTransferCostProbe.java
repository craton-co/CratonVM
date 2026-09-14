// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.IOException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.StandardSocketOptions;
import java.nio.ByteBuffer;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.util.Arrays;

/**
 * Prices the CAPACITY-DEPENDENT part of a `SocketChannel.read`, using a paired
 * measurement so the loopback round trip cancels out.
 *
 * <h2>What is being measured, and why the obvious probe cannot measure it</h2>
 *
 * The cost is per-CALL and scales with the destination buffer's REMAINING
 * CAPACITY, not with the bytes moved: `sc_read` used to do
 * `vec![0u8; limit - position]` before asking the kernel how many bytes were
 * waiting, so a reactor reading a 128-byte request into a 64 KiB receive
 * buffer paid to allocate and zero 64 KiB.
 *
 * The naive probe — time a read at each capacity, compare rows — does not
 * work here. One loopback write+read pair costs ~65 µs on this host, the
 * capacity-dependent term is single-digit µs, and the run-to-run spread
 * between rows that SHOULD be identical was ~15%. The signal sits inside the
 * noise.
 *
 * So the two capacities are interleaved INSIDE ONE LOOP, alternating on every
 * iteration, with a separate accumulator each. Both arms then see the same
 * scheduler, the same cache state and the same host load, and the quantity
 * reported is their DIFFERENCE:
 *
 * <pre>
 *   delta = ns(capacity 64 KiB) - ns(capacity 128B)     payload fixed at 128B
 * </pre>
 *
 * Common-mode noise cancels. The round trip, the syscalls, the TCP stack and
 * the interpreter's own per-call cost are all identical between the two halves
 * and subtract out; what survives is the work that scales with the offered
 * capacity, which is precisely the defect.
 *
 * <h2>What each outcome means</h2>
 *
 * <pre>
 *   delta >> 0   cost scales with the buffer the application OFFERED — the defect
 *   delta ~= 0   cost scales with the traffic it GOT — the fixed state
 * </pre>
 *
 * HotSpot is the control and must report delta ~= 0; if it does not, the
 * instrument is measuring the host and no CratonVM number from it means
 * anything. That check comes first, before either CratonVM arm is read.
 *
 * <h2>Arms</h2>
 *
 * <pre>
 *   CRATONVM_SC_SCRATCH=1   (default)  reusable buffer: expect delta ~= 0
 *   CRATONVM_SC_SCRATCH=0              per-call allocation: expect delta >> 0
 *   CRATONVM_SC_IO_STATS=1             engagement census — read it FIRST
 * </pre>
 *
 * `scratch hit=0` means the path never engaged and the timing is measuring
 * something else, which is the one reading that invalidates the rest.
 */
public class SocketTransferCostProbe {

    /** Bytes moved per read. Constant across both halves of the pair. */
    static final int PAYLOAD = 128;

    /** The two offered capacities. Only these differ between the halves. */
    static final int SMALL_CAP = 128;
    static final int LARGE_CAP = 64 * 1024;

    static final int WARMUP = 3_000;
    static final int ITERS = 30_000;
    static final int ROUNDS = 3;

    /** A "fast" arm that moved the wrong bytes must not look fast. */
    static long checksum = 0;

    static void writeFully(SocketChannel ch, ByteBuffer src) throws IOException {
        while (src.hasRemaining()) {
            ch.write(src);
        }
    }

    static void readFully(SocketChannel ch, ByteBuffer dst, int want) throws IOException {
        int got = 0;
        while (got < want) {
            int n = ch.read(dst);
            if (n < 0) {
                throw new IOException("EOF after " + got);
            }
            got += n;
        }
    }

    /** One transfer: `payload` bytes into a destination offering `dst`'s capacity. */
    static void transfer(SocketChannel client, SocketChannel peer, ByteBuffer src, ByteBuffer dst)
            throws IOException {
        src.clear();
        writeFully(peer, src);
        // `clear()` makes `limit - position` the FULL capacity — the shape a
        // reactor presents, and what the old code sized its allocation from.
        dst.clear();
        readFully(client, dst, PAYLOAD);
        checksum += dst.get(0) + dst.get(PAYLOAD - 1);
    }

    /**
     * Interleaved pair. Returns {ns per small-capacity transfer, ns per
     * large-capacity transfer}.
     */
    static double[] pairedRound(SocketChannel client, SocketChannel peer, boolean direct, int iters)
            throws IOException {
        byte[] payload = new byte[PAYLOAD];
        for (int i = 0; i < PAYLOAD; i++) {
            payload[i] = (byte) i;
        }
        ByteBuffer src = ByteBuffer.wrap(payload);
        ByteBuffer small = direct ? ByteBuffer.allocateDirect(SMALL_CAP) : ByteBuffer.allocate(SMALL_CAP);
        ByteBuffer large = direct ? ByteBuffer.allocateDirect(LARGE_CAP) : ByteBuffer.allocate(LARGE_CAP);

        long smallNs = 0;
        long largeNs = 0;
        for (int i = 0; i < iters; i++) {
            // Alternate on every iteration, not in blocks: a block layout lets
            // a load excursion land entirely on one half and masquerade as the
            // effect being measured.
            long t0 = System.nanoTime();
            transfer(client, peer, src, small);
            long t1 = System.nanoTime();
            transfer(client, peer, src, large);
            long t2 = System.nanoTime();
            smallNs += t1 - t0;
            largeNs += t2 - t1;
        }
        return new double[] {(double) smallNs / iters, (double) largeNs / iters};
    }

    public static void main(String[] args) throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();
        try (ServerSocketChannel server = ServerSocketChannel.open()) {
            server.bind(new InetSocketAddress(lo, 0));
            int port = ((InetSocketAddress) server.getLocalAddress()).getPort();
            try (SocketChannel client = SocketChannel.open(new InetSocketAddress(lo, port));
                    SocketChannel peer = server.accept()) {
                client.configureBlocking(true);
                peer.configureBlocking(true);
                // Nagle would add a delay that varies with message timing and
                // would land unevenly across the interleave.
                try {
                    client.setOption(StandardSocketOptions.TCP_NODELAY, Boolean.TRUE);
                    peer.setOption(StandardSocketOptions.TCP_NODELAY, Boolean.TRUE);
                } catch (Exception ignored) {
                    // Not fatal: it raises the floor for both halves equally.
                }

                // Warm up at the LARGE capacity so the reusable buffer is
                // already at its high-water mark — timing its growth is timing
                // startup, not steady state.
                pairedRound(client, peer, false, WARMUP);

                System.out.println("payload=" + PAYLOAD + "B small=" + SMALL_CAP
                        + "B large=" + LARGE_CAP + "B iters=" + ITERS + " rounds=" + ROUNDS);
                for (boolean direct : new boolean[] {false, true}) {
                    double[] deltas = new double[ROUNDS];
                    for (int r = 0; r < ROUNDS; r++) {
                        double[] p = pairedRound(client, peer, direct, ITERS);
                        deltas[r] = p[1] - p[0];
                        System.out.printf("%s round=%d  small=%8.1f  large=%8.1f  delta=%+8.1f ns%n",
                                direct ? "direct" : "heap  ", r + 1, p[0], p[1], deltas[r]);
                    }
                    Arrays.sort(deltas);
                    System.out.printf("%s MEDIAN DELTA %+.1f ns per read%n",
                            direct ? "direct" : "heap  ", deltas[ROUNDS / 2]);
                }
                System.out.println("checksum=" + checksum);
            }
        }
    }
}
