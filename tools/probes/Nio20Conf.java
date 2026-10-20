import java.io.*;
import java.net.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.file.*;
import java.util.*;

/**
 * NIO conformance facts, round nio20 wave 1 (lane L5): the gaps NioConf19 does not reach.
 * Every line is a fact; no timings; diff against HotSpot 25 on Linux.
 * -Dnio20.rows=a,b selects rows.
 *
 * gather    gathering/scattering I/O with more than 16 buffers (IOUtil.iovMax: HotSpot
 *           answers sysconf(_SC_IOV_MAX) = 1024 on Linux, so one write takes every buffer)
 * interrupt ClosedByInterruptException on file, socket and pipe channels
 * wakeup    Selector.wakeup / selectNow consumption (IOUtil.drain on the wakeup fd)
 * append    APPEND-mode FileChannel position / write / positional write / transferTo
 * trunc     truncate / position / size edge cases, scattering read across EOF
 * stdfd     a FileChannel over FileDescriptor.err (fd 2)
 *
 * wave 2 (lane L4):
 * stdfd2     FileChannels over FileDescriptor.out (fd 1) and .in (fd 0): out writes its bytes
 *            (the "nio20-stdout-bytes" line is part of the expected output)
 * nioerr     libnio failure exceptions: class + bare strerror message (connect refused,
 *            peer reset on read / write for SocketChannel and Socket, bind in use,
 *            read0 on a directory = EISDIR)
 * unsafewide Unsafe / VarHandle int- and long-width access over a primitive array of another
 *            element type (plain, volatile, CAS, getAndAdd): raw little-endian bytes
 *
 * wave 3 (lane L5):
 * optfail    a setsockopt the kernel refuses (TCP_KEEPIDLE / KEEPINTERVAL / KEEPCOUNT above
 *            the Linux maxima, EINVAL) through SocketChannel and Socket: jdk.net's handleError
 *            is SocketException(strerror), not an IOException
 * isother    FileDispatcher.isOther0 (FileChannelImpl.isOther, reflective): a regular file and
 *            a directory are not "other", /dev/null is (Linux; prints n/a elsewhere). Needs
 *            --add-opens java.base/sun.nio.ch=ALL-UNNAMED (prints a line saying so without it)
 * stdhandle  FileDescriptor.in/out/err `handle` (-1 on Unix, the console handles on Windows)
 *            and a fresh FileDescriptor's fd/handle. Needs --add-opens java.base/java.io=ALL-UNNAMED
 */
public class Nio20Conf {
    static String rows = System.getProperty("nio20.rows", "all");
    static boolean on(String r) { return rows.equals("all") || Arrays.asList(rows.split(",")).contains(r); }

    static String ex(Throwable t) { return t.getClass().getName() + ":" + t.getMessage(); }
    interface Fact { Object get() throws Throwable; }
    static String f(Fact x) { try { return String.valueOf(x.get()); } catch (Throwable t) { return "!" + ex(t); } }

    public static void main(String[] a) throws Throwable {
        String[] all = {"gather", "interrupt", "wakeup", "append", "trunc", "stdfd", "stdfd2", "nioerr", "unsafewide", "optfail", "isother", "stdhandle"};
        // Every row runs on its own daemon thread with a bound, so a row that blocks
        // prints "<row> TIMEOUT" and the probe still finishes (and the JVM still exits).
        long boundMs = Long.getLong("nio20.rowTimeoutMs", 20_000L);
        for (String r : all) {
            if (!on(r)) continue;
            Thread t = new Thread(() -> {
                try {
                    Nio20Conf.class.getDeclaredMethod(r).invoke(null);
                } catch (java.lang.reflect.InvocationTargetException e) {
                    System.out.println(r + " THREW " + ex(e.getCause()));
                } catch (Throwable e) {
                    System.out.println(r + " THREW " + ex(e));
                }
            }, "nio20-" + r);
            t.setDaemon(true);
            t.start();
            t.join(boundMs);
            if (t.isAlive()) System.out.println(r + " TIMEOUT");
        }
        System.out.println("done");
        System.out.flush();
        System.exit(0);
    }

    static ByteBuffer[] bufs(int n, int each, boolean direct) {
        ByteBuffer[] bs = new ByteBuffer[n];
        for (int i = 0; i < n; i++) {
            bs[i] = direct ? ByteBuffer.allocateDirect(each) : ByteBuffer.allocate(each);
            for (int j = 0; j < each; j++) bs[i].put((byte) ('a' + (i % 26)));
            bs[i].flip();
        }
        return bs;
    }

