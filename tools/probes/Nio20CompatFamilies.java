import java.io.*;
import java.lang.ref.*;
import java.lang.reflect.*;
import java.net.*;
import java.nio.*;
import java.nio.channels.*;
import java.util.*;

/**
 * nio20 wave 1 lane L4: conformance facts for the `--compatible` families this lane adds
 * (`jdk-references`, `jdk-object-bodies`, `jdk-unsafe-bulk-bodies`, `jdk-optional`,
 * `jdk-nio-channels`) and the `System$1.blockedOn` hook it gives `jdk-pipe`. Every line is a
 * fact (no timings, no hashes, no addresses), so the output diffs against HotSpot 25 as is.
 *
 * Run (the `unsafe` row needs the export on BOTH VMs; javac needs nothing):
 *   javac -d out tools/probes/Nio20CompatFamilies.java
 *   java --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out Nio20CompatFamilies
 *   cratonvm --compatible --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out Nio20CompatFamilies
 * A/B one family: CRATONVM_COMPAT_ALIGN=-jdk-references (or -jdk-object-bodies,
 * -jdk-unsafe-bulk-bodies, -jdk-optional, -jdk-pipe); the channel rows need
 * CRATONVM_COMPAT_ALIGN=+jdk-nio-channels (default off). -Dnio20.rows=a,b selects rows.
 */
public class Nio20CompatFamilies {
    static String rows = System.getProperty("nio20.rows", "all");
    static boolean on(String r) { return rows.equals("all") || Arrays.asList(rows.split(",")).contains(r); }

    static String ex(Throwable t) { return t.getClass().getName() + ":" + t.getMessage(); }
    interface Fact { Object get() throws Throwable; }
    static String f(Fact x) { try { return String.valueOf(x.get()); } catch (Throwable t) { return "!" + ex(t); } }

    public static void main(String[] a) throws Throwable {
        String[] all = {"refs", "refgc", "refwake", "fence", "object", "optional", "unsafe", "pipeintr",
                "sockchan", "selector", "dgram", "sockintr"};
        for (String r : all) {
            if (!on(r)) continue;
            try {
                Nio20CompatFamilies.class.getDeclaredMethod(r).invoke(null);
            } catch (InvocationTargetException e) {
                System.out.println(r + " THREW " + ex(e.getCause()));
            }
        }
        System.out.println("done");
    }

    // ---- jdk-references ---------------------------------------------------------------

    static void refs() throws Exception {
        Object o = new Object();
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        WeakReference<Object> w = new WeakReference<>(o, q);
        SoftReference<Object> s = new SoftReference<>(o);
        PhantomReference<Object> p = new PhantomReference<>(o, q);
        WeakReference<Object> nul = new WeakReference<>(null);
        System.out.println("refs-new " + (w.get() == o) + "," + (s.get() == o) + "," + p.get() + "," + nul.get()
                + "," + w.refersTo(o) + "," + w.refersTo(null) + "," + nul.refersTo(null) + "," + p.refersTo(o)
                + "," + w.isEnqueued() + "," + q.poll());
        boolean e1 = w.enqueue();
        boolean e2 = w.enqueue();
        boolean noQueue = s.enqueue();
        System.out.println("refs-enqueue " + e1 + "," + e2 + "," + noQueue + "," + w.get() + "," + w.isEnqueued()
                + "," + (s.get() == o));
        Reference<?> polled = q.poll();
        System.out.println("refs-poll " + (polled == w) + "," + w.isEnqueued() + "," + q.poll() + "," + w.enqueue());
        Reference<?> again = q.remove(1);
        System.out.println("refs-remove " + (again == w) + "," + q.remove(1) + "," + f(() -> q.remove(-1)));
        s.clear();
        p.clear();
        System.out.println("refs-clear " + s.get() + "," + s.refersTo(null) + "," + p.refersTo(o) + "," + p.enqueue()
                + "," + (q.poll() == p) + "," + q.poll());
        Reference.reachabilityFence(o);
        Reference.reachabilityFence(null);
    }

    static volatile Object sink;

