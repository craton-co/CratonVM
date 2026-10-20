import java.io.IOException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.SocketAddress;
import java.net.StandardSocketOptions;
import java.nio.ByteBuffer;
import java.nio.channels.AcceptPendingException;
import java.nio.channels.AlreadyBoundException;
import java.nio.channels.AlreadyConnectedException;
import java.nio.channels.AsynchronousChannelGroup;
import java.nio.channels.AsynchronousCloseException;
import java.nio.channels.AsynchronousFileChannel;
import java.nio.channels.AsynchronousServerSocketChannel;
import java.nio.channels.AsynchronousSocketChannel;
import java.nio.channels.ClosedChannelException;
import java.nio.channels.CompletionHandler;
import java.nio.channels.ConnectionPendingException;
import java.nio.channels.FileChannel;
import java.nio.channels.FileLock;
import java.nio.channels.InterruptedByTimeoutException;
import java.nio.channels.NonReadableChannelException;
import java.nio.channels.NonWritableChannelException;
import java.nio.channels.NotYetBoundException;
import java.nio.channels.NotYetConnectedException;
import java.nio.channels.OverlappingFileLockException;
import java.nio.channels.ReadPendingException;
import java.nio.channels.ShutdownChannelGroupException;
import java.nio.charset.StandardCharsets;
import java.nio.file.FileStore;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.Set;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

/**
 * SBR wave 6, lane {@code asyncnet}: the asynchronous half of NIO -- async socket and server
 * channels, channel groups, {@code AsynchronousFileChannel}, {@code FileLock}, the fs provider's
 * {@code newAsynchronousFileChannel}, {@code FileSystem.getFileStores} -- over the real
 * {@code sun.nio.ch} layer.
 *
 * <p>Each row returns deterministic observations only (no ports, no identity hashes). It is
 * evaluated ONCE and printed, then re-evaluated in a hot loop and compared with the first value;
 * {@code drift} counts mismatches. Diff the whole output against HotSpot 25.
 *
 * <pre>
 *   java SbrW6Asyncnet [iters]              (HotSpot oracle)
 *   cratonvm --jdk-only -cp . SbrW6Asyncnet [iters]
 * </pre>
 */
public class SbrW6Asyncnet {
    interface S {
        String get() throws Exception;
    }

    static final List<String> LABELS = new ArrayList<>();
    static final List<S> ROWS = new ArrayList<>();

    static void row(String label, S s) {
        LABELS.add(label);
        ROWS.add(s);
    }

    static final String TIMEOUT = "!TIMEOUT (row did not finish in 30 s)";