    static ByteBuffer[] empties(int n, int each) {
        ByteBuffer[] bs = new ByteBuffer[n];
        for (int i = 0; i < n; i++) bs[i] = ByteBuffer.allocate(each);
        return bs;
    }

    static void gather() throws Exception {
        Path p = Files.createTempFile("nio20g", ".bin");
        try (FileChannel fc = FileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            long w20 = fc.write(bufs(20, 3, false));
            long w40d = fc.write(bufs(40, 2, true));
            long w17 = fc.write(bufs(17, 1, false), 0, 17);
            fc.position(0);
            ByteBuffer[] in = empties(30, 5);
            long r30 = fc.read(in);
            int lastFull = 0;
            for (int i = 0; i < in.length; i++) if (in[i].position() > 0) lastFull = i;
            System.out.println("gather-file " + w20 + "," + w40d + "," + w17 + "," + fc.size() + "," + r30 + "," + lastFull + "," + (char) in[19].get(0) + "," + fc.position());
        }
        Files.delete(p);
        try (ServerSocketChannel ss = ServerSocketChannel.open()) {
            ss.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            SocketChannel c = SocketChannel.open(ss.getLocalAddress());
            SocketChannel s = ss.accept();
            long sw = c.write(bufs(24, 4, false));
            c.shutdownOutput();
            // 40 x 3 = 120 bytes of room for the 96 sent, so the loop always ends at EOF
            // (a read into buffers with nothing remaining answers 0 forever).
            ByteBuffer[] in = empties(40, 3);
            long got = 0, n;
            while ((n = s.read(in)) >= 0) got += n;
            System.out.println("gather-sock " + sw + "," + got + "," + (char) in[31].get(0));
            c.close();
            s.close();
        }
        Pipe pipe = Pipe.open();
        long pw = pipe.sink().write(bufs(18, 2, false));
        ByteBuffer pin = ByteBuffer.allocate(64);
        int pr = pipe.source().read(pin);
        System.out.println("gather-pipe " + pw + "," + pr);
        pipe.sink().close();
        pipe.source().close();
    }

