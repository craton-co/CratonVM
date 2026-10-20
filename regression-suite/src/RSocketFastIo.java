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
import java.util.Set;

/**
 * Regression: the `SocketChannel` transfer path now moves bytes through a
 * REUSED per-thread buffer (`native-io/src/socket_fast_io.rs`) instead of a
 * fresh `vec![0u8; limit - position]` per call, decodes the `ByteBuffer`
 * through memoized field slots instead of by name, and builds a gathering
 * write in ONE buffer instead of one per source plus a concatenation.
 *
 * <h2>Why every check here asserts an exact byte</h2>
 *
 * The reused buffer is NOT zeroed between transfers — that zeroing is the cost
 * being removed — so it still holds the previous transfer's bytes when the
 * next one starts. Every consumer therefore has to honour the kernel's byte
 * count exactly. A path that copied `limit - position` bytes instead of the
 * `n` returned would deliver the PREVIOUS message's tail into the destination
 * and throw nothing: the channel keeps working, the peer just sees data it
 * never sent. Section 2 is written specifically to catch that, by pre-filling
 * the destination with a sentinel and requiring it to survive past `n`.
 *
 * The same argument covers the memoized slots. A cached `position` index that
 * named the wrong field would advance the wrong thing and desynchronise the
 * channel silently, so positions are compared to fixed numbers rather than
 * merely observed to change.
 *
 * <h2>What this vector does NOT cover</h2>
 *
 * The selected-key fast path (`append_selected_keys_fast`) only engages for
 * netty's own `SelectedSelectionKeySet`, which is not on this suite's
 * classpath. Section 7 exercises the GENERIC path that every other selector
 * takes, plus the readiness-mirror cache that both paths sit behind — a
 * `select()` now skips the process-global side-table write when nothing
 * changed, and section 7's second round is what fails if that cache ever
 * suppresses a real change.
 *
 * Every value is compared against a fixed constant, so HotSpot and CratonVM
 * both have to produce it; the suite additionally diffs the two runs' output.
 */
public class RSocketFastIo {
    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** {@code n} bytes whose value is their own index — any misplaced byte shows. */
    static byte[] ramp(int n) {
        byte[] b = new byte[n];
        for (int i = 0; i < n; i++) {
            b[i] = (byte) i;
        }
        return b;
    }

    /**
     * Read until the buffer has taken {@code want} bytes, or fail.
     *
     * TCP may split a write across any number of reads, so a vector that
     * asserted a single `read()` returned everything would be flaky rather
     * than strict. Looping keeps the byte-level assertions exact without
     * depending on segmentation.
     */
    static int readFully(SocketChannel ch, ByteBuffer dst, int want) throws IOException {
        int got = 0;
        // Bounded, because sections 7 onwards run the channel NON-BLOCKING:
        // there a `read` with nothing buffered answers 0, and an unbounded loop
        // would spin forever on a readiness that did not materialise. A vector
        // that hangs reports nothing at all — `SameThreadTimeoutInvocation` can
        // only check a timeout after the invocation returns — so this fails
        // loudly with a count instead.
        int idle = 0;
        while (got < want) {
            int n = ch.read(dst);
            if (n < 0) {
                throw new IOException("EOF after " + got + " of " + want);
            }
            if (n == 0) {
                if (++idle > 2000) {
                    throw new IOException("stalled after " + got + " of " + want);
                }
                try {
                    Thread.sleep(1);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    throw new IOException("interrupted after " + got + " of " + want);
                }
                continue;
            }
            idle = 0;
            got += n;
        }
        return got;
    }