    static String safe(S s) {
        final String[] out = {TIMEOUT};
        Thread t = new Thread(() -> {
            String v;
            try {
                v = s.get();
            } catch (Throwable e) {
                v = "!" + e.getClass().getName() + ": " + e.getMessage();
            }
            out[0] = v;
        });
        t.setDaemon(true);
        t.start();
        try {
            t.join(30_000);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        return t.isAlive() ? TIMEOUT : out[0];
    }

    static String ex(S s) {
        try {
            return "ok:" + shape(s.get());
        } catch (ExecutionException e) {
            return "ee:" + e.getCause().getClass().getName();
        } catch (Throwable t) {
            return "ex:" + t.getClass().getName();
        }
    }

    static final InetAddress LO = InetAddress.getLoopbackAddress();
    static final long T = 10;

    static ByteBuffer bb(String s) {
        return ByteBuffer.wrap(s.getBytes(StandardCharsets.US_ASCII));
    }

    static String str(ByteBuffer b) {
        b.flip();
        byte[] a = new byte[b.remaining()];
        b.get(a);
        return new String(a, StandardCharsets.US_ASCII);
    }

    static String shape(Object o) {
        String s = String.valueOf(o).replaceAll("@[0-9a-f]+", "@H");
        return s.replaceAll("[0-9]+", "N").replaceAll("localhost|127\\.N\\.N\\.N|/N\\.N\\.N\\.N", "A");
    }

    static String cls(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    static int readN(AsynchronousSocketChannel a, ByteBuffer dst) throws Exception {
        int total = 0;
        while (dst.hasRemaining()) {
            int n = a.read(dst).get(T, TimeUnit.SECONDS);
            if (n < 0) return total == 0 ? -1 : total;
            total += n;
        }
        return total;
    }

    /** A connected pair: [client, accepted] over an ephemeral loopback server. */
    static AsynchronousSocketChannel[] pair(AsynchronousServerSocketChannel s) throws Exception {
        Future<AsynchronousSocketChannel> acc = s.accept();
        AsynchronousSocketChannel c = AsynchronousSocketChannel.open();
        c.connect(s.getLocalAddress()).get(T, TimeUnit.SECONDS);
        return new AsynchronousSocketChannel[] {c, acc.get(T, TimeUnit.SECONDS)};
    }

    static AsynchronousServerSocketChannel server() throws Exception {
        return AsynchronousServerSocketChannel.open().bind(new InetSocketAddress(LO, 0));
    }

    static Path tmp() throws IOException {
        return Files.createTempDirectory("sbrw6");
    }

    static void rm(Path d) {
        try (var s = Files.walk(d)) {
            s.sorted(Comparator.reverseOrder()).forEach(p -> {
                try {
                    Files.deleteIfExists(p);
                } catch (IOException ignored) {
                }
            });
        } catch (IOException ignored) {
        }
    }

    static <V> CompletionHandler<V, Object> latch(AtomicReference<Object> res, CountDownLatch l) {
        return new CompletionHandler<V, Object>() {
            public void completed(V v, Object att) {
                res.set("completed:" + v + ":" + att);
                l.countDown();
            }

            public void failed(Throwable t, Object att) {
                res.set("failed:" + t.getClass().getName() + ":" + att);
                l.countDown();
            }
        };
    }

    public static void main(String[] args) throws Exception {
        // ---- async socket channel -------------------------------------------------------
        row("as.fresh", () -> {
            try (AsynchronousSocketChannel c = AsynchronousSocketChannel.open()) {
                return cls(c) + " open=" + c.isOpen() + " local=" + c.getLocalAddress()
                        + " remote=" + ex(() -> String.valueOf(c.getRemoteAddress()))
                        + " provider=" + cls(c.provider()) + " opts=" + new java.util.TreeSet<>(
                                c.supportedOptions().stream().map(Object::toString).toList());
            }
        });
        row("as.echo", () -> {
            try (AsynchronousServerSocketChannel s = server()) {
                AsynchronousSocketChannel[] p = pair(s);
                try (AsynchronousSocketChannel c = p[0]; AsynchronousSocketChannel a = p[1]) {
                    int w = c.write(bb("hello async")).get(T, TimeUnit.SECONDS);
                    ByteBuffer in = ByteBuffer.allocate(11);
                    int n = readN(a, in);
                    a.write(bb("pong")).get(T, TimeUnit.SECONDS);
                    ByteBuffer in2 = ByteBuffer.allocate(4);
                    int n2 = readN(c, in2);
                    return "w=" + w + " n=" + n + " got=" + str(in) + " n2=" + n2 + " got2=" + str(in2)
                            + " localSet=" + (c.getLocalAddress() != null)
                            + " remoteIsServer=" + c.getRemoteAddress().equals(s.getLocalAddress())
                            + " acls=" + cls(a) + " ccls=" + cls(c);
                }
            }
        });
        row("as.handlers", () -> {
            try (AsynchronousServerSocketChannel s = server()) {
                AtomicReference<Object> accRes = new AtomicReference<>();
                CountDownLatch accL = new CountDownLatch(1);
                AtomicReference<AsynchronousSocketChannel> got = new AtomicReference<>();
                s.accept("acc", new CompletionHandler<AsynchronousSocketChannel, Object>() {
                    public void completed(AsynchronousSocketChannel ch, Object att) {
                        got.set(ch);
                        accRes.set("completed:" + att);
                        accL.countDown();
                    }

                    public void failed(Throwable t, Object att) {
                        accRes.set("failed:" + t);
                        accL.countDown();
                    }
                });
                try (AsynchronousSocketChannel c = AsynchronousSocketChannel.open()) {
                    AtomicReference<Object> conRes = new AtomicReference<>();
                    CountDownLatch conL = new CountDownLatch(1);
                    c.connect(s.getLocalAddress(), "con", latch(conRes, conL));
                    boolean a1 = accL.await(T, TimeUnit.SECONDS), a2 = conL.await(T, TimeUnit.SECONDS);
                    AsynchronousSocketChannel a = got.get();
                    AtomicReference<Object> wRes = new AtomicReference<>(), rRes = new AtomicReference<>();
                    CountDownLatch wL = new CountDownLatch(1), rL = new CountDownLatch(1);
                    ByteBuffer in = ByteBuffer.allocate(5);
                    a.read(in, "rd", latch(rRes, rL));
                    c.write(bb("12345"), "wr", latch(wRes, wL));
                    boolean w1 = wL.await(T, TimeUnit.SECONDS), r1 = rL.await(T, TimeUnit.SECONDS);
                    a.close();
                    return accRes.get() + " " + conRes.get() + " " + a1 + a2 + " w=" + wRes.get() + " r=" + rRes.get()
                            + " " + w1 + r1 + " got=" + str(in);
                }
            }
        });
        row("as.handlerThread", () -> {
            try (AsynchronousServerSocketChannel s = server()) {
                AsynchronousSocketChannel[] p = pair(s);
                try (AsynchronousSocketChannel c = p[0]; AsynchronousSocketChannel a = p[1]) {
                    AtomicReference<Thread> th = new AtomicReference<>();
                    CountDownLatch l = new CountDownLatch(1);
                    ByteBuffer in = ByteBuffer.allocate(1);
                    a.read(in, null, new CompletionHandler<Integer, Object>() {
                        public void completed(Integer v, Object att) {
                            th.set(Thread.currentThread());
                            l.countDown();
                        }

                        public void failed(Throwable t, Object att) {
                            l.countDown();
                        }
                    });
                    c.write(bb("x")).get(T, TimeUnit.SECONDS);
                    l.await(T, TimeUnit.SECONDS);
                    Thread t = th.get();
                    return "ran=" + (t != null) + " daemon=" + (t != null && t.isDaemon())
                            + " virtual=" + (t != null && t.isVirtual());
                }
            }
        });
        row("as.readTimeout", () -> {
            try (AsynchronousServerSocketChannel s = server()) {
                AsynchronousSocketChannel[] p = pair(s);
                try (AsynchronousSocketChannel c = p[0]; AsynchronousSocketChannel a = p[1]) {
                    AtomicReference<Object> res = new AtomicReference<>();
                    CountDownLatch l = new CountDownLatch(1);
                    a.read(ByteBuffer.allocate(4), 100, TimeUnit.MILLISECONDS, "t", latch(res, l));
                    l.await(T, TimeUnit.SECONDS);
                    // After a timeout the channel refuses further reads.
                    String again = ex(() -> String.valueOf(a.read(ByteBuffer.allocate(1)).get(T, TimeUnit.SECONDS)));
                    return res.get() + " again=" + again;
                }
            }
        });
        row("as.readPending", () -> {
            try (AsynchronousServerSocketChannel s = server()) {
                AsynchronousSocketChannel[] p = pair(s);
                try (AsynchronousSocketChannel c = p[0]; AsynchronousSocketChannel a = p[1]) {
                    Future<Integer> f1 = a.read(ByteBuffer.allocate(4));
                    String second = ex(() -> String.valueOf(a.read(ByteBuffer.allocate(4))));
                    c.write(bb("go")).get(T, TimeUnit.SECONDS);
                    return "second=" + second + " first=" + f1.get(T, TimeUnit.SECONDS) + " done=" + f1.isDone();
                }
            }
        });
        row("as.notConnected", () -> {
            try (AsynchronousSocketChannel c = AsynchronousSocketChannel.open()) {
                return "read=" + ex(() -> String.valueOf(c.read(ByteBuffer.allocate(1))))
                        + " write=" + ex(() -> String.valueOf(c.write(bb("x"))))
                        + " shIn=" + ex(() -> String.valueOf(c.shutdownInput()))
                        + " shOut=" + ex(() -> String.valueOf(c.shutdownOutput()));
            }
        });
        row("as.connectTwice", () -> {
            try (AsynchronousServerSocketChannel s = server();
                    AsynchronousSocketChannel c = AsynchronousSocketChannel.open()) {
                c.connect(s.getLocalAddress()).get(T, TimeUnit.SECONDS);
                return "again=" + ex(() -> String.valueOf(c.connect(s.getLocalAddress())));
            }
        });
        row("as.connectRefused", () -> {
            int port;
            try (AsynchronousServerSocketChannel s = server()) {
                port = ((InetSocketAddress) s.getLocalAddress()).getPort();
            }
            try (AsynchronousSocketChannel c = AsynchronousSocketChannel.open()) {
                String r = ex(() -> String.valueOf(c.connect(new InetSocketAddress(LO, port)).get(T, TimeUnit.SECONDS)));
                Throwable cause = null;
                try {
                    c.connect(new InetSocketAddress(LO, port)).get(T, TimeUnit.SECONDS);
                } catch (ExecutionException e) {
                    cause = e.getCause();
                } catch (Throwable t) {
                    cause = t;
                }
                return r + " msg=" + (cause == null ? "null" : cause.getMessage()) + " open=" + c.isOpen();
            }
        });
        row("as.close", () -> {
            AsynchronousSocketChannel c = AsynchronousSocketChannel.open();
            c.close();
            c.close();
            return "open=" + c.isOpen() + " connect=" + ex(() -> String.valueOf(c.connect(new InetSocketAddress(LO, 1))))
                    + " read=" + ex(() -> String.valueOf(c.read(ByteBuffer.allocate(1))))
                    + " write=" + ex(() -> String.valueOf(c.write(bb("x"))))
                    + " local=" + ex(() -> String.valueOf(c.getLocalAddress()))
                    + " bind=" + ex(() -> String.valueOf(c.bind(null)))
                    + " opt=" + ex(() -> String.valueOf(c.setOption(StandardSocketOptions.TCP_NODELAY, true)));
        });
        row("as.closeWhileRead", () -> {
            try (AsynchronousServerSocketChannel s = server()) {
                AsynchronousSocketChannel[] p = pair(s);
                try (AsynchronousSocketChannel c = p[0]; AsynchronousSocketChannel a = p[1]) {
                    AtomicReference<Object> res = new AtomicReference<>();
                    CountDownLatch l = new CountDownLatch(1);
                    a.read(ByteBuffer.allocate(4), "att", latch(res, l));
                    Thread.sleep(50);
                    a.close();
                    boolean done = l.await(T, TimeUnit.SECONDS);
                    return res.get() + " " + done;
                }
            }
        });
        row("as.eofAndShutdown", () -> {
            try (AsynchronousServerSocketChannel s = server()) {
                AsynchronousSocketChannel[] p = pair(s);
                try (AsynchronousSocketChannel c = p[0]; AsynchronousSocketChannel a = p[1]) {
                    c.write(bb("ab")).get(T, TimeUnit.SECONDS);
                    c.shutdownOutput();
                    ByteBuffer in = ByteBuffer.allocate(8);
                    int n1 = a.read(in).get(T, TimeUnit.SECONDS);
                    int n2 = a.read(ByteBuffer.allocate(8)).get(T, TimeUnit.SECONDS);
                    String wr = ex(() -> String.valueOf(c.write(bb("z")).get(T, TimeUnit.SECONDS)));
                    a.shutdownInput();
                    int n3 = a.read(ByteBuffer.allocate(8)).get(T, TimeUnit.SECONDS);
                    return "n1=" + n1 + " n2=" + n2 + " n3=" + n3 + " writeAfterShutdown=" + wr;
                }
            }
        });
        row("as.options", () -> {
            try (AsynchronousSocketChannel c = AsynchronousSocketChannel.open()) {
                c.setOption(StandardSocketOptions.TCP_NODELAY, true);
                c.setOption(StandardSocketOptions.SO_KEEPALIVE, true);
                c.setOption(StandardSocketOptions.SO_RCVBUF, 65536);
                Object r = c.bind(new InetSocketAddress(LO, 0));
                return "nodelay=" + c.getOption(StandardSocketOptions.TCP_NODELAY) + " keep="
                        + c.getOption(StandardSocketOptions.SO_KEEPALIVE) + " rcvOk="
                        + (c.getOption(StandardSocketOptions.SO_RCVBUF) >= 65536)
                        + " same=" + (r == c) + " local=" + shape(c.getLocalAddress())
                        + " bindTwice=" + ex(() -> String.valueOf(c.bind(null)))
                        + " badOpt=" + ex(() -> String.valueOf(c.getOption(StandardSocketOptions.IP_TOS)));
            }
        });
        row("as.bulk", () -> {
            try (AsynchronousServerSocketChannel s = server()) {
                AsynchronousSocketChannel[] p = pair(s);
                try (AsynchronousSocketChannel c = p[0]; AsynchronousSocketChannel a = p[1]) {
                    int size = 1 << 20;
                    byte[] data = new byte[size];
                    for (int i = 0; i < size; i++) data[i] = (byte) (i * 31 + 7);
                    Thread w = new Thread(() -> {
                        try {
                            ByteBuffer src = ByteBuffer.wrap(data);
                            while (src.hasRemaining()) c.write(src).get(T, TimeUnit.SECONDS);
                            c.shutdownOutput();
                        } catch (Exception e) {
                            e.printStackTrace();
                        }
                    });
                    w.start();
                    ByteBuffer in = ByteBuffer.allocate(size);
                    int total = 0;
                    while (in.hasRemaining()) {
                        int n = a.read(in).get(T, TimeUnit.SECONDS);
                        if (n < 0) break;
                        total += n;
                    }
                    w.join();
                    long sum = 0;
                    for (int i = 0; i < size; i++) sum += in.get(i) * (i % 251);
                    return "total=" + total + " sum=" + sum;
                }
            }
        });
        row("as.scatterGather", () -> {
            try (AsynchronousServerSocketChannel s = server()) {
                AsynchronousSocketChannel[] p = pair(s);
                try (AsynchronousSocketChannel c = p[0]; AsynchronousSocketChannel a = p[1]) {
                    AtomicReference<Object> wres = new AtomicReference<>(), rres = new AtomicReference<>();
                    CountDownLatch wl = new CountDownLatch(1), rl = new CountDownLatch(1);
                    ByteBuffer[] src = {bb("aa"), bb("bbb"), bb("c")};
                    ByteBuffer[] dst = {ByteBuffer.allocate(2), ByteBuffer.allocate(4)};
                    a.read(dst, 0, 2, T, TimeUnit.SECONDS, "r", latch(rres, rl));
                    c.write(src, 0, 3, T, TimeUnit.SECONDS, "w", latch(wres, wl));
                    wl.await(T, TimeUnit.SECONDS);
                    rl.await(T, TimeUnit.SECONDS);
                    return wres.get() + " " + rres.get() + " " + str(dst[0]) + "/" + str(dst[1]);
                }
            }
        });
        row("as.cancelFuture", () -> {
            try (AsynchronousServerSocketChannel s = server()) {
                AsynchronousSocketChannel[] p = pair(s);
                try (AsynchronousSocketChannel c = p[0]; AsynchronousSocketChannel a = p[1]) {
                    Future<Integer> f = a.read(ByteBuffer.allocate(4));
                    boolean cancelled = f.cancel(true);
                    return "cancelled=" + cancelled + " isCancelled=" + f.isCancelled() + " done=" + f.isDone()
                            + " get=" + ex(() -> String.valueOf(f.get(T, TimeUnit.SECONDS)))
                            + " open=" + a.isOpen();
                }
            }
        });
        row("as.virtualHandler", () -> {
            try (AsynchronousServerSocketChannel s = server()) {
                AtomicReference<String> res = new AtomicReference<>("never");
                Thread v = Thread.ofVirtual().start(() -> {
                    try {
                        AsynchronousSocketChannel[] p = pair(s);
                        try (AsynchronousSocketChannel c = p[0]; AsynchronousSocketChannel a = p[1]) {
                            c.write(bb("virt")).get(T, TimeUnit.SECONDS);
                            ByteBuffer in = ByteBuffer.allocate(4);
                            int n = readN(a, in);
                            res.set("n=" + n + " got=" + str(in) + " vt=" + Thread.currentThread().isVirtual());
                        }
                    } catch (Throwable e) {
                        res.set("!" + e);
                    }
                });
                v.join(20_000);
                return res.get();
            }
        });

        // ---- async server socket ---------------------------------------------------------
        row("assc.fresh", () -> {
            try (AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()) {
                String before = "local0=" + s.getLocalAddress() + " open=" + s.isOpen();
                Object r = s.bind(new InetSocketAddress(LO, 0), 5);
                return cls(s) + " " + before + " same=" + (r == s) + " local=" + shape(s.getLocalAddress())
                        + " bindTwice=" + ex(() -> String.valueOf(s.bind(null)))
                        + " provider=" + cls(s.provider()) + " opts=" + new java.util.TreeSet<>(
                                s.supportedOptions().stream().map(Object::toString).toList());
            }
        });
        row("assc.notBound", () -> {
            try (AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()) {
                return "accept=" + ex(() -> String.valueOf(s.accept()))
                        + " local=" + s.getLocalAddress();
            }
        });
        row("assc.acceptPending", () -> {
            try (AsynchronousServerSocketChannel s = server()) {
                Future<AsynchronousSocketChannel> f = s.accept();
                String second = ex(() -> String.valueOf(s.accept()));
                try (AsynchronousSocketChannel c = AsynchronousSocketChannel.open()) {
                    c.connect(s.getLocalAddress()).get(T, TimeUnit.SECONDS);
                    AsynchronousSocketChannel a = f.get(T, TimeUnit.SECONDS);
                    a.close();
                    return "second=" + second + " first=" + (a != null);
                }
            }
        });
        row("assc.closeWhileAccept", () -> {
            AsynchronousServerSocketChannel s = server();
            AtomicReference<Object> res = new AtomicReference<>();
            CountDownLatch l = new CountDownLatch(1);
            s.accept("att", new CompletionHandler<AsynchronousSocketChannel, Object>() {
                public void completed(AsynchronousSocketChannel ch, Object att) {
                    res.set("completed");
                    l.countDown();
                }

                public void failed(Throwable t, Object att) {
                    res.set("failed:" + t.getClass().getName() + ":" + att);
                    l.countDown();
                }
            });
            Thread.sleep(50);
            s.close();
            boolean done = l.await(T, TimeUnit.SECONDS);
            return res.get() + " " + done + " open=" + s.isOpen()
                    + " afterClose=" + ex(() -> String.valueOf(s.accept()))
                    + " localAfterClose=" + ex(() -> String.valueOf(s.getLocalAddress()));
        });
        row("assc.options", () -> {
            try (AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()) {
                s.setOption(StandardSocketOptions.SO_REUSEADDR, true);
                s.setOption(StandardSocketOptions.SO_RCVBUF, 32768);
                return "reuse=" + s.getOption(StandardSocketOptions.SO_REUSEADDR) + " rcvOk="
                        + (s.getOption(StandardSocketOptions.SO_RCVBUF) > 0)
                        + " bad=" + ex(() -> String.valueOf(s.getOption(StandardSocketOptions.TCP_NODELAY)));
            }
        });
        row("assc.acceptedAddresses", () -> {
            try (AsynchronousServerSocketChannel s = server()) {
                AsynchronousSocketChannel[] p = pair(s);
                try (AsynchronousSocketChannel c = p[0]; AsynchronousSocketChannel a = p[1]) {
                    return "aLocal=" + a.getLocalAddress().equals(s.getLocalAddress())
                            + " aRemote=" + a.getRemoteAddress().equals(c.getLocalAddress())
                            + " cRemote=" + c.getRemoteAddress().equals(s.getLocalAddress());
                }
            }
        });
        row("assc.manyClients", () -> {
            try (AsynchronousServerSocketChannel s = server()) {
                int n = 8;
                AtomicInteger served = new AtomicInteger();
                CountDownLatch done = new CountDownLatch(n);
                s.accept(null, new CompletionHandler<AsynchronousSocketChannel, Object>() {
                    public void completed(AsynchronousSocketChannel ch, Object att) {
                        s.accept(null, this);
                        ByteBuffer b = ByteBuffer.allocate(4);
                        ch.read(b, null, new CompletionHandler<Integer, Object>() {
                            public void completed(Integer v, Object a) {
                                b.flip();
                                ch.write(b, null, new CompletionHandler<Integer, Object>() {
                                    public void completed(Integer v2, Object a2) {
                                        served.incrementAndGet();
                                        try {
                                            ch.close();
                                        } catch (IOException ignored) {
                                        }
                                        done.countDown();
                                    }

                                    public void failed(Throwable t, Object a2) {
                                        done.countDown();
                                    }
                                });
                            }

                            public void failed(Throwable t, Object a) {
                                done.countDown();
                            }
                        });
                    }

                    public void failed(Throwable t, Object att) {
                    }
                });
                StringBuilder sb = new StringBuilder();
                List<String> outs = new ArrayList<>();
                for (int i = 0; i < n; i++) {
                    try (AsynchronousSocketChannel c = AsynchronousSocketChannel.open()) {
                        c.connect(s.getLocalAddress()).get(T, TimeUnit.SECONDS);
                        c.write(bb("id" + i % 10)).get(T, TimeUnit.SECONDS);
                        ByteBuffer in = ByteBuffer.allocate(3);
                        readN(c, in);
                        outs.add(str(in));
                    }
                }
                done.await(T, TimeUnit.SECONDS);
                return "served=" + served.get() + " " + outs;
            }
        });

        // ---- channel groups --------------------------------------------------------------
        row("group.fixed", () -> {
            AtomicInteger created = new AtomicInteger();
            AsynchronousChannelGroup g = AsynchronousChannelGroup.withFixedThreadPool(2, r -> {
                created.incrementAndGet();
                Thread t = new Thread(r, "sbrw6-grp");
                t.setDaemon(true);
                return t;
            });
            try (AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open(g)
                    .bind(new InetSocketAddress(LO, 0))) {
                Future<AsynchronousSocketChannel> acc = s.accept();
                try (AsynchronousSocketChannel c = AsynchronousSocketChannel.open(g)) {
                    c.connect(s.getLocalAddress()).get(T, TimeUnit.SECONDS);
                    AsynchronousSocketChannel a = acc.get(T, TimeUnit.SECONDS);
                    AtomicReference<String> tn = new AtomicReference<>();
                    CountDownLatch l = new CountDownLatch(1);
                    a.read(ByteBuffer.allocate(1), null, new CompletionHandler<Integer, Object>() {
                        public void completed(Integer v, Object x) {
                            tn.set(Thread.currentThread().getName());
                            l.countDown();
                        }

                        public void failed(Throwable t, Object x) {
                            l.countDown();
                        }
                    });
                    c.write(bb("q")).get(T, TimeUnit.SECONDS);
                    l.await(T, TimeUnit.SECONDS);
                    a.close();
                    String before = "shutdown=" + g.isShutdown() + " term=" + g.isTerminated();
                    g.shutdown();
                    String afterShut = "shutdown=" + g.isShutdown();
                    String open = ex(() -> String.valueOf(AsynchronousSocketChannel.open(g)));
                    return before + " thread=" + tn.get() + " createdAtLeast1=" + (created.get() >= 1)
                            + " " + afterShut + " openAfterShutdown=" + open;
                }
            } finally {
                g.shutdownNow();
                g.awaitTermination(T, TimeUnit.SECONDS);
            }
        });
        row("group.terminate", () -> {
            ExecutorService ex = Executors.newCachedThreadPool(r -> {
                Thread t = new Thread(r, "sbrw6-cached");
                t.setDaemon(true);
                return t;
            });
            AsynchronousChannelGroup g = AsynchronousChannelGroup.withCachedThreadPool(ex, 1);
            AsynchronousSocketChannel c = AsynchronousSocketChannel.open(g);
            AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open(g);
            g.shutdown();
            boolean term0 = g.isTerminated();
            boolean w0 = g.awaitTermination(200, TimeUnit.MILLISECONDS);
            c.close();
            s.close();
            boolean w1 = g.awaitTermination(T, TimeUnit.SECONDS);
            return "term0=" + term0 + " await0=" + w0 + " await1=" + w1 + " term=" + g.isTerminated()
                    + " exShutdown=" + ex.isShutdown() + " provider=" + cls(g.provider());
        });
        row("group.shutdownNow", () -> {
            AsynchronousChannelGroup g = AsynchronousChannelGroup.withThreadPool(Executors.newFixedThreadPool(2, r -> {
                Thread t = new Thread(r);
                t.setDaemon(true);
                return t;
            }));
            AsynchronousSocketChannel c = AsynchronousSocketChannel.open(g);
            AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open(g);
            g.shutdownNow();
            boolean w = g.awaitTermination(T, TimeUnit.SECONDS);
            return "cOpen=" + c.isOpen() + " sOpen=" + s.isOpen() + " term=" + w;
        });
        row("group.badArgs", () -> {
            return "fixed0=" + ex(() -> String.valueOf(AsynchronousChannelGroup.withFixedThreadPool(0, Thread::new)))
                    + " cachedNeg=" + ex(() -> String.valueOf(AsynchronousChannelGroup
                            .withCachedThreadPool(Executors.newCachedThreadPool(), -1)))
                    + " fixedNullTf=" + ex(() -> String.valueOf(AsynchronousChannelGroup.withFixedThreadPool(1, null)));
        });

        // ---- AsynchronousFileChannel -----------------------------------------------------
        row("afc.roundtrip", () -> {
            Path d = tmp();
            try {
                Path f = d.resolve("a.bin");
                try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(f, StandardOpenOption.CREATE,
                        StandardOpenOption.READ, StandardOpenOption.WRITE)) {
                    int w = ch.write(bb("hello afc"), 0).get(T, TimeUnit.SECONDS);
                    int w2 = ch.write(bb("!!"), 20).get(T, TimeUnit.SECONDS);
                    ch.force(true);
                    ByteBuffer in = ByteBuffer.allocate(9);
                    int n = ch.read(in, 0).get(T, TimeUnit.SECONDS);
                    ByteBuffer eof = ByteBuffer.allocate(4);
                    int e = ch.read(eof, 100).get(T, TimeUnit.SECONDS);
                    ch.truncate(5);
                    return cls(ch) + " w=" + w + " w2=" + w2 + " n=" + n + " got=" + str(in) + " eof=" + e
                            + " size=" + ch.size() + " open=" + ch.isOpen()
                            + " file=" + Files.size(f);
                }
            } finally {
                rm(d);
            }
        });
        row("afc.handlers", () -> {
            Path d = tmp();
            try {
                Path f = d.resolve("h.bin");
                try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(f, StandardOpenOption.CREATE,
                        StandardOpenOption.READ, StandardOpenOption.WRITE)) {
                    AtomicReference<Object> wr = new AtomicReference<>(), rr = new AtomicReference<>();
                    CountDownLatch wl = new CountDownLatch(1), rl = new CountDownLatch(1);
                    ch.write(bb("handler"), 0, "w", latch(wr, wl));
                    wl.await(T, TimeUnit.SECONDS);
                    ByteBuffer in = ByteBuffer.allocate(7);
                    ch.read(in, 0, "r", latch(rr, rl));
                    rl.await(T, TimeUnit.SECONDS);
                    return wr.get() + " " + rr.get() + " " + str(in);
                }
            } finally {
                rm(d);
            }
        });
        row("afc.handlerThread", () -> {
            Path d = tmp();
            try {
                Path f = Files.writeString(d.resolve("t.bin"), "x");
                try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(f, StandardOpenOption.READ)) {
                    AtomicReference<Thread> th = new AtomicReference<>();
                    CountDownLatch l = new CountDownLatch(1);
                    ch.read(ByteBuffer.allocate(1), 0, null, new CompletionHandler<Integer, Object>() {
                        public void completed(Integer v, Object a) {
                            th.set(Thread.currentThread());
                            l.countDown();
                        }

                        public void failed(Throwable t, Object a) {
                            l.countDown();
                        }
                    });
                    l.await(T, TimeUnit.SECONDS);
                    Thread t = th.get();
                    return "ran=" + (t != null) + " notMain=" + (t != Thread.currentThread())
                            + " daemon=" + (t != null && t.isDaemon());
                }
            } finally {
                rm(d);
            }
        });
        row("afc.modeChecks", () -> {
            Path d = tmp();
            try {
                Path f = Files.writeString(d.resolve("m.bin"), "abc");
                String r;
                try (AsynchronousFileChannel ro = AsynchronousFileChannel.open(f, StandardOpenOption.READ)) {
                    r = "roWrite=" + ex(() -> String.valueOf(ro.write(bb("z"), 0)))
                            + " roTrunc=" + ex(() -> String.valueOf(ro.truncate(1)))
                            + " roLockExcl=" + ex(() -> String.valueOf(ro.lock(0, 1, false).get(T, TimeUnit.SECONDS)))
                            + " roTrySh=" + ex(() -> String.valueOf(ro.tryLock(0, 1, true).isShared()))
                            + " negPos=" + ex(() -> String.valueOf(ro.read(ByteBuffer.allocate(1), -1)));
                }
                try (AsynchronousFileChannel wo = AsynchronousFileChannel.open(f, StandardOpenOption.WRITE)) {
                    r += " woRead=" + ex(() -> String.valueOf(wo.read(ByteBuffer.allocate(1), 0)))
                            + " woLockShared=" + ex(() -> String.valueOf(wo.lock(0, 1, true).get(T, TimeUnit.SECONDS)));
                }
                r += " append=" + ex(() -> String.valueOf(AsynchronousFileChannel.open(f, StandardOpenOption.APPEND)))
                        + " missing=" + ex(() -> String.valueOf(AsynchronousFileChannel.open(d.resolve("nope"),
                                StandardOpenOption.READ)))
                        + " createNew=" + ex(() -> String.valueOf(AsynchronousFileChannel.open(f,
                                StandardOpenOption.CREATE_NEW, StandardOpenOption.WRITE)));
                return r;
            } finally {
                rm(d);
            }
        });
        row("afc.closed", () -> {
            Path d = tmp();
            try {
                Path f = Files.writeString(d.resolve("c.bin"), "abc");
                AsynchronousFileChannel ch = AsynchronousFileChannel.open(f, StandardOpenOption.READ,
                        StandardOpenOption.WRITE);
                ch.close();
                ch.close();
                AtomicReference<Object> res = new AtomicReference<>();
                CountDownLatch l = new CountDownLatch(1);
                String h = "?";
                try {
                    ch.read(ByteBuffer.allocate(1), 0, "a", latch(res, l));
                    l.await(T, TimeUnit.SECONDS);
                    h = String.valueOf(res.get());
                } catch (Throwable t) {
                    h = "threw:" + t.getClass().getName();
                }
                return "open=" + ch.isOpen() + " read=" + ex(() -> String.valueOf(ch.read(ByteBuffer.allocate(1), 0).get(T, TimeUnit.SECONDS)))
                        + " write=" + ex(() -> String.valueOf(ch.write(bb("x"), 0).get(T, TimeUnit.SECONDS)))
                        + " size=" + ex(() -> String.valueOf(ch.size()))
                        + " force=" + ex(() -> { ch.force(false); return "ok"; })
                        + " trunc=" + ex(() -> String.valueOf(ch.truncate(0)))
                        + " lock=" + ex(() -> String.valueOf(ch.lock().get(T, TimeUnit.SECONDS)))
                        + " tryLock=" + ex(() -> String.valueOf(ch.tryLock()))
                        + " handler=" + h;
            } finally {
                rm(d);
            }
        });
        row("afc.closeWhilePending", () -> {
            Path d = tmp();
            try {
                Path f = Files.writeString(d.resolve("p.bin"), "abc");
                AsynchronousFileChannel ch = AsynchronousFileChannel.open(f, StandardOpenOption.READ,
                        StandardOpenOption.WRITE);
                FileLock lk = ch.lock().get(T, TimeUnit.SECONDS);
                String before = "valid=" + lk.isValid();
                ch.close();
                return before + " afterClose=" + lk.isValid();
            } finally {
                rm(d);
            }
        });
        row("afc.locks", () -> {
            Path d = tmp();
            try {
                Path f = Files.writeString(d.resolve("l.bin"), "0123456789");
                try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(f, StandardOpenOption.READ,
                        StandardOpenOption.WRITE)) {
                    FileLock a = ch.lock(0, 5, false).get(T, TimeUnit.SECONDS);
                    String r = "a.valid=" + a.isValid() + " shared=" + a.isShared() + " pos=" + a.position()
                            + " size=" + a.size() + " chan=" + (a.channel() == null) + " acq=" + (a.acquiredBy() == ch)
                            + " overlapsIn=" + a.overlaps(2, 1) + " overlapsOut=" + a.overlaps(6, 1)
                            + " str=" + shape(a);
                    r += " overlap=" + ex(() -> String.valueOf(ch.tryLock(3, 4, false)));
                    r += " overlapLock=" + ex(() -> String.valueOf(ch.lock(3, 4, false).get(T, TimeUnit.SECONDS)));
                    FileLock b = ch.tryLock(6, 2, false);
                    r += " b=" + (b != null) + " bvalid=" + (b != null && b.isValid());
                    a.release();
                    a.release();
                    r += " aAfter=" + a.isValid();
                    FileLock again = ch.tryLock(0, 5, false);
                    r += " again=" + (again != null);
                    if (again != null) again.close();
                    if (b != null) b.release();
                    FileLock whole = ch.lock().get(T, TimeUnit.SECONDS);
                    r += " whole=" + whole.position() + "/" + (whole.size() == Long.MAX_VALUE) + " sharedFlag=" + whole.isShared();
                    whole.release();
                    // Handler form
                    AtomicReference<Object> res = new AtomicReference<>();
                    CountDownLatch l = new CountDownLatch(1);
                    AtomicReference<FileLock> got = new AtomicReference<>();
                    ch.lock(0, 3, false, "att", new CompletionHandler<FileLock, Object>() {
                        public void completed(FileLock v, Object att) {
                            got.set(v);
                            res.set("completed:" + att);
                            l.countDown();
                        }

                        public void failed(Throwable t, Object att) {
                            res.set("failed:" + t);
                            l.countDown();
                        }
                    });
                    l.await(T, TimeUnit.SECONDS);
                    r += " handler=" + res.get() + " gotValid=" + (got.get() != null && got.get().isValid());
                    if (got.get() != null) got.get().release();
                    return r;
                }
            } finally {
                rm(d);
            }
        });
        row("afc.executorOpen", () -> {
            Path d = tmp();
            ExecutorService ex = Executors.newFixedThreadPool(2, r -> {
                Thread t = new Thread(r, "sbrw6-afc");
                t.setDaemon(true);
                return t;
            });
            try {
                Path f = d.resolve("e.bin");
                try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(f,
                        Set.of(StandardOpenOption.CREATE, StandardOpenOption.READ, StandardOpenOption.WRITE), ex)) {
                    ch.write(bb("exec"), 0).get(T, TimeUnit.SECONDS);
                    AtomicReference<String> tn = new AtomicReference<>();
                    CountDownLatch l = new CountDownLatch(1);
                    ByteBuffer in = ByteBuffer.allocate(4);
                    ch.read(in, 0, null, new CompletionHandler<Integer, Object>() {
                        public void completed(Integer v, Object a) {
                            tn.set(Thread.currentThread().getName());
                            l.countDown();
                        }

                        public void failed(Throwable t, Object a) {
                            l.countDown();
                        }
                    });
                    l.await(T, TimeUnit.SECONDS);
                    return "got=" + str(in) + " thread=" + tn.get();
                }
            } finally {
                ex.shutdownNow();
                rm(d);
            }
        });
        row("afc.provider", () -> {
            Path d = tmp();
            ExecutorService ex = Executors.newSingleThreadExecutor(r -> {
                Thread t = new Thread(r, "sbrw6-prov");
                t.setDaemon(true);
                return t;
            });
            try {
                Path f = d.resolve("pv.bin");
                java.nio.file.spi.FileSystemProvider p = FileSystems.getDefault().provider();
                try (AsynchronousFileChannel ch = p.newAsynchronousFileChannel(f,
                        Set.of(StandardOpenOption.CREATE, StandardOpenOption.WRITE, StandardOpenOption.READ), ex)) {
                    ch.write(bb("prov"), 0).get(T, TimeUnit.SECONDS);
                    ByteBuffer in = ByteBuffer.allocate(4);
                    ch.read(in, 0).get(T, TimeUnit.SECONDS);
                    return cls(ch) + " got=" + str(in) + " size=" + ch.size();
                }
            } finally {
                ex.shutdownNow();
                rm(d);
            }
        });
        row("afc.parallel", () -> {
            Path d = tmp();
            try {
                Path f = d.resolve("par.bin");
                try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(f, StandardOpenOption.CREATE,
                        StandardOpenOption.READ, StandardOpenOption.WRITE)) {
                    List<Future<Integer>> fs = new ArrayList<>();
                    for (int i = 0; i < 32; i++) {
                        ByteBuffer b = ByteBuffer.allocate(8);
                        for (int k = 0; k < 8; k++) b.put((byte) i);
                        b.flip();
                        fs.add(ch.write(b, i * 8L));
                    }
                    int total = 0;
                    for (Future<Integer> x : fs) total += x.get(T, TimeUnit.SECONDS);
                    ByteBuffer all = ByteBuffer.allocate(32 * 8);
                    ch.read(all, 0).get(T, TimeUnit.SECONDS);
                    long sum = 0;
                    for (int i = 0; i < 256; i++) sum += all.get(i) * (i + 1);
                    return "total=" + total + " sum=" + sum + " size=" + ch.size();
                }
            } finally {
                rm(d);
            }
        });

        // ---- FileLockImpl on the synchronous channel -------------------------------------
        row("filelock.sync", () -> {
            Path d = tmp();
            try {
                Path f = Files.writeString(d.resolve("s.bin"), "0123456789");
                try (FileChannel ch = FileChannel.open(f, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
                    FileLock a = ch.lock(0, 5, false);
                    String r = "valid=" + a.isValid() + " chan=" + (a.channel() == ch) + " acq=" + (a.acquiredBy() == ch)
                            + " str=" + shape(a);
                    r += " overlap=" + ex(() -> String.valueOf(ch.tryLock(2, 2, false)));
                    a.release();
                    a.release();
                    r += " after=" + a.isValid();
                    FileLock b = ch.tryLock();
                    r += " b=" + (b != null) + " bsize=" + (b != null && b.size() == Long.MAX_VALUE);
                    ch.close();
                    r += " afterClose=" + (b != null && b.isValid());
                    return r;
                }
            } finally {
                rm(d);
            }
        });

        // ---- FileSystem.getFileStores ----------------------------------------------------
        row("fs.getFileStores", () -> {
            List<FileStore> all = new ArrayList<>();
            FileSystems.getDefault().getFileStores().forEach(all::add);
            FileStore root = Files.getFileStore(Path.of("/"));
            boolean rootListed = all.stream().anyMatch(s -> s.equals(root));
            boolean allNamed = all.stream().allMatch(s -> s.name() != null && !s.type().isEmpty());
            boolean allSized = all.stream().allMatch(s -> {
                try {
                    return s.getTotalSpace() >= 0;
                } catch (IOException e) {
                    return true;
                }
            });
            boolean unique = all.stream().map(FileStore::toString).distinct().count() == all.size();
            String tmpType = Files.getFileStore(Path.of("/tmp")).type();
            FileStore tmpStore = safeStore("/tmp");
            boolean tmpListed = all.stream().anyMatch(s -> s.equals(tmpStore));
            return "nonEmpty=" + !all.isEmpty() + " rootListed=" + rootListed + " named=" + allNamed
                    + " sized=" + allSized + " unique=" + unique + " tmpTypeNonEmpty=" + !tmpType.isEmpty()
                    + " tmpListed=" + tmpListed + " cls=" + cls(all.get(0)) + " again="
                    + (count() == all.size());
        });
        row("fs.getFileStores.iter", () -> {
            var it = FileSystems.getDefault().getFileStores().iterator();
            int n = 0;
            while (it.hasNext()) {
                it.next();
                n++;
            }
            String rm = ex(() -> {
                var i2 = FileSystems.getDefault().getFileStores().iterator();
                i2.next();
                i2.remove();
                return "removed";
            });
            return "n>0=" + (n > 0) + " exhausted=" + ex(() -> String.valueOf(it.next())) + " remove=" + rm;
        });

        // ---- SO_REUSEPORT: Net.isReusePortAvailable0 and the option round trip -------------
        row("opt.reuseport", () -> {
            StringBuilder sb = new StringBuilder();
            sb.append("sc=").append(java.nio.channels.SocketChannel.open().supportedOptions()
                    .contains(StandardSocketOptions.SO_REUSEPORT));
            sb.append(" ssc=").append(java.nio.channels.ServerSocketChannel.open().supportedOptions()
                    .contains(StandardSocketOptions.SO_REUSEPORT));
            sb.append(" dc=").append(java.nio.channels.DatagramChannel.open().supportedOptions()
                    .contains(StandardSocketOptions.SO_REUSEPORT));
            try (AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()) {
                s.setOption(StandardSocketOptions.SO_REUSEPORT, true);
                sb.append(" asscGet=").append(s.getOption(StandardSocketOptions.SO_REUSEPORT));
            }
            try (java.nio.channels.ServerSocketChannel s = java.nio.channels.ServerSocketChannel.open()) {
                s.setOption(StandardSocketOptions.SO_REUSEPORT, true);
                sb.append(" sscGet=").append(s.getOption(StandardSocketOptions.SO_REUSEPORT));
            }
            try (java.net.ServerSocket s = new java.net.ServerSocket()) {
                s.setOption(StandardSocketOptions.SO_REUSEPORT, true);
                sb.append(" ssGet=").append(s.getOption(StandardSocketOptions.SO_REUSEPORT));
            }
            return sb.toString();
        });
        // ---- virtual threads on real channels (NativeThread.current() == -1) -----------------
        row("vt.channelClose", () -> {
            try (java.nio.channels.ServerSocketChannel s = java.nio.channels.ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                java.nio.channels.SocketChannel c = java.nio.channels.SocketChannel.open(s.getLocalAddress());
                try (java.nio.channels.SocketChannel a = s.accept()) {
                    AtomicReference<String> res = new AtomicReference<>("never");
                    CountDownLatch started = new CountDownLatch(1);
                    Thread v = Thread.ofVirtual().start(() -> {
                        started.countDown();
                        try {
                            c.read(ByteBuffer.allocate(4));
                            res.set("read-returned");
                        } catch (Throwable e) {
                            res.set(e.getClass().getName() + " vt=" + Thread.currentThread().isVirtual());
                        }
                    });
                    started.await();
                    Thread.sleep(150);
                    c.close();
                    v.join(10_000);
                    return res.get() + " alive=" + v.isAlive();
                }
            }
        });
        row("vt.channelInterrupt", () -> {
            try (java.nio.channels.ServerSocketChannel s = java.nio.channels.ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                try (java.nio.channels.SocketChannel c = java.nio.channels.SocketChannel.open(s.getLocalAddress());
                        java.nio.channels.SocketChannel a = s.accept()) {
                    AtomicReference<String> res = new AtomicReference<>("never");
                    CountDownLatch started = new CountDownLatch(1);
                    Thread v = Thread.ofVirtual().start(() -> {
                        started.countDown();
                        try {
                            c.read(ByteBuffer.allocate(4));
                            res.set("read-returned");
                        } catch (Throwable e) {
                            res.set(e.getClass().getName());
                        }
                    });
                    started.await();
                    Thread.sleep(150);
                    v.interrupt();
                    v.join(10_000);
                    return res.get() + " alive=" + v.isAlive() + " cOpen=" + c.isOpen();
                }
            }
        });
        row("vt.fileChannelLock", () -> {
            Path d = tmp();
            try {
                Path f = Files.writeString(d.resolve("v.bin"), "0123456789");
                AtomicReference<String> res = new AtomicReference<>("never");
                Thread v = Thread.ofVirtual().start(() -> {
                    try (FileChannel ch = FileChannel.open(f, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
                        FileLock l = ch.lock();
                        ByteBuffer b = ByteBuffer.allocate(4);
                        ch.read(b, 2);
                        l.release();
                        res.set("ok " + str(b));
                    } catch (Throwable e) {
                        res.set("!" + e);
                    }
                });
                v.join(10_000);
                return res.get();
            } finally {
                rm(d);
            }
        });

        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println("row " + LABELS.get(i) + " = " + first[i]);
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 30;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                if (first[i] == TIMEOUT) continue;
                String v = safe(ROWS.get(i));
                if (!v.equals(first[i])) {
                    if (drift < 10) {
                        System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + v);
                    }
                    drift++;
                }
            }
        }
        System.out.println("rows " + ROWS.size() + " drift " + drift);
    }

    static FileStore safeStore(String p) {
        try {
            return Files.getFileStore(Path.of(p));
        } catch (IOException e) {
            return null;
        }
    }

    static int count() {
        int n = 0;
        for (FileStore s : FileSystems.getDefault().getFileStores()) n++;
        return n;
    }
}
