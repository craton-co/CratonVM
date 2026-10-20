// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.IOException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.AsynchronousFileChannel;
import java.nio.channels.AsynchronousServerSocketChannel;
import java.nio.channels.AsynchronousSocketChannel;
import java.nio.channels.CompletionHandler;
import java.nio.file.FileStore;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;

/**
 * SBR wave 6, lane {@code asyncnet}: cost of the asynchronous channels the
 * retirement moves from native carriers to JDK bytecode.
 *
 * <pre>
 *   java SbrW6AsyncnetPerf [rounds] [mib] [pings]
 * </pre>
 *
 * Per round, each after its own warm-up pass:
 * <ul>
 *   <li>{@code rtt}: microseconds per 1-byte ping-pong over a connected async
 *       pair (Future form, {@code get()} on each step);</li>
 *   <li>{@code rtt.h}: the same driven by CompletionHandlers;</li>
 *   <li>{@code tput}: MiB/s, {@code mib} MiB through 64 KiB buffers;</li>
 *   <li>{@code conn}: microseconds per connect + accept + close of the pair;</li>
 *   <li>{@code afc}: microseconds per 4 KiB positional write + read pair;</li>
 *   <li>{@code stores}: microseconds per {@code getFileStores()} walk.</li>
 * </ul>
 */
