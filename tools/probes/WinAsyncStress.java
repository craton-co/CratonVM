import java.net.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

/**
 * Asynchronous channel edge rows on Windows (both modes against HotSpot): bulk echo through completion
 * handlers, a pending read / accept / connect interrupted by close, a read timeout, half-close, a
 * scatter/gather transfer, a locked range contended by two channels, a file read past EOF, group shutdown
 * with work in flight. Every printed line is a fact; no timings.
 */
public class WinAsyncStress {
    static String ex(Throwable t) {
        Throwable c = (t instanceof ExecutionException && t.getCause() != null) ? t.getCause() : t;
        return c.getClass().getName();
    }

    /** Future-form echo through the JDK channels: deterministic and handler-free. */
    static void echoFutures() throws Exception {
        AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()
                .bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
        int conns = 30, size = 300_000;
        Thread srv = new Thread(() -> {
            try {
                for (int i = 0; i < conns; i++) {
                    AsynchronousSocketChannel ch = s.accept().get(20, TimeUnit.SECONDS);
                    ByteBuffer b = ByteBuffer.allocate(32 * 1024);
                    int total = 0;
                    while (total < size) {
                        b.clear();
                        int n = ch.read(b).get(20, TimeUnit.SECONDS);
                        if (n < 0) break;
                        b.flip();
                        while (b.hasRemaining()) ch.write(b).get(20, TimeUnit.SECONDS);
                        total += n;
                    }
                    ch.close();
                }
            } catch (Throwable t) {
                System.out.println("srv ERR " + ex(t));
            }
        });
        srv.start();
        long sum = 0;
        for (int i = 0; i < conns; i++) {
            AsynchronousSocketChannel c = AsynchronousSocketChannel.open();
            c.connect(s.getLocalAddress()).get(20, TimeUnit.SECONDS);
            byte[] data = new byte[size];
            for (int k = 0; k < size; k++) data[k] = (byte) (k * 31 + i);
            ByteBuffer out = ByteBuffer.wrap(data);
            ByteBuffer in = ByteBuffer.allocate(size);
            Future<Integer> w = null;
            while (out.hasRemaining()) c.write(out).get(20, TimeUnit.SECONDS);
            while (in.hasRemaining()) {
                int n = c.read(in).get(20, TimeUnit.SECONDS);
                if (n < 0) break;
            }
            sum += Arrays.hashCode(in.array()) == Arrays.hashCode(data) ? 1 : 1000;
            c.close();
        }
        srv.join();
        System.out.println("echo ok=" + sum);
        s.close();
    }

    static void pendingClose() throws Exception {
        AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()
                .bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
        AsynchronousSocketChannel c = AsynchronousSocketChannel.open();
        c.connect(s.getLocalAddress()).get(5, TimeUnit.SECONDS);
        AsynchronousSocketChannel srv = s.accept().get(5, TimeUnit.SECONDS);
        Future<Integer> pr = c.read(ByteBuffer.allocate(8));
        Thread.sleep(200);
        c.close();
        try { System.out.println("pending-read " + pr.get(5, TimeUnit.SECONDS)); } catch (Throwable t) { System.out.println("pending-read " + ex(t)); }
        Future<AsynchronousSocketChannel> pa = s.accept();
        Thread.sleep(200);
        s.close();
        try { System.out.println("pending-accept " + pa.get(5, TimeUnit.SECONDS)); } catch (Throwable t) { System.out.println("pending-accept " + ex(t)); }
        try { c.write(ByteBuffer.wrap(new byte[1])).get(); } catch (Throwable t) { System.out.println("write-closed " + ex(t)); }
        srv.close();
    }

    static void timeoutAndHalfClose() throws Exception {
        AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()
                .bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
        AsynchronousSocketChannel c = AsynchronousSocketChannel.open();
        c.connect(s.getLocalAddress()).get(5, TimeUnit.SECONDS);
        AsynchronousSocketChannel srv = s.accept().get(5, TimeUnit.SECONDS);
        CountDownLatch l = new CountDownLatch(1);
        String[] res = new String[1];
        c.read(ByteBuffer.allocate(4), 300, TimeUnit.MILLISECONDS, null, new CompletionHandler<Integer, Object>() {
            public void completed(Integer r, Object o) { res[0] = "completed " + r; l.countDown(); }
            public void failed(Throwable t, Object o) { res[0] = "failed " + t.getClass().getName(); l.countDown(); }
        });
        l.await(5, TimeUnit.SECONDS);
        System.out.println("read-timeout " + res[0]);
        AsynchronousSocketChannel srv2 = srv;
        srv2.shutdownOutput();
        AsynchronousSocketChannel c2 = AsynchronousSocketChannel.open();
        c2.connect(s.getLocalAddress()).get(5, TimeUnit.SECONDS);
        AsynchronousSocketChannel srvB = s.accept().get(5, TimeUnit.SECONDS);
        srvB.shutdownOutput();
        System.out.println("half-close-eof " + c2.read(ByteBuffer.allocate(4)).get(5, TimeUnit.SECONDS));
        c2.write(ByteBuffer.wrap("x".getBytes())).get(5, TimeUnit.SECONDS);
        ByteBuffer rb = ByteBuffer.allocate(4);
        System.out.println("half-close-still-reads " + srvB.read(rb).get(5, TimeUnit.SECONDS));
        c.close(); c2.close(); srv.close(); srvB.close(); s.close();
    }

