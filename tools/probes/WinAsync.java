import java.net.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.file.*;
import java.util.concurrent.*;

/**
 * Windows asynchronous channels (IOCP): socket, server, file and group rows. Run in both modes against
 * HotSpot; what the JDK's own `sun.nio.ch.Iocp` / `WindowsAsynchronous*ChannelImpl` do is what is compared:
 * the classes, the completion thread and its stack, the refusal text, argument checks, group shutdown.
 */
public class WinAsync {
    static void part1() throws Exception {
        try {
            AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()
                    .bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            System.out.println("server " + s.getClass().getName());
            Future<AsynchronousSocketChannel> acc = s.accept();
            AsynchronousSocketChannel c = AsynchronousSocketChannel.open();
            System.out.println("client " + c.getClass().getName());
            c.connect(s.getLocalAddress()).get(5, TimeUnit.SECONDS);
            AsynchronousSocketChannel srv = acc.get(5, TimeUnit.SECONDS);
            System.out.println("write " + c.write(ByteBuffer.wrap("hello".getBytes())).get(5, TimeUnit.SECONDS));
            ByteBuffer b = ByteBuffer.allocate(16);
            System.out.println("read " + srv.read(b).get(5, TimeUnit.SECONDS) + " " + new String(b.array(), 0, 5));
            c.close();
            System.out.println("eof " + srv.read(ByteBuffer.allocate(4)).get(5, TimeUnit.SECONDS));
            srv.close();
            s.close();
        } catch (Throwable t) {
            System.out.println("sock ERR " + t);
        }
        try {
            Path p = Files.createTempFile("as", ".bin");
            AsynchronousFileChannel f = AsynchronousFileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE);
            System.out.println("file " + f.getClass().getName());
            System.out.println("fw " + f.write(ByteBuffer.wrap("abcdef".getBytes()), 0).get(5, TimeUnit.SECONDS));
            ByteBuffer b = ByteBuffer.allocate(6);
            System.out.println("fr " + f.read(b, 0).get(5, TimeUnit.SECONDS) + " " + new String(b.array()));
            ByteBuffer e = ByteBuffer.allocate(4);
            System.out.println("eof " + f.read(e, 100).get(5, TimeUnit.SECONDS));
            FileLock l = f.lock(0, 3, false).get(5, TimeUnit.SECONDS);
            System.out.println("lock " + l.isValid() + " " + l.position() + " " + l.size() + " " + l.isShared());
            l.release();
            System.out.println("size " + f.size());
            f.close();
        } catch (Throwable t) {
            System.out.println("file ERR " + t);
        }
    }

    static void part2() throws Exception {
        AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()
                .bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
        CountDownLatch l = new CountDownLatch(1);
        s.accept(null, new CompletionHandler<AsynchronousSocketChannel, Object>() {
            public void completed(AsynchronousSocketChannel r, Object o) {
                StackTraceElement[] st = new Throwable().getStackTrace();
                StringBuilder b = new StringBuilder();
                for (int i = 0; i < Math.min(st.length, 8); i++)
                    b.append(st[i].getClassName()).append('.').append(st[i].getMethodName()).append(';');
                System.out.println("thread=" + Thread.currentThread().getName().replaceAll("[0-9]+", "N")
                        + " daemon=" + Thread.currentThread().isDaemon());
                System.out.println("stack=" + b);
                l.countDown();
            }

            public void failed(Throwable t, Object o) {
                System.out.println("failed " + t);
                l.countDown();
            }
        });
        AsynchronousSocketChannel c = AsynchronousSocketChannel.open();
        c.connect(s.getLocalAddress()).get();
        l.await(5, TimeUnit.SECONDS);
        AsynchronousChannelGroup g = AsynchronousChannelGroup.withFixedThreadPool(2, Executors.defaultThreadFactory());
        System.out.println("group " + g.getClass().getName() + " " + g.isShutdown());
        try {
            AsynchronousSocketChannel x = AsynchronousSocketChannel.open(g);
            x.connect(new InetSocketAddress(InetAddress.getLoopbackAddress(), 1)).get(5, TimeUnit.SECONDS);
        } catch (Throwable t) {
            System.out.println("refused " + t.getClass().getName() + " "
                    + (t.getCause() == null ? null : t.getCause().getClass().getName() + ":" + t.getCause().getMessage()));
        }
        ByteBuffer ro = ByteBuffer.allocate(4).asReadOnlyBuffer();
        try {
            c.read(ro);
        } catch (Throwable t) {
            System.out.println("ro " + t);
        }
        try {
            c.read(ByteBuffer.allocate(4), -1, TimeUnit.SECONDS, null, null);
        } catch (Throwable t) {
            System.out.println("neg " + t);
        }
        g.shutdownNow();
        System.out.println("term " + g.awaitTermination(5, TimeUnit.SECONDS));
    }

    public static void main(String[] x) throws Exception {
        part1();
        part2();
        System.exit(0);
    }
}