    static void interrupt() throws Exception {
        Path p = Files.createTempFile("nio20i", ".bin");
        Files.write(p, new byte[100]);
        FileChannel fc = FileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE);
        Thread.currentThread().interrupt();
        String r = f(() -> fc.read(ByteBuffer.allocate(4)));
        boolean still = Thread.interrupted();
        System.out.println("interrupt-file " + r + "," + still + "," + fc.isOpen() + "," + f(() -> fc.size()));
        FileChannel fc2 = FileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE);
        Thread.currentThread().interrupt();
        String w = f(() -> fc2.write(ByteBuffer.allocate(4), 0));
        System.out.println("interrupt-pwrite " + w + "," + Thread.interrupted() + "," + fc2.isOpen());
        FileChannel fc3 = FileChannel.open(p, StandardOpenOption.READ);
        Thread.currentThread().interrupt();
        String pos = f(() -> fc3.position());
        System.out.println("interrupt-position " + pos + "," + Thread.interrupted() + "," + fc3.isOpen());
        fc3.close();
        Files.delete(p);
        try (ServerSocketChannel ss = ServerSocketChannel.open()) {
            ss.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            SocketChannel c = SocketChannel.open(ss.getLocalAddress());
            SocketChannel s = ss.accept();
            Thread.currentThread().interrupt();
            String sr = f(() -> c.read(ByteBuffer.allocate(4)));
            System.out.println("interrupt-sock " + sr + "," + Thread.interrupted() + "," + c.isOpen() + "," + s.read(ByteBuffer.allocate(4)));
            s.close();
        }
        Pipe pipe = Pipe.open();
        Thread.currentThread().interrupt();
        String pr = f(() -> pipe.source().read(ByteBuffer.allocate(4)));
        System.out.println("interrupt-pipe " + pr + "," + Thread.interrupted() + "," + pipe.source().isOpen() + "," + pipe.sink().isOpen());
        pipe.sink().close();
        // A thread blocked in a read and then interrupted.
        try (ServerSocketChannel ss = ServerSocketChannel.open()) {
            ss.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            SocketChannel c = SocketChannel.open(ss.getLocalAddress());
            SocketChannel s = ss.accept();
            String[] out = new String[2];
            Thread t = new Thread(() -> {
                out[0] = f(() -> c.read(ByteBuffer.allocate(4)));
                out[1] = String.valueOf(Thread.currentThread().isInterrupted());
            });
            t.setDaemon(true);
            t.start();
            Thread.sleep(200);
            t.interrupt();
            t.join(5_000);
            System.out.println("interrupt-blocked " + out[0] + "," + out[1] + "," + c.isOpen() + "," + t.isAlive());
            s.close();
        }
    }

    static void wakeup() throws Exception {
        try (Selector sel = Selector.open()) {
            sel.wakeup();
            int a = sel.select();
            sel.wakeup();
            sel.wakeup();
            int b = sel.selectNow();
            int c = sel.select(50);
            sel.wakeup();
            int d = sel.select(10_000);
            int e = sel.selectNow();
            System.out.println("wakeup " + a + "," + b + "," + c + "," + d + "," + e);
        }
    }

    static void append() throws Exception {
        Path p = Files.createTempFile("nio20a", ".bin");
        Files.write(p, "0123456789".getBytes());
        try (FileChannel fc = FileChannel.open(p, StandardOpenOption.WRITE, StandardOpenOption.APPEND)) {
            long p0 = fc.position();
            int w = fc.write(ByteBuffer.wrap("AB".getBytes()));
            long p1 = fc.position();
            fc.position(0);
            int w2 = fc.write(ByteBuffer.wrap("CD".getBytes()));
            long p2 = fc.position();
            System.out.println("append " + p0 + "," + w + "," + p1 + "," + w2 + "," + p2 + "," + fc.size()
                    + "," + f(() -> fc.read(ByteBuffer.allocate(1))) + "," + f(() -> fc.lock(0, 1, true)));
        }
        Path src = Files.createTempFile("nio20s", ".bin");
        Files.write(src, "xyz".getBytes());
        try (FileChannel in = FileChannel.open(src); FileChannel out = FileChannel.open(p, StandardOpenOption.WRITE, StandardOpenOption.APPEND)) {
            long t = in.transferTo(0, 3, out);
            long tf = out.transferFrom(in, 0, 3);
            System.out.println("append-transfer " + t + "," + tf + "," + out.size() + "," + out.position());
        }
        System.out.println("append-disk " + new String(Files.readAllBytes(p)));
        Files.delete(src);
        Files.delete(p);
    }

    static void trunc() throws Exception {
        Path p = Files.createTempFile("nio20t", ".bin");
        try (FileChannel fc = FileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            fc.write(ByteBuffer.wrap(new byte[20]));
            fc.position(30);
            FileChannel t1 = fc.truncate(100);
            long a = fc.position(), sz = fc.size();
            fc.truncate(10);
            long b = fc.position();
            fc.position(5);
            fc.truncate(8);
            long c = fc.position();
            fc.position(Long.MAX_VALUE);
            int eof = fc.read(ByteBuffer.allocate(1));
            fc.position(4);
            ByteBuffer[] in = empties(3, 3);
            long sr = fc.read(in);
            long sr2 = fc.read(in);
            int z = fc.read(ByteBuffer.allocate(0));
            System.out.println("trunc " + (t1 == fc) + "," + a + "," + sz + "," + b + "," + c + "," + eof + "," + sr + "," + in[1].position() + "," + in[2].position() + "," + sr2 + "," + z
                    + "," + f(() -> fc.truncate(-1)) + "," + f(() -> fc.write(ByteBuffer.allocate(1), -1)) + "," + f(() -> fc.position(Long.MIN_VALUE)));
            fc.position(1_000_000);
            int w = fc.write(ByteBuffer.wrap("Z".getBytes()));
            System.out.println("trunc-hole " + w + "," + fc.size() + "," + fc.position() + "," + f(() -> { ByteBuffer one = ByteBuffer.allocate(1); fc.read(one, 500_000); return one.get(0); }));
        }
        Files.delete(p);
    }

    static void stdfd() throws Exception {
        FileOutputStream err = new FileOutputStream(FileDescriptor.err);
        FileChannel ch = err.getChannel();
        String w = f(() -> ch.write(ByteBuffer.wrap("nio20-stderr\n".getBytes())));
        System.out.println("stdfd " + w + "," + ch.isOpen());
    }

    // Never closed: closing these channels would close the standard streams.
    static void stdfd2() throws Exception {
        System.out.flush();
        FileChannel out = new FileOutputStream(FileDescriptor.out).getChannel();
        String wo = f(() -> out.write(ByteBuffer.wrap("nio20-stdout-bytes\n".getBytes())));
        System.out.println("stdfd-out " + wo + "," + out.isOpen());
        FileChannel in = new FileInputStream(FileDescriptor.in).getChannel();
        // A zero-remaining read answers 0 without a native call on HotSpot; the channel
        // itself must exist and be open (it used to be refused at the descriptor).
        System.out.println("stdfd-in " + in.isOpen() + "," + f(() -> in.read(ByteBuffer.allocate(0))));
    }

    static void nioerr() throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();
        int dead;
        try (ServerSocket tmp = new ServerSocket(0, 1, lo)) { dead = tmp.getLocalPort(); }
        System.out.println("nioerr-refused-ch " + f(() -> { SocketChannel.open(new InetSocketAddress(lo, dead)).close(); return "connected"; }));
        System.out.println("nioerr-refused-sock " + f(() -> { new Socket(lo, dead).close(); return "connected"; }));
        try (ServerSocketChannel a1 = ServerSocketChannel.open()) {
            a1.bind(new InetSocketAddress(lo, 0));
            System.out.println("nioerr-bind " + f(() -> {
                try (ServerSocketChannel b = ServerSocketChannel.open()) { b.bind(a1.getLocalAddress()); return "bound"; }
            }));
        }
        // Peer reset (SO_LINGER 0 close) seen by a SocketChannel read, then a write on another pair.
        try (ServerSocketChannel ss = ServerSocketChannel.open()) {
            ss.bind(new InetSocketAddress(lo, 0));
            SocketChannel c = SocketChannel.open(ss.getLocalAddress());
            SocketChannel s = ss.accept();
            c.setOption(StandardSocketOptions.SO_LINGER, 0);
            c.close();
            Thread.sleep(200);
            System.out.println("nioerr-reset-read " + f(() -> s.read(ByteBuffer.allocate(4))) + " | " + f(() -> s.read(ByteBuffer.allocate(4))));
            s.close();
            SocketChannel c2 = SocketChannel.open(ss.getLocalAddress());
            SocketChannel s2 = ss.accept();
            c2.setOption(StandardSocketOptions.SO_LINGER, 0);
            c2.close();
            Thread.sleep(200);
            System.out.println("nioerr-reset-write " + f(() -> { int n = 0; for (int i = 0; i < 4; i++) n += s2.write(ByteBuffer.wrap(new byte[8])); return n; }));
            s2.close();
        }
        // The same through java.net.Socket (NioSocketImpl).
        try (ServerSocket ss = new ServerSocket(0, 1, lo)) {
            Socket c = new Socket(lo, ss.getLocalPort());
            Socket s = ss.accept();
            c.setSoLinger(true, 0);
            c.close();
            Thread.sleep(200);
            System.out.println("nioerr-reset-sock " + f(() -> s.getInputStream().read()) + " | " + f(() -> s.getInputStream().read()));
            s.close();
        }
        // read0 on a directory descriptor: EISDIR, libnio's IOException("Is a directory").
        Path dir = Files.createTempDirectory("nio20d");
        try (FileChannel fc = FileChannel.open(dir, StandardOpenOption.READ)) {
            System.out.println("nioerr-eisdir " + f(() -> fc.read(ByteBuffer.allocate(8))) + " | " + f(() -> fc.read(ByteBuffer.allocateDirect(8), 0)));
        } catch (IOException e) {
            System.out.println("nioerr-eisdir open " + ex(e));
        }
        Files.delete(dir);
    }

    static void unsafewide() throws Exception {
        java.lang.reflect.Field tu = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        tu.setAccessible(true);
        sun.misc.Unsafe u = (sun.misc.Unsafe) tu.get(null);
        long bB = u.arrayBaseOffset(byte[].class), iB = u.arrayBaseOffset(int[].class);
        long lB = u.arrayBaseOffset(long[].class), fB = u.arrayBaseOffset(float[].class);
        long dB = u.arrayBaseOffset(double[].class);
        byte[] b = new byte[16];
        for (int i = 0; i < 16; i++) b[i] = (byte) (i + 1);
        int[] ia = {0x11223344, 0x55667788, 0x0a0b0c0d};
        long[] la = new long[2];
        // Plain accessors (already raw before wave 2) as the control.
        String plain = Long.toHexString(u.getLong(ia, iB + 4)) + "," + Integer.toHexString(u.getInt(b, bB + 3));
        u.putInt(la, lB + 4, 0x12345678);
        plain += "," + Long.toHexString(la[0]);
        // Volatile and CAS over a mismatched element type.
        String vol = Long.toHexString(u.getLongVolatile(b, bB)) + "," + Integer.toHexString(u.getIntVolatile(la, lB + 4));
        u.putLongVolatile(b, bB + 8, 0x0102030405060708L);
        vol += "," + b[8] + "," + b[15];
        u.putIntVolatile(la, lB + 8, -1);
        vol += "," + Long.toHexString(la[1]);
        String cas = u.compareAndSwapLong(b, bB, 0L, 1L) + "," + u.compareAndSwapLong(b, bB, u.getLong(b, bB), 0x7fL)
                + "," + b[0] + "," + b[1] + "," + u.getAndAddInt(la, lB + 4, 1) + "," + u.getAndAddLong(b, bB, 2L) + "," + b[0];
        float[] fa = {1.5f, 2.5f};
        double[] da = {0.25, 0.5};
        cas += "," + u.compareAndSwapInt(fa, fB + 4, Float.floatToRawIntBits(2.5f), Float.floatToRawIntBits(7.5f)) + "," + fa[1]
                + "," + u.compareAndSwapLong(da, dB, Double.doubleToRawLongBits(0.25), Double.doubleToRawLongBits(9.0)) + "," + da[0];
        System.out.println("unsafewide-plain " + plain);
        System.out.println("unsafewide-volatile " + vol);
        System.out.println("unsafewide-cas " + cas);
        // The VarHandle shapes that reach these natives without sun.misc.Unsafe.
        java.lang.invoke.VarHandle le = java.lang.invoke.MethodHandles.byteArrayViewVarHandle(long[].class, ByteOrder.LITTLE_ENDIAN);
        java.lang.invoke.VarHandle be = java.lang.invoke.MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
        byte[] v = new byte[16];
        le.setVolatile(v, 8, 0x1122334455667788L);
        String vh = Long.toHexString((long) le.getVolatile(v, 8)) + "," + v[8] + "," + v[15]
                + "," + (boolean) le.compareAndSet(v, 8, 0x1122334455667788L, 5L) + "," + v[8]
                + "," + (long) le.getAndAdd(v, 8, 3L) + "," + (long) le.getAcquire(v, 8)
                + "," + (boolean) be.compareAndSet(v, 0, 0, 0x01020304) + "," + v[0] + "," + v[3]
                + "," + (int) be.getVolatile(v, 0);
        java.lang.invoke.VarHandle fv = java.lang.invoke.MethodHandles.arrayElementVarHandle(float[].class);
        java.lang.invoke.VarHandle dv = java.lang.invoke.MethodHandles.arrayElementVarHandle(double[].class);
        vh += "," + (boolean) fv.compareAndSet(fa, 0, 1.5f, 3.0f) + "," + fa[0] + "," + (float) fv.getAndAdd(fa, 0, 1.0f) + "," + fa[0]
                + "," + (boolean) dv.compareAndSet(da, 1, 0.5, 4.0) + "," + da[1];
        ByteBuffer hb = ByteBuffer.allocate(16).order(ByteOrder.LITTLE_ENDIAN);
        java.lang.invoke.VarHandle bbv = java.lang.invoke.MethodHandles.byteBufferViewVarHandle(long[].class, ByteOrder.LITTLE_ENDIAN);
        bbv.setVolatile(hb, 8, 42L);
        vh += "," + (long) bbv.getAndAdd(hb, 8, 1L) + "," + hb.getLong(8) + "," + (boolean) bbv.compareAndSet(hb, 8, 43L, 44L) + "," + hb.get(8);
        System.out.println("unsafewide-varhandle " + vh);
    }

    static void optfail() throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();
        try (ServerSocketChannel ss = ServerSocketChannel.open()) {
            ss.bind(new InetSocketAddress(lo, 0));
            try (SocketChannel c = SocketChannel.open(ss.getLocalAddress()); SocketChannel s = ss.accept()) {
                System.out.println("optfail-ch " + f(() -> { c.setOption(jdk.net.ExtendedSocketOptions.TCP_KEEPIDLE, 40_000); return "ok"; })
                        + " | " + f(() -> { c.setOption(jdk.net.ExtendedSocketOptions.TCP_KEEPINTERVAL, 40_000); return "ok"; })
                        + " | " + f(() -> { c.setOption(jdk.net.ExtendedSocketOptions.TCP_KEEPCOUNT, 200); return "ok"; })
                        + " | " + f(() -> { c.setOption(jdk.net.ExtendedSocketOptions.TCP_KEEPIDLE, 60); return "ok"; })
                        + " | " + f(() -> c.getOption(jdk.net.ExtendedSocketOptions.TCP_KEEPIDLE)));
            }
        }
        try (ServerSocket ss = new ServerSocket(0, 1, lo); Socket c = new Socket(lo, ss.getLocalPort()); Socket s = ss.accept()) {
            System.out.println("optfail-sock " + f(() -> { c.setOption(jdk.net.ExtendedSocketOptions.TCP_KEEPIDLE, 40_000); return "ok"; })
                    + " | " + f(() -> { c.setOption(jdk.net.ExtendedSocketOptions.TCP_KEEPCOUNT, 200); return "ok"; })
                    + " | " + f(() -> { c.setOption(jdk.net.ExtendedSocketOptions.TCP_KEEPCOUNT, 5); return "ok"; })
                    + " | " + f(() -> c.getOption(jdk.net.ExtendedSocketOptions.TCP_KEEPCOUNT)));
        }
    }

    static void isother() throws Exception {
        java.lang.reflect.Method m;
        try {
            m = Class.forName("sun.nio.ch.FileChannelImpl").getDeclaredMethod("isOther");
            m.setAccessible(true);
        } catch (RuntimeException e) {
            System.out.println("isother needs --add-opens java.base/sun.nio.ch=ALL-UNNAMED");
            return;
        }
        java.lang.reflect.Method im = m;
        Path p = Files.createTempFile("nio20o", ".bin");
        try (FileChannel fc = FileChannel.open(p, StandardOpenOption.READ)) {
            System.out.println("isother-file " + f(() -> im.invoke(fc)));
        }
        Files.delete(p);
        Path dir = Files.createTempDirectory("nio20od");
        try (FileChannel fc = FileChannel.open(dir, StandardOpenOption.READ)) {
            System.out.println("isother-dir " + f(() -> im.invoke(fc)));
        } catch (IOException e) {
            System.out.println("isother-dir open " + ex(e));
        }
        Files.delete(dir);
        Path devnull = Paths.get("/dev/null");
        if (Files.exists(devnull)) {
            try (FileChannel fc = FileChannel.open(devnull, StandardOpenOption.READ)) {
                System.out.println("isother-devnull " + f(() -> im.invoke(fc)) + "," + f(() -> fc.position()) + "," + f(() -> fc.size()));
            }
        } else {
            System.out.println("isother-devnull n/a");
        }
    }

    static void stdhandle() throws Exception {
        java.lang.reflect.Field fdf, hf;
        try {
            fdf = FileDescriptor.class.getDeclaredField("fd");
            hf = FileDescriptor.class.getDeclaredField("handle");
            fdf.setAccessible(true);
            hf.setAccessible(true);
        } catch (RuntimeException e) {
            System.out.println("stdhandle needs --add-opens java.base/java.io=ALL-UNNAMED");
            return;
        }
        boolean windows = System.getProperty("os.name").startsWith("Windows");
        StringBuilder sb = new StringBuilder("stdhandle");
        for (FileDescriptor d : new FileDescriptor[] {FileDescriptor.in, FileDescriptor.out, FileDescriptor.err}) {
            long h = hf.getLong(d);
            // Windows' console handles are host values: print only whether one is present.
            sb.append(' ').append(fdf.getInt(d)).append('/').append(windows ? (h != -1 ? "handle" : "-1") : String.valueOf(h));
        }
        FileDescriptor fresh = new FileDescriptor();
        sb.append(" fresh ").append(fdf.getInt(fresh)).append('/').append(hf.getLong(fresh)).append(',').append(fresh.valid());
        System.out.println(sb);
    }
}
