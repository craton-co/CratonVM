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
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Iterator;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.zip.CRC32;

/**
 * gci i1/pn (2026-10-11): a loopback NIO echo server and blocking clients, the
 * nearest local stand-in for the netty buffer matrix and the Tomcat fixture's
 * NIO connector (the real workloads of the pinned young copy's flip gate,
 * {@code docs/known-issues/gc/gengc-r4w6-pinstale6-pinned-copy-default-flip-gate-20260924.md},
 * gate R2; neither is available on the probe host).
 *
 * <p>Shape. One server thread runs a {@link Selector} over a non-blocking
 * {@link ServerSocketChannel} on 127.0.0.1; every connection gets a DIRECT
 * read buffer and a queue of pending HEAP write buffers (netty's two buffer
 * kinds). {@code clients} threads each open a blocking {@link SocketChannel}
 * and send {@code msgs} messages whose sizes and bytes come from a per-client
 * xorshift seed; every echo is read back into a direct buffer and compared
 * byte for byte. Each client keeps its last 64 payloads alive (medium-lived
 * young data that a young collection carries, promotes or pins) and builds a
 * small object graph per message (short-lived churn). The server's and the
 * clients' loops are JIT-warm after a few thousand messages, and every pause
 * finds threads parked in compiled code and in native socket calls.
 *
 * <p>stdout (deterministic; HotSpot 25 {@code -XX:+UseSerialGC} is the
 * oracle): {@code PASS nio-echo clients=C msgs=M bytes=B bad=0 kept=K checksum=X},
 * or {@code FAIL ...} with the same fields. {@code checksum} is the sum of the
 * clients' CRCs over every echoed byte, so the thread schedule cannot change
 * it. Timings go to stderr.
 *
 * <p>Run: {@code cratonvm -XX:+UseGenerationalGC -Xmx64m -cp <classes>
 * GciPn1NioEchoProbe 4 20000}.
 */
public class GciPn1NioEchoProbe {
    static final int MAX_MSG = 6000;

    /** Per-connection server state: the direct read buffer and the pending echoes. */
    static final class Conn {
        final ByteBuffer in = ByteBuffer.allocateDirect(8192);
        final ArrayDeque<ByteBuffer> pending = new ArrayDeque<>();
    }

    /** Short-lived churn node built per message. */
    static final class Node {
        final int v;
        final Node next;
        final byte[] pad;

        Node(int v, Node next) {
            this.v = v;
            this.next = next;
            this.pad = new byte[16 + (v & 63)];
        }
    }

    static int xorshift(int x) {
        x ^= x << 13;
        x ^= x >>> 17;
        x ^= x << 5;
        return x;
    }

    static void serve(Selector sel, ServerSocketChannel ss, int expected, AtomicInteger closed) throws IOException {
        while (closed.get() < expected) {
            sel.select(200);
            Iterator<SelectionKey> it = sel.selectedKeys().iterator();
            while (it.hasNext()) {
                SelectionKey k = it.next();
                it.remove();
                if (!k.isValid()) {
                    continue;
                }
                if (k.isAcceptable()) {
                    SocketChannel c = ss.accept();
                    if (c != null) {
                        c.configureBlocking(false);
                        c.register(sel, SelectionKey.OP_READ, new Conn());
                    }
                    continue;
                }
                SocketChannel c = (SocketChannel) k.channel();
                Conn conn = (Conn) k.attachment();
                if (k.isReadable()) {
                    conn.in.clear();
                    int r = c.read(conn.in);
                    if (r < 0) {
                        k.cancel();
                        c.close();
                        closed.incrementAndGet();
                        continue;
                    }
                    if (r > 0) {
                        conn.in.flip();
                        byte[] copy = new byte[conn.in.remaining()];
                        conn.in.get(copy);
                        conn.pending.add(ByteBuffer.wrap(copy));
                    }
                }
                while (!conn.pending.isEmpty()) {
                    ByteBuffer out = conn.pending.peek();
                    c.write(out);
                    if (out.hasRemaining()) {
                        break;
                    }
                    conn.pending.poll();
                }
                if (k.isValid()) {
                    k.interestOps(conn.pending.isEmpty()
                            ? SelectionKey.OP_READ
                            : SelectionKey.OP_READ | SelectionKey.OP_WRITE);
                }
            }
        }
    }

