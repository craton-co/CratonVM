// SPDX-License-Identifier: Apache-2.0
// nio20 census probe for `jdk-nio-channels`: a Selector-driven echo server, many concurrent
// blocking and non-blocking clients, interrupt and close paths, and an fd-leak check.
// Output is deterministic (no ports, timings or fd numbers): diff against HotSpot 25.
//   java -cp out Nio20EchoStress
//   cratonvm --compatible [CRATONVM_COMPAT_ALIGN=+jdk-nio-channels] -cp out Nio20EchoStress
import java.io.IOException;
import java.net.InetSocketAddress;
import java.net.StandardSocketOptions;
import java.nio.ByteBuffer;
import java.nio.channels.*;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicLong;

public class Nio20EchoStress {
    static final int CLIENTS = Integer.getInteger("nio20.clients", 32);
    static final int ROUNDS = Integer.getInteger("nio20.rounds", 50);

    static long fdCount() {
        try (var s = Files.list(Path.of("/proc/self/fd"))) { return s.count(); }
        catch (IOException e) { return -1; }
    }

    public static void main(String[] a) throws Exception {
        long fd0 = fdCount();
        for (int pass = 0; pass < 2; pass++) {
            String r = echoPass(pass == 1);
            System.out.println("echo-" + (pass == 1 ? "nb" : "blocking") + " " + r);
        }
        System.out.println("interrupt " + interruptPass());
        System.out.println("close-wakes " + closeWakesPass());
        System.out.println("selector-churn " + selectorChurn());
        System.gc();
        Thread.sleep(200);
        long fd1 = fdCount();
        System.out.println("fd-leak " + (fd0 < 0 ? "n/a" : (fd1 - fd0 <= 2 ? "none" : "LEAK+" + (fd1 - fd0))));
        System.out.println("done");
        System.exit(0);
    }

    // A selector echo server on one thread; CLIENTS clients each send ROUNDS messages.
    static String echoPass(boolean nonBlockingClients) throws Exception {
        Selector sel = Selector.open();
        ServerSocketChannel ssc = ServerSocketChannel.open();
        ssc.bind(new InetSocketAddress("127.0.0.1", 0));
        ssc.configureBlocking(false);
        ssc.register(sel, SelectionKey.OP_ACCEPT);
        int port = ((InetSocketAddress) ssc.getLocalAddress()).getPort();
        AtomicLong served = new AtomicLong();
        Thread srv = new Thread(() -> {
            try {
                while (sel.isOpen()) {
                    sel.select(200);
                    if (!sel.isOpen()) break;
                    Iterator<SelectionKey> it = sel.selectedKeys().iterator();
                    while (it.hasNext()) {
                        SelectionKey k = it.next();
                        it.remove();
                        if (!k.isValid()) continue;
                        if (k.isAcceptable()) {
                            SocketChannel c = ssc.accept();
                            if (c == null) continue;
                            c.configureBlocking(false);
                            c.setOption(StandardSocketOptions.TCP_NODELAY, true);
                            c.register(sel, SelectionKey.OP_READ, ByteBuffer.allocate(256));
                        } else if (k.isReadable()) {
                            SocketChannel c = (SocketChannel) k.channel();
                            ByteBuffer b = (ByteBuffer) k.attachment();
                            int n;
                            try { n = c.read(b); } catch (IOException e) { k.cancel(); c.close(); continue; }
                            if (n < 0) { k.cancel(); c.close(); continue; }
                            b.flip();
                            while (b.hasRemaining()) c.write(b);
                            b.clear();
                            served.addAndGet(n);
                        }
                    }
                }
            } catch (ClosedSelectorException e) {
            } catch (IOException e) {
                System.out.println("server-error " + e.getClass().getName());
            }
        });
        srv.setDaemon(true);
        srv.start();
        ExecutorService pool = Executors.newFixedThreadPool(8);
        List<Future<Long>> fs = new ArrayList<>();
        for (int i = 0; i < CLIENTS; i++) {
            final int id = i;
            fs.add(pool.submit(() -> client(port, id, nonBlockingClients)));
        }
        long sum = 0; int bad = 0;
        for (Future<Long> f : fs) {
            try { sum += f.get(60, TimeUnit.SECONDS); } catch (Exception e) { bad++; }
        }
        pool.shutdown();
        pool.awaitTermination(10, TimeUnit.SECONDS);
        long expect = 0;
        for (int i = 0; i < CLIENTS; i++) for (int r = 0; r < ROUNDS; r++) expect += msg(i, r).length;
        sel.close();
        ssc.close();
        srv.join(5000);
        return (sum == expect ? "ok" : "MISMATCH") + ",bad=" + bad + ",keysAfterClose=" + !ssc.isOpen();
    }