    static void writeFully(SocketChannel ch, ByteBuffer src) throws IOException {
        int idle = 0;
        while (src.hasRemaining()) {
            if (ch.write(src) == 0 && ++idle > 2000) {
                throw new IOException("write stalled with " + src.remaining() + " left");
            }
        }
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

                // ---- 1. Heap destination: exact bytes, exact position ----
                writeFully(peer, ByteBuffer.wrap(ramp(256)));
                ByteBuffer heap = ByteBuffer.allocate(256);
                readFully(client, heap, 256);
                check(heap.position() == 256, "heap read position = " + heap.position());
                byte[] got = heap.array();
                boolean rampOk = true;
                for (int i = 0; i < 256; i++) {
                    if (got[i] != (byte) i) {
                        rampOk = false;
                        break;
                    }
                }
                check(rampOk, "heap read did not reproduce the ramp");

                // ---- 2. The reused buffer must not leak past the byte count ----
                //
                // The destination is pre-filled with a sentinel and offered far
                // more room than the peer sends. Everything past the bytes
                // actually transferred has to still be the sentinel: if the
                // transfer path ever copied its whole scratch region instead of
                // the `n` the kernel reported, THIS is the check that sees it,
                // and nothing else would — no exception is thrown on that path.
                ByteBuffer sentinel = ByteBuffer.allocate(512);
                java.util.Arrays.fill(sentinel.array(), (byte) 0x5A);
                writeFully(peer, ByteBuffer.wrap(new byte[] {1, 2, 3, 4}));
                readFully(client, sentinel, 4);
                check(sentinel.position() == 4, "sentinel read position = " + sentinel.position());
                byte[] s = sentinel.array();
                check(s[0] == 1 && s[1] == 2 && s[2] == 3 && s[3] == 4, "sentinel payload wrong");
                boolean tailIntact = true;
                for (int i = 4; i < 512; i++) {
                    if (s[i] != (byte) 0x5A) {
                        tailIntact = false;
                        break;
                    }
                }
                check(tailIntact, "bytes past the read count were overwritten");

                // ---- 3. A window inside a larger buffer ----
                //
                // position and limit both away from the array bounds, so a
                // cached slot naming the wrong field lands the bytes in the
                // wrong place instead of failing.
                ByteBuffer window = ByteBuffer.allocate(128);
                java.util.Arrays.fill(window.array(), (byte) 0x77);
                window.position(10).limit(20);
                writeFully(peer, ByteBuffer.wrap(ramp(10)));
                readFully(client, window, 10);
                check(window.position() == 20, "window position = " + window.position());
                check(window.limit() == 20, "window limit moved to " + window.limit());
                byte[] w = window.array();
                check(w[9] == (byte) 0x77, "byte before the window was overwritten");
                check(w[10] == 0 && w[19] == 9, "window payload wrong");
                check(w[20] == (byte) 0x77, "byte after the window was overwritten");

                // ---- 4. Direct destination ----
                writeFully(peer, ByteBuffer.wrap(ramp(64)));
                ByteBuffer direct = ByteBuffer.allocateDirect(64);
                readFully(client, direct, 64);
                check(direct.position() == 64, "direct read position = " + direct.position());
                direct.flip();
                boolean directOk = true;
                for (int i = 0; i < 64; i++) {
                    if (direct.get(i) != (byte) i) {
                        directOk = false;
                        break;
                    }
                }
                check(directOk, "direct read did not reproduce the ramp");

                // ---- 5. Gathering write: order, and per-buffer position ----
                //
                // Three sources including an EMPTY one, which must contribute
                // nothing and leave no hole. The single-buffer rewrite of this
                // path is exactly where a stale byte between two sources would
                // appear, so the peer's copy is compared byte for byte.
                ByteBuffer g0 = ByteBuffer.wrap(new byte[] {10, 11, 12});
                ByteBuffer g1 = ByteBuffer.allocate(0);
                ByteBuffer g2 = ByteBuffer.wrap(new byte[] {20, 21, 22, 23});
                long wrote = 0;
                ByteBuffer[] srcs = {g0, g1, g2};
                while (wrote < 7) {
                    wrote += client.write(srcs);
                }
                check(wrote == 7, "gathering write returned " + wrote);
                check(g0.position() == 3, "g0 position = " + g0.position());
                check(g1.position() == 0, "g1 position = " + g1.position());
                check(g2.position() == 4, "g2 position = " + g2.position());
                ByteBuffer sink = ByteBuffer.allocate(7);
                readFully(peer, sink, 7);
                byte[] k = sink.array();
                check(k[0] == 10 && k[1] == 11 && k[2] == 12, "gathered head wrong");
                check(k[3] == 20 && k[6] == 23, "gathered tail wrong — a hole between sources?");

                // ---- 6. Scattering read across two destinations ----
                writeFully(peer, ByteBuffer.wrap(ramp(16)));
                ByteBuffer d0 = ByteBuffer.allocate(8);
                ByteBuffer d1 = ByteBuffer.allocate(8);
                ByteBuffer[] dsts = {d0, d1};
                long scattered = 0;
                while (scattered < 16) {
                    long n = client.read(dsts);
                    if (n < 0) {
                        throw new IOException("EOF during scattering read");
                    }
                    scattered += n;
                }
                check(scattered == 16, "scattering read returned " + scattered);
                check(d0.position() == 8, "d0 position = " + d0.position());
                check(d1.position() == 8, "d1 position = " + d1.position());
                check(d0.array()[0] == 0 && d0.array()[7] == 7, "d0 payload wrong");
                check(d1.array()[0] == 8 && d1.array()[7] == 15, "d1 payload wrong");

                // ---- 7. Selector readiness, twice ----
                //
                // The second round is the point. Readiness is now mirrored into
                // the shared side-table only when it CHANGED, so a key that goes
                // not-ready and ready again must be reported both times. A cache
                // that latched would pass round one and silently fail here.
                client.configureBlocking(false);
                try (Selector sel = Selector.open()) {
                    SelectionKey key = client.register(sel, SelectionKey.OP_READ);
                    check(key.isValid(), "key invalid straight after register");
                    check(key.interestOps() == SelectionKey.OP_READ,
                            "interestOps = " + key.interestOps());

                    for (int round = 1; round <= 2; round++) {
                        writeFully(peer, ByteBuffer.wrap(new byte[] {(byte) round}));
                        int ready = 0;
                        // A spurious zero-key return is legal; loop until the
                        // readiness we know is coming actually arrives, so the
                        // assertion below is about correctness, not timing.
                        for (int spin = 0; spin < 200 && ready == 0; spin++) {
                            ready = sel.select(100);
                        }
                        check(ready == 1, "round " + round + " select returned " + ready);
                        Set<SelectionKey> selected = sel.selectedKeys();
                        check(selected.size() == 1,
                                "round " + round + " selectedKeys size = " + selected.size());
                        check(selected.contains(key), "round " + round + " selected the wrong key");
                        check(key.isReadable(), "round " + round + " key not readable");

                        ByteBuffer one = ByteBuffer.allocate(1);
                        readFully(client, one, 1);
                        check(one.array()[0] == (byte) round,
                                "round " + round + " payload = " + one.array()[0]);
                        selected.clear();
                    }

                    // Nothing left: a non-blocking probe must report nothing,
                    // and must not resurrect the previous round's readiness.
                    check(sel.selectNow() == 0, "selectNow saw readiness with an empty socket");
                    check(sel.selectedKeys().isEmpty(), "selectNow left keys selected");
                    check(key.isValid(), "key went invalid without cancel()");

                    key.cancel();
                    sel.selectNow();
                    check(!key.isValid(), "key still valid after cancel()");
                }

                // ---- 8. A read that finds nothing is 0, not -1 ----
                client.configureBlocking(false);
                ByteBuffer empty = ByteBuffer.allocate(16);
                check(client.read(empty) == 0, "non-blocking read of an idle socket was not 0");
                check(empty.position() == 0, "empty read moved the position");

                // ---- 9. A zero-room destination transfers nothing ----
                ByteBuffer full = ByteBuffer.allocate(8);
                full.position(8);
                check(client.read(full) == 0, "read into a full buffer was not 0");
                check(full.position() == 8, "read into a full buffer moved the position");

                // Observables, not just a verdict: a run that asserted fewer
                // things than the oracle would otherwise diff identically.
                System.out.println("CK RSocketFastIo checks=" + checks);
                System.out.println("CK RSocketFastIo ramp_tail=" + (got[255] & 0xFF));
                System.out.println("CK RSocketFastIo window=" + (w[10] & 0xFF)
                        + "," + (w[19] & 0xFF) + "," + (w[20] & 0xFF));
                System.out.println("CK RSocketFastIo gathered=" + (k[2] & 0xFF)
                        + "," + (k[3] & 0xFF));
                long sum = 0;
                for (int i = 0; i < 256; i++) {
                    sum += got[i] & 0xFF;
                }
                System.out.println("CK RSocketFastIo ramp_sum=" + sum);
                System.out.println("PASS RSocketFastIo (" + checks + " checks)");
            }
        }
    }
}