    public static void main(String[] args) throws Exception {
        int clients = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int msgs = args.length > 1 ? Integer.parseInt(args[1]) : 20000;

        Selector sel = Selector.open();
        ServerSocketChannel ss = ServerSocketChannel.open();
        ss.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
        ss.configureBlocking(false);
        ss.register(sel, SelectionKey.OP_ACCEPT);
        int port = ((InetSocketAddress) ss.getLocalAddress()).getPort();

        AtomicInteger closed = new AtomicInteger();
        Throwable[] serverFailure = new Throwable[1];
        Thread server = new Thread(() -> {
            try {
                serve(sel, ss, clients, closed);
            } catch (Throwable t) {
                serverFailure[0] = t;
            }
        }, "nio-echo-server");
        server.setDaemon(true);
        server.start();

        long t0 = System.nanoTime();
        AtomicLong checksum = new AtomicLong();
        AtomicLong bytes = new AtomicLong();
        AtomicLong bad = new AtomicLong();
        AtomicLong kept = new AtomicLong();
        Throwable[] clientFailure = new Throwable[1];
        List<Thread> threads = new ArrayList<>();
        for (int c = 0; c < clients; c++) {
            final int id = c;
            Thread t = new Thread(() -> {
                try (SocketChannel ch = SocketChannel.open(
                        new InetSocketAddress(InetAddress.getLoopbackAddress(), port))) {
                    ByteBuffer back = ByteBuffer.allocateDirect(MAX_MSG);
                    ArrayDeque<byte[]> window = new ArrayDeque<>();
                    CRC32 crc = new CRC32();
                    int seed = 0x9E3779B9 ^ (id * 0x45d9f3b) ^ 1;
                    long myBytes = 0;
                    long myBad = 0;
                    for (int m = 0; m < msgs; m++) {
                        seed = xorshift(seed);
                        int len = 1 + ((seed >>> 1) % MAX_MSG);
                        byte[] payload = new byte[len];
                        int s = seed;
                        for (int i = 0; i < len; i++) {
                            s = xorshift(s);
                            payload[i] = (byte) s;
                        }
                        Node chain = null;
                        for (int i = 0; i < 8; i++) {
                            chain = new Node(s + i, chain);
                        }
                        ByteBuffer out = ByteBuffer.wrap(payload);
                        while (out.hasRemaining()) {
                            ch.write(out);
                        }
                        back.clear();
                        back.limit(len);
                        while (back.hasRemaining()) {
                            if (ch.read(back) < 0) {
                                throw new IOException("server closed early");
                            }
                        }
                        back.flip();
                        for (int i = 0; i < len; i++) {
                            byte b = back.get(i);
                            if (b != payload[i]) {
                                myBad++;
                                break;
                            }
                        }
                        crc.update(payload, 0, len);
                        myBytes += len;
                        window.addLast(payload);
                        if (window.size() > 64) {
                            window.removeFirst();
                        }
                        if (chain.v != s + 7) {
                            myBad++;
                        }
                    }
                    // The kept window must still read its own bytes.
                    long k = 0;
                    for (byte[] p : window) {
                        int t2 = 0;
                        for (byte b : p) {
                            t2 += b;
                        }
                        k += p.length + (t2 & 1);
                    }
                    kept.addAndGet(k);
                    checksum.addAndGet(crc.getValue());
                    bytes.addAndGet(myBytes);
                    bad.addAndGet(myBad);
                } catch (Throwable e) {
                    synchronized (clientFailure) {
                        clientFailure[0] = e;
                    }
                }
            }, "nio-echo-client-" + c);
            threads.add(t);
            t.start();
        }
        for (Thread t : threads) {
            t.join();
        }
        server.join(30_000);
        System.err.println("[nio-echo] ms=" + (System.nanoTime() - t0) / 1_000_000);
        String line = "nio-echo clients=" + clients + " msgs=" + msgs + " bytes=" + bytes.get()
                + " bad=" + bad.get() + " kept=" + kept.get() + " checksum=" + checksum.get();
        if (clientFailure[0] != null || serverFailure[0] != null) {
            System.out.println("FAIL " + line + " client=" + (clientFailure[0] == null ? "ok" : clientFailure[0].getClass().getName())
                    + " server=" + (serverFailure[0] == null ? "ok" : serverFailure[0].getClass().getName()));
            if (clientFailure[0] != null) {
                clientFailure[0].printStackTrace();
            }
            if (serverFailure[0] != null) {
                serverFailure[0].printStackTrace();
            }
            System.exit(1);
        }
        System.out.println((bad.get() == 0 ? "PASS " : "FAIL ") + line);
        ss.close();
        sel.close();
    }
}