public class SbrW6AsyncnetPerf {
    static final InetAddress LO = InetAddress.getLoopbackAddress();
    static final long T = 30;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        int mib = args.length > 1 ? Integer.parseInt(args[1]) : 32;
        int pings = args.length > 2 ? Integer.parseInt(args[2]) : 3000;
        rtt(pings / 5);
        rttH(pings / 5);
        tput(Math.max(1, mib / 8));
        conn(50);
        afc(300);
        stores(20);
        for (int r = 0; r < rounds; r++) {
            double a = rtt(pings);
            double b = rttH(pings);
            double c = tput(mib);
            double d = conn(200);
            double e = afc(2000);
            double f = stores(100);
            System.out.printf("round %d rtt=%.1f us rtt.h=%.1f us tput=%.1f MiB/s conn=%.1f us afc=%.1f us stores=%.1f us%n",
                    r, a, b, c, d, e, f);
        }
    }

    static AsynchronousSocketChannel[] pair(AsynchronousServerSocketChannel s) throws Exception {
        // Handler-form accept and connect: the only forms the pre-wave-6 native carrier implements.
        AsynchronousSocketChannel[] got = new AsynchronousSocketChannel[1];
        CountDownLatch accepted = new CountDownLatch(1);
        s.accept(null, new CompletionHandler<AsynchronousSocketChannel, Object>() {
            public void completed(AsynchronousSocketChannel ch, Object x) {
                got[0] = ch;
                accepted.countDown();
            }

            public void failed(Throwable t, Object x) {
                accepted.countDown();
            }
        });
        AsynchronousSocketChannel c = AsynchronousSocketChannel.open();
        c.connect(s.getLocalAddress()).get(T, TimeUnit.SECONDS);
        accepted.await(T, TimeUnit.SECONDS);
        return new AsynchronousSocketChannel[] {c, got[0]};
    }

    static double rtt(int pings) throws Exception {
        try (AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()
                .bind(new InetSocketAddress(LO, 0))) {
            AsynchronousSocketChannel[] p = pair(s);
            try (AsynchronousSocketChannel c = p[0]; AsynchronousSocketChannel a = p[1]) {
                Thread echo = new Thread(() -> {
                    try {
                        ByteBuffer b = ByteBuffer.allocate(1);
                        for (int i = 0; i < pings; i++) {
                            b.clear();
                            a.read(b).get(T, TimeUnit.SECONDS);
                            b.flip();
                            a.write(b).get(T, TimeUnit.SECONDS);
                        }
                    } catch (Exception e) {
                        e.printStackTrace();
                    }
                });
                echo.start();
                ByteBuffer out = ByteBuffer.allocate(1);
                ByteBuffer in = ByteBuffer.allocate(1);
                long t0 = System.nanoTime();
                for (int i = 0; i < pings; i++) {
                    out.clear();
                    out.put((byte) 1).flip();
                    c.write(out).get(T, TimeUnit.SECONDS);
                    in.clear();
                    c.read(in).get(T, TimeUnit.SECONDS);
                }
                long t1 = System.nanoTime();
                echo.join();
                return (t1 - t0) / 1000.0 / pings;
            }
        }
    }

    /** Server side: read one byte, write it back, repeat. */
    static final class Echo {
        final AsynchronousSocketChannel ch;
        final ByteBuffer buf = ByteBuffer.allocate(1);
        final CompletionHandler<Integer, Object> onWrite = new CompletionHandler<>() {
            public void completed(Integer v, Object x) {
                next();
            }

            public void failed(Throwable t, Object x) {
            }
        };
        final CompletionHandler<Integer, Object> onRead = new CompletionHandler<>() {
            public void completed(Integer v, Object x) {
                buf.flip();
                ch.write(buf, null, onWrite);
            }

            public void failed(Throwable t, Object x) {
            }
        };

        Echo(AsynchronousSocketChannel ch) {
            this.ch = ch;
        }

        void next() {
            buf.clear();
            ch.read(buf, null, onRead);
        }
    }

    /** Client side: write one byte, read one byte, count down. */
    static final class Ping {
        final AsynchronousSocketChannel ch;
        final ByteBuffer buf = ByteBuffer.allocate(1);
        final CountDownLatch done = new CountDownLatch(1);
        int left;
        final CompletionHandler<Integer, Object> onRead = new CompletionHandler<>() {
            public void completed(Integer v, Object x) {
                if (--left == 0) {
                    done.countDown();
                } else {
                    send();
                }
            }

            public void failed(Throwable t, Object x) {
                done.countDown();
            }
        };
        final CompletionHandler<Integer, Object> onWrite = new CompletionHandler<>() {
            public void completed(Integer v, Object x) {
                buf.clear();
                ch.read(buf, null, onRead);
            }

            public void failed(Throwable t, Object x) {
                done.countDown();
            }
        };

        Ping(AsynchronousSocketChannel ch, int n) {
            this.ch = ch;
            this.left = n;
        }

        void send() {
            buf.clear();
            buf.put((byte) 1).flip();
            ch.write(buf, null, onWrite);
        }
    }

    static double rttH(int pings) throws Exception {
        try (AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()
                .bind(new InetSocketAddress(LO, 0))) {
            AsynchronousSocketChannel[] p = pair(s);
            try (AsynchronousSocketChannel c = p[0]; AsynchronousSocketChannel a = p[1]) {
                new Echo(a).next();
                Ping ping = new Ping(c, pings);
                long t0 = System.nanoTime();
                ping.send();
                ping.done.await(60, TimeUnit.SECONDS);
                long t1 = System.nanoTime();
                return (t1 - t0) / 1000.0 / pings;
            }
        }
    }

    static double tput(int mib) throws Exception {
        try (AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()
                .bind(new InetSocketAddress(LO, 0))) {
            AsynchronousSocketChannel[] p = pair(s);
            try (AsynchronousSocketChannel c = p[0]; AsynchronousSocketChannel a = p[1]) {
                long total = (long) mib << 20;
                Thread reader = new Thread(() -> {
                    try {
                        ByteBuffer b = ByteBuffer.allocate(65536);
                        long n = 0;
                        while (n < total) {
                            b.clear();
                            int r = a.read(b).get(T, TimeUnit.SECONDS);
                            if (r < 0) break;
                            n += r;
                        }
                    } catch (Exception e) {
                        e.printStackTrace();
                    }
                });
                reader.start();
                ByteBuffer b = ByteBuffer.allocate(65536);
                long t0 = System.nanoTime();
                long sent = 0;
                while (sent < total) {
                    b.clear();
                    sent += c.write(b).get(T, TimeUnit.SECONDS);
                }
                reader.join();
                long t1 = System.nanoTime();
                return mib / ((t1 - t0) / 1e9);
            }
        }
    }

    static double conn(int n) throws Exception {
        try (AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()
                .bind(new InetSocketAddress(LO, 0))) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                AsynchronousSocketChannel[] p = pair(s);
                p[0].close();
                p[1].close();
            }
            long t1 = System.nanoTime();
            return (t1 - t0) / 1000.0 / n;
        }
    }

    static double afc(int n) throws Exception {
        Path d = Files.createTempDirectory("sbrw6perf");
        try {
            Path f = Files.write(d.resolve("p.bin"), new byte[64 * 4096]);
            try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(f, StandardOpenOption.READ,
                    StandardOpenOption.WRITE)) {
                ByteBuffer w = ByteBuffer.allocate(4096);
                ByteBuffer r = ByteBuffer.allocate(4096);
                long t0 = System.nanoTime();
                for (int i = 0; i < n; i++) {
                    w.clear();
                    ch.write(w, (i % 64) * 4096L).get(T, TimeUnit.SECONDS);
                    r.clear();
                    ch.read(r, (i % 64) * 4096L).get(T, TimeUnit.SECONDS);
                }
                long t1 = System.nanoTime();
                return (t1 - t0) / 1000.0 / n;
            }
        } finally {
            try (var st = Files.walk(d)) {
                st.sorted(java.util.Comparator.reverseOrder()).forEach(p -> {
                    try {
                        Files.deleteIfExists(p);
                    } catch (IOException ignored) {
                    }
                });
            }
        }
    }

    static double stores(int n) {
        long t0 = System.nanoTime();
        int k = 0;
        for (int i = 0; i < n; i++) {
            for (FileStore fs : FileSystems.getDefault().getFileStores()) {
                k += fs.name().length();
            }
        }
        long t1 = System.nanoTime();
        if (k < 0) System.out.println(k);
        return (t1 - t0) / 1000.0 / n;
    }
}