    static void gather() throws Exception {
        AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()
                .bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
        AsynchronousSocketChannel c = AsynchronousSocketChannel.open();
        c.connect(s.getLocalAddress()).get(5, TimeUnit.SECONDS);
        AsynchronousSocketChannel srv = s.accept().get(5, TimeUnit.SECONDS);
        ByteBuffer[] outs = { ByteBuffer.wrap("abc".getBytes()), ByteBuffer.wrap("defgh".getBytes()), ByteBuffer.wrap("ij".getBytes()) };
        CountDownLatch l = new CountDownLatch(1);
        long[] w = new long[1];
        c.write(outs, 0, 3, 5, TimeUnit.SECONDS, null, new CompletionHandler<Long, Object>() {
            public void completed(Long r, Object o) { w[0] = r; l.countDown(); }
            public void failed(Throwable t, Object o) { w[0] = -1; l.countDown(); }
        });
        l.await(5, TimeUnit.SECONDS);
        ByteBuffer[] ins = { ByteBuffer.allocate(4), ByteBuffer.allocate(4), ByteBuffer.allocate(4) };
        CountDownLatch l2 = new CountDownLatch(1);
        long[] r = new long[1];
        srv.read(ins, 0, 3, 5, TimeUnit.SECONDS, null, new CompletionHandler<Long, Object>() {
            public void completed(Long n, Object o) { r[0] = n; l2.countDown(); }
            public void failed(Throwable t, Object o) { r[0] = -1; l2.countDown(); }
        });
        l2.await(5, TimeUnit.SECONDS);
        StringBuilder sb = new StringBuilder();
        for (ByteBuffer b : ins) { b.flip(); sb.append('[').append(new String(b.array(), 0, b.limit())).append(']'); }
        System.out.println("gather w=" + w[0] + " r=" + r[0] + " " + sb);
        c.close(); srv.close(); s.close();
    }

    static void files() throws Exception {
        Path p = Files.createTempFile("wasf", ".bin");
        byte[] data = new byte[200_000];
        for (int i = 0; i < data.length; i++) data[i] = (byte) (i * 7);
        try (AsynchronousFileChannel f = AsynchronousFileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            System.out.println("w " + f.write(ByteBuffer.wrap(data), 0).get(10, TimeUnit.SECONDS));
            ByteBuffer b = ByteBuffer.allocate(data.length);
            int n = 0;
            while (b.hasRemaining()) { int r = f.read(b, b.position()).get(10, TimeUnit.SECONDS); if (r < 0) break; n += r; }
            System.out.println("r " + n + " same=" + Arrays.equals(b.array(), data));
            System.out.println("past-eof " + f.read(ByteBuffer.allocate(8), 300_000).get(10, TimeUnit.SECONDS));
            System.out.println("trunc " + f.truncate(100).size());
            FileLock a = f.lock(0, 50, false).get(10, TimeUnit.SECONDS);
            try (AsynchronousFileChannel g = AsynchronousFileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
                FileLock t = null;
                try { t = g.tryLock(10, 10, false); } catch (Throwable x) { System.out.println("trylock " + ex(x)); }
                System.out.println("trylock-null " + (t == null));
                try { f.lock(0, 50, false).get(1, TimeUnit.SECONDS); } catch (Throwable x) { System.out.println("overlap-lock " + ex(x)); }
            }
            a.release();
            System.out.println("released " + a.isValid());
        }
        Files.deleteIfExists(p);
    }

    static void groups() throws Exception {
        ExecutorService ex = Executors.newFixedThreadPool(2);
        AsynchronousChannelGroup g = AsynchronousChannelGroup.withThreadPool(ex);
        AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open(g)
                .bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
        Future<AsynchronousSocketChannel> pend = s.accept();
        g.shutdown();
        System.out.println("shutdown " + g.isShutdown() + " terminated " + g.isTerminated());
        try { AsynchronousSocketChannel.open(g); } catch (Throwable t) { System.out.println("open-after-shutdown " + t.getClass().getName()); }
        s.close();
        System.out.println("awaited " + g.awaitTermination(10, TimeUnit.SECONDS) + " " + g.isTerminated());
        try { pend.get(2, TimeUnit.SECONDS); } catch (Throwable t) { System.out.println("pend " + ex(t)); }
        ex.shutdownNow();
    }

    public static void main(String[] a) throws Exception {
        echoFutures();
        pendingClose();
        timeoutAndHalfClose();
        gather();
        files();
        groups();
        System.exit(0);
    }
}