    /** A dropped referent is cleared and its reference enqueued by the collector. */
    static void refgc() throws Exception {
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        WeakReference<Object> w = makeWeak(q);
        Reference<?> got = null;
        for (int i = 0; i < 50 && got == null; i++) {
            System.gc();
            sink = new byte[1 << 16];
            got = q.remove(100);
        }
        System.out.println("refgc " + (got == w) + "," + w.get() + "," + w.refersTo(null) + "," + w.isEnqueued());
    }

    static WeakReference<Object> makeWeak(ReferenceQueue<Object> q) {
        return new WeakReference<>(new Object() { }, q);
    }

    /** An untimed `ReferenceQueue.remove()` is woken by the collector's enqueue (the queue wake-ups). */
    static void refwake() throws Exception {
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        WeakReference<Object> w = makeWeak(q);
        Reference<?>[] got = new Reference<?>[1];
        Thread t = new Thread(() -> {
            try { got[0] = q.remove(); } catch (InterruptedException e) { got[0] = null; }
        });
        t.setDaemon(true);
        t.start();
        long dl = System.currentTimeMillis() + 10000;
        while (t.isAlive() && System.currentTimeMillis() < dl) {
            System.gc();
            sink = new byte[1 << 16];
            t.join(100);
        }
        boolean woken = !t.isAlive();
        if (!woken) t.interrupt();
        System.out.println("refwake " + woken + "," + (got[0] == w));
    }

    static void fence() {
        long n = 0;
        Object o = new Object();
        for (int i = 0; i < 200000; i++) {
            Reference.reachabilityFence(o);
            n += i & 1;
        }
        System.out.println("fence " + n);
    }

    // ---- jdk-object-bodies ------------------------------------------------------------

    static void object() throws Exception {
        Object o = new Object();
        Object lock = new Object();
        String ts = o.toString();
        System.out.println("object " + o.equals(o) + "," + o.equals(new Object()) + "," + o.equals(null)
                + "," + ts.startsWith("java.lang.Object@") + "," + ts.substring(ts.indexOf('@') + 1).equals(Integer.toHexString(o.hashCode()))
                + "," + f(() -> { o.wait(); return "returned"; })
                + "," + f(() -> { o.wait(5); return "returned"; })
                + "," + f(() -> { synchronized (lock) { lock.wait(-1); } return "returned"; })
                + "," + f(() -> { synchronized (lock) { lock.wait(1); } return "returned"; })
                + "," + f(() -> { synchronized (lock) { lock.wait(0, 1_000_000); } return "returned"; }));
        Thread.currentThread().interrupt();
        System.out.println("object-wait-interrupted " + f(() -> { synchronized (lock) { lock.wait(); } return "returned"; })
                + "," + Thread.interrupted());
        int[] arr = new int[2];
        System.out.println("object-array " + arr.toString().startsWith("[I@") + "," + arr.equals(arr.clone()));
    }

    // ---- jdk-optional -----------------------------------------------------------------

    static void optional() {
        Optional<String> e = Optional.empty();
        Optional<String> a = Optional.of("a");
        System.out.println("optional " + (Optional.empty() == Optional.empty()) + "," + (Optional.ofNullable(null) == Optional.empty())
                + "," + (OptionalInt.empty() == OptionalInt.empty()) + "," + (OptionalLong.empty() == OptionalLong.empty())
                + "," + (OptionalDouble.empty() == OptionalDouble.empty())
                + "," + e + "," + a + "," + a.equals(Optional.of("a")) + "," + (a.hashCode() == "a".hashCode()) + "," + e.hashCode()
                + "," + e.isPresent() + "," + e.isEmpty() + "," + a.orElse("z") + "," + e.orElse("z")
                + "," + f(e::get) + "," + f(e::orElseThrow) + "," + f(() -> Optional.of(null))
                + "," + a.map(String::toUpperCase).get() + "," + a.filter(String::isEmpty).isPresent());
        System.out.println("optional-prim " + OptionalInt.of(5) + "," + OptionalInt.empty() + "," + OptionalLong.of(-7) + "," + OptionalDouble.of(1.5)
                + "," + OptionalInt.of(5).getAsInt() + "," + OptionalLong.empty().orElse(7) + "," + OptionalDouble.empty().orElse(2.5)
                + "," + OptionalInt.of(5).equals(OptionalInt.of(5)) + "," + OptionalInt.of(5).hashCode() + "," + f(() -> OptionalInt.empty().getAsInt())
                + "," + OptionalDouble.of(Double.NaN).equals(OptionalDouble.of(Double.NaN)) + "," + OptionalInt.of(3).isPresent());
    }