    static byte[] msg(int id, int r) { return ("c" + id + "-r" + r + "-payload").getBytes(); }

    static long client(int port, int id, boolean nb) throws IOException {
        try (SocketChannel c = SocketChannel.open()) {
            if (nb) {
                c.configureBlocking(false);
                if (!c.connect(new InetSocketAddress("127.0.0.1", port))) {
                    while (!c.finishConnect()) Thread.onSpinWait();
                }
            } else {
                c.connect(new InetSocketAddress("127.0.0.1", port));
            }
            long total = 0;
            for (int r = 0; r < ROUNDS; r++) {
                byte[] m = msg(id, r);
                ByteBuffer out = ByteBuffer.wrap(m);
                while (out.hasRemaining()) c.write(out);
                ByteBuffer in = ByteBuffer.allocate(m.length);
                while (in.hasRemaining()) {
                    int n = c.read(in);
                    if (n < 0) throw new IOException("eof");
                    if (n == 0) Thread.onSpinWait();
                }
                if (!Arrays.equals(in.array(), m)) throw new IOException("corrupt");
                total += m.length;
            }
            return total;
        }
    }

    // A thread blocked in SocketChannel.read is interrupted: ClosedByInterruptException, channel closed.
    static String interruptPass() throws Exception {
        try (ServerSocketChannel ssc = ServerSocketChannel.open()) {
            ssc.bind(new InetSocketAddress("127.0.0.1", 0));
            SocketChannel c = SocketChannel.open(ssc.getLocalAddress());
            SocketChannel peer = ssc.accept();
            String[] got = new String[1];
            Thread t = new Thread(() -> {
                try { c.read(ByteBuffer.allocate(8)); got[0] = "returned"; }
                catch (Exception e) { got[0] = e.getClass().getSimpleName(); }
            });
            t.start();
            Thread.sleep(300);
            t.interrupt();
            t.join(5000);
            String r = got[0] + "," + c.isOpen() + "," + t.isAlive();
            peer.close();
            c.close();
            return r;
        }
    }

    // Closing a channel from another thread wakes a blocked reader with AsynchronousCloseException.
    static String closeWakesPass() throws Exception {
        try (ServerSocketChannel ssc = ServerSocketChannel.open()) {
            ssc.bind(new InetSocketAddress("127.0.0.1", 0));
            SocketChannel c = SocketChannel.open(ssc.getLocalAddress());
            SocketChannel peer = ssc.accept();
            String[] got = new String[1];
            Thread t = new Thread(() -> {
                try { c.read(ByteBuffer.allocate(8)); got[0] = "returned"; }
                catch (Exception e) { got[0] = e.getClass().getSimpleName(); }
            });
            t.start();
            Thread.sleep(300);
            c.close();
            t.join(5000);
            peer.close();
            return got[0] + "," + t.isAlive();
        }
    }

    // Open/register/cancel/close many selectors and channels; wakeup from another thread.
    static String selectorChurn() throws Exception {
        int woke = 0;
        for (int i = 0; i < 64; i++) {
            Selector s = Selector.open();
            SocketChannel c = SocketChannel.open();
            c.configureBlocking(false);
            SelectionKey k = c.register(s, SelectionKey.OP_CONNECT);
            Thread w = new Thread(s::wakeup);
            w.start();
            s.select(2000);
            w.join();
            woke++;
            k.cancel();
            s.selectNow();
            c.close();
            s.close();
        }
        return "woke=" + woke;
    }
}