    // ---- jdk-unsafe-bulk-bodies -------------------------------------------------------

    static void unsafe() throws Exception {
        Class<?> uc;
        Object u;
        try {
            uc = Class.forName("jdk.internal.misc.Unsafe");
            u = uc.getMethod("getUnsafe").invoke(null);
        } catch (Throwable t) {
            System.out.println("unsafe SKIPPED (run with --add-exports java.base/jdk.internal.misc=ALL-UNNAMED) " + t.getClass().getName());
            return;
        }
        Method copy = uc.getMethod("copyMemory", Object.class, long.class, Object.class, long.class, long.class);
        Method set = uc.getMethod("setMemory", Object.class, long.class, long.class, byte.class);
        long bb = (Integer) uc.getMethod("arrayBaseOffset", Class.class).invoke(u, byte[].class);
        long ib = (Integer) uc.getMethod("arrayBaseOffset", Class.class).invoke(u, int[].class);
        byte[] src = {1, 2, 3, 4, 5, 6, 7, 8};
        byte[] dst = new byte[8];
        copy.invoke(u, src, bb + 2, dst, bb + 1, 4L);
        int[] ints = new int[2];
        copy.invoke(u, src, bb, ints, ib, 8L);
        set.invoke(u, dst, bb + 6, 2L, (byte) 9);
        System.out.println("unsafe " + Arrays.toString(dst) + "," + Integer.toHexString(ints[0]) + "," + Integer.toHexString(ints[1])
                + "," + call(copy, u, src, bb, dst, bb, -1L)
                + "," + call(copy, u, new Object(), 16L, dst, bb, 1L)
                + "," + call(copy, u, src, bb, new Object(), 16L, 1L)
                + "," + call(copy, u, src, bb, dst, bb, 0L)
                + "," + call(set, u, dst, bb, -1L, (byte) 0)
                + "," + call(set, u, new Object(), 16L, 1L, (byte) 0)
                + "," + call(set, u, dst, bb, 0L, (byte) 0));
    }

    static String call(Method m, Object recv, Object... args) {
        try {
            m.invoke(recv, args);
            return "ok";
        } catch (InvocationTargetException e) {
            return "!" + ex(e.getCause());
        } catch (Throwable t) {
            return "!!" + ex(t);
        }
    }

    // ---- jdk-pipe + the blockedOn hook ------------------------------------------------

    /** A blocking `Pipe.source().read` interrupted by another thread: HotSpot closes the channel. */
    static void pipeintr() throws Exception {
        Pipe p = Pipe.open();
        String[] got = new String[1];
        Thread t = new Thread(() -> got[0] = f(() -> p.source().read(ByteBuffer.allocate(4))));
        t.setDaemon(true);
        t.start();
        Thread.sleep(300);
        t.interrupt();
        t.join(5000);
        boolean hung = t.isAlive();
        if (hung) p.sink().close();
        t.join(5000);
        System.out.println("pipeintr " + (hung ? "HUNG" : got[0]) + "," + p.source().isOpen() + "," + p.sink().isOpen());
        p.source().close();
        p.sink().close();
    }

    // ---- jdk-nio-channels -------------------------------------------------------------

    static void sockchan() throws Exception {
        InetSocketAddress lo = new InetSocketAddress(InetAddress.getLoopbackAddress(), 0);
        try (ServerSocketChannel ss = ServerSocketChannel.open()) {
            ss.bind(lo);
            SocketChannel fresh = SocketChannel.open();
            String linger = f(() -> fresh.getOption(StandardSocketOptions.SO_LINGER));
            String unconnected = f(() -> fresh.read(ByteBuffer.allocate(1))) + "," + f(() -> fresh.finishConnect());
            fresh.close();
            SocketChannel c = SocketChannel.open(ss.getLocalAddress());
            SocketChannel s = ss.accept();
            c.shutdownOutput();
            System.out.println("sockchan " + linger + "," + unconnected + "," + f(() -> c.write(ByteBuffer.allocate(1)))
                    + "," + f(() -> c.connect(ss.getLocalAddress())) + "," + s.read(ByteBuffer.allocate(1))
                    + "," + f(() -> ss.bind(lo)) + "," + f(() -> c.register(Selector.open(), SelectionKey.OP_READ))
                    + "," + c.getClass().getName() + "," + ss.getClass().getName());
            c.close();
            s.close();
            System.out.println("sockchan-closed " + f(() -> c.getRemoteAddress()) + "," + f(() -> c.getLocalAddress())
                    + "," + f(() -> c.read(ByteBuffer.allocate(1))) + "," + f(() -> c.configureBlocking(false)) + "," + c.isOpen());
        }
        System.out.println("sockchan-ssc-closed " + f(() -> {
            ServerSocketChannel x = ServerSocketChannel.open();
            x.close();
            return x.getLocalAddress();
        }));
    }

    static void selector() throws Exception {
        try (Selector sel = Selector.open(); ServerSocketChannel ss = ServerSocketChannel.open()) {
            ss.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0)).configureBlocking(false);
            SelectionKey sk = ss.register(sel, SelectionKey.OP_ACCEPT, "srv");
            SocketChannel c = SocketChannel.open();
            c.configureBlocking(false);
            c.connect(ss.getLocalAddress());
            SelectionKey ck = c.register(sel, SelectionKey.OP_CONNECT);
            String regRead = f(() -> ss.register(sel, SelectionKey.OP_READ));
            String bad = f(() -> ck.interestOps(-1));
            String blk = f(() -> c.configureBlocking(true));
            sk.cancel();
            sel.selectNow();
            System.out.println("selector " + regRead + "," + bad + "," + blk + "," + f(sk::interestOps) + "," + sk.isValid()
                    + "," + ss.isRegistered() + "," + (ss.keyFor(sel) == null) + "," + sel.getClass().getName()
                    + "," + sel.provider().getClass().getName() + "," + sk.attachment());
            c.close();
        }
    }

    static void dgram() throws Exception {
        try (DatagramChannel a = DatagramChannel.open(); DatagramChannel b = DatagramChannel.open()) {
            a.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            b.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            a.connect(b.getLocalAddress());
            System.out.println("dgram " + f(() -> a.send(ByteBuffer.allocate(1), new InetSocketAddress(InetAddress.getLoopbackAddress(), 9)))
                    + "," + a.isConnected() + "," + a.getClass().getName() + "," + a.socket().getClass().getName());
        }
        DatagramChannel z = DatagramChannel.open();
        z.close();
        System.out.println("dgram-closed " + f(() -> z.send(ByteBuffer.allocate(1), new InetSocketAddress(InetAddress.getLoopbackAddress(), 9)))
                + "," + f(z::getLocalAddress));
        try (MulticastSocket m = new MulticastSocket()) {
            System.out.println("dgram-multicast " + m.isBound() + "," + f(() -> {
                m.setOption(StandardSocketOptions.IP_MULTICAST_LOOP, false);
                return m.getOption(StandardSocketOptions.IP_MULTICAST_LOOP);
            }));
        }
    }

    /** A blocking `SocketChannel.read` interrupted by another thread (`nioBlocker`). */
    static void sockintr() throws Exception {
        try (ServerSocketChannel ss = ServerSocketChannel.open()) {
            ss.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0));
            SocketChannel c = SocketChannel.open(ss.getLocalAddress());
            SocketChannel s = ss.accept();
            String[] got = new String[1];
            Thread t = new Thread(() -> got[0] = f(() -> c.read(ByteBuffer.allocate(4))));
            t.setDaemon(true);
            t.start();
            Thread.sleep(300);
            t.interrupt();
            t.join(5000);
            boolean hung = t.isAlive();
            if (hung) s.close();
            t.join(5000);
            System.out.println("sockintr " + (hung ? "HUNG" : got[0]) + "," + c.isOpen());
            c.close();
            s.close();
        }
    }
}
