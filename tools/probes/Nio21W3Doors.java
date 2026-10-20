import com.sun.net.httpserver.HttpServer;
import java.io.*;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.reflect.InvocationTargetException;
import java.net.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
import java.util.concurrent.locks.LockSupport;

/**
 * nio21 wave 3 lane L1: conformance and bench rows for the `--compatible` doors the lane closed
 * or staged on the `HttpClient` path:
 *
 *  * `intr`   `jdk-thread-interrupt` (default ON): Thread.interrupt / isInterrupted / interrupted
 *             against the VM's blocking natives (sleep, wait, park, join, a NEW thread)
 *  * `unsafe` `jdk-unsafe-delegations` (default ON): the VarHandle / atomic surfaces whose
 *             bodies reach Unsafe.weakCompareAndSet* / get*Acquire / put*Release / *Opaque /
 *             the fences
 *  * `atom`   `jdk-scalar-atomics` (flipped ON this wave)
 *  * `uri`    `jdk-uri` (staged OFF: run the co arm with +jdk-uri), including VM-minted URIs
 *             (Path.toUri, File.toURI, URL.toURI, URI.create) read by the JDK's bodies
 *  * `baos`   `jdk-byte-array-streams` (staged OFF: +jdk-byte-array-streams)
 *  * `huc`    the HttpURLConnection fixed-length body stream: getClass().getName()
 *             (`CRATONVM_HUC_BODY_STREAM_DISPLAY`) and the request body the server received
 *
 * Every conformance line is a fact, so the output diffs against HotSpot 25. Row `hucbaos`
 * prints `instanceof ByteArrayOutputStream` and the superclass for that stream (HotSpot: false,
 * java.io.FilterOutputStream); in the default set since nio21 wave 4, when the `--compatible`
 * stream became the JDK's own StreamingOutputStream over the carrier.
 *
 * Run:
 *   javac -d out tools/probes/Nio21W3Doors.java
 *   java -cp out Nio21W3Doors > hs.txt
 *   cratonvm --jdk-only   -cp out Nio21W3Doors > jo.txt
 *   cratonvm --compatible -cp out Nio21W3Doors > co.txt
 *   CRATONVM_COMPAT_ALIGN=+jdk-uri,+jdk-byte-array-streams cratonvm --compatible -cp out Nio21W3Doors > co-on.txt
 *   CRATONVM_COMPAT_ALIGN=-jdk-thread-interrupt,-jdk-unsafe-delegations,-jdk-scalar-atomics \
 *       cratonvm --compatible -cp out Nio21W3Doors > co-off.txt
 * hs, jo, co and co-on must be identical (nio21 wave 4: `jdk-uri` and `jdk-byte-array-streams`
 * are default ON, so co and co-on are the same arms). -Dnio21.rows=a,b selects rows.
 *
 * Bench (`<row> <N> ns/op`): -Dnio21.bench=1, rows bench-interrupted bench-isinterrupted
 * bench-wcas bench-acq bench-atomic bench-uri bench-baos (-Dnio21.iters=N default 200000,
 * -Dnio21.warm=N default 3). Interleave the arms, 5 runs each.
 */
public class Nio21W3Doors {
    static String rows = System.getProperty("nio21.rows", "all");
    static boolean on(String r) {
        // nio21 w4 L1: `hucbaos` is in the default set since the fixed-length
        // stream is the JDK's own StreamingOutputStream under --compatible too.
        return rows.equals("all") || Arrays.asList(rows.split(",")).contains(r);
    }
    static String ex(Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }
    static void p(String s) { System.out.println(s); }

    public static void main(String[] a) throws Throwable {
        Locale.setDefault(Locale.US);
        String bv = System.getProperty("nio21.bench", "");
        if (bv.equals("1") || bv.equals("true")) { bench(); return; }
        String[] all = {"intr", "unsafe", "atom", "uri", "baos", "huc", "hucbaos"};
        for (String r : all) {
            if (!on(r)) continue;
            try {
                Nio21W3Doors.class.getDeclaredMethod(r).invoke(null);
            } catch (InvocationTargetException e) {
                p(r + " THREW " + ex(e.getCause()));
            }
        }
        p("done");
    }

    // ---- jdk-thread-interrupt ----------------------------------------------------------

    static void intr() throws Throwable {
        Thread me = Thread.currentThread();
        p("intr start " + me.isInterrupted() + " " + Thread.interrupted());
        me.interrupt();
        p("intr self " + me.isInterrupted() + " " + me.isInterrupted() + " " + Thread.interrupted()
                + " " + Thread.interrupted() + " " + me.isInterrupted());
        me.interrupt();
        try { Thread.sleep(1000); p("intr sleep none"); }
        catch (InterruptedException e) { p("intr sleep " + ex(e) + " after=" + me.isInterrupted()); }
        me.interrupt();
        Object lock = new Object();
        synchronized (lock) {
            try { lock.wait(1000); p("intr wait none"); }
            catch (InterruptedException e) { p("intr wait " + e.getClass().getName() + " after=" + me.isInterrupted()); }
        }
        me.interrupt();
        LockSupport.park();
        p("intr park returns status=" + me.isInterrupted() + " " + Thread.interrupted());

        Thread fresh = new Thread(() -> {});
        fresh.interrupt();
        p("intr new " + fresh.isInterrupted() + " state=" + fresh.getState());
        fresh.start();
        fresh.join();
        p("intr new-after-run " + fresh.isInterrupted());

        CountDownLatch ready = new CountDownLatch(1);
        String[] seen = new String[3];
        Thread sleeper = new Thread(() -> {
            ready.countDown();
            try { Thread.sleep(60_000); seen[0] = "slept"; }
            catch (InterruptedException e) { seen[0] = e.getClass().getName(); }
            seen[1] = String.valueOf(Thread.currentThread().isInterrupted());
            Thread.currentThread().interrupt();
            seen[2] = String.valueOf(Thread.interrupted()) + Thread.interrupted();
        });
        sleeper.start();
        ready.await();
        while (sleeper.getState() != Thread.State.TIMED_WAITING) Thread.onSpinWait();
        sleeper.interrupt();
        sleeper.join(10_000);
        p("intr cross-sleep " + seen[0] + " " + seen[1] + " " + seen[2] + " alive=" + sleeper.isAlive());

        BlockingQueue<String> q = new ArrayBlockingQueue<>(1);
        String[] got = new String[1];
        Thread taker = new Thread(() -> {
            try { got[0] = q.take(); }
            catch (InterruptedException e) { got[0] = e.getClass().getName() + " " + Thread.currentThread().isInterrupted(); }
        });
        taker.start();
        while (taker.getState() != Thread.State.WAITING) Thread.onSpinWait();
        taker.interrupt();
        taker.join(10_000);
        p("intr cross-take " + got[0]);

        Thread parker = new Thread(() -> {
            LockSupport.park();
            got[0] = "unparked " + Thread.currentThread().isInterrupted() + " " + Thread.interrupted()
                    + " " + Thread.currentThread().isInterrupted();
        });
        parker.start();
        while (parker.getState() != Thread.State.WAITING) Thread.onSpinWait();
        parker.interrupt();
        parker.join(10_000);
        p("intr cross-park " + got[0]);

        ExecutorService pool = Executors.newFixedThreadPool(2);
        CountDownLatch started = new CountDownLatch(1);
        Future<?> f = pool.submit(() -> {
            started.countDown();
            try { Thread.sleep(60_000); } catch (InterruptedException e) { return "int"; }
            return "slept";
        });
        started.await();
        List<Runnable> left = pool.shutdownNow();
        p("intr shutdownNow left=" + left.size() + " term=" + pool.awaitTermination(10, TimeUnit.SECONDS)
                + " fut=" + f.get());
    }

    // ---- jdk-unsafe-delegations ----------------------------------------------------------

    static int iv;
    static long lv;
    static Object ov;
    static final VarHandle IV, LV, OV;
    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            IV = l.findStaticVarHandle(Nio21W3Doors.class, "iv", int.class);
            LV = l.findStaticVarHandle(Nio21W3Doors.class, "lv", long.class);
            OV = l.findStaticVarHandle(Nio21W3Doors.class, "ov", Object.class);
        } catch (ReflectiveOperationException e) { throw new ExceptionInInitializerError(e); }
    }

    static boolean wcasInt(int e, int n) {
        // A weak CAS may fail spuriously: retry while the value still matches.
        while (true) {
            if (IV.weakCompareAndSet(e, n)) return true;
            if ((int) IV.getVolatile() != e) return false;
        }
    }

    static void unsafe() {
        iv = 1; lv = 10L; ov = "a";
        p("unsafe wcas " + wcasInt(1, 2) + " " + wcasInt(1, 3) + " " + iv);
        boolean b = false;
        for (int i = 0; i < 100 && !b; i++) b = LV.weakCompareAndSetPlain(10L, 11L);
        p("unsafe wcas-long-plain " + b + " " + lv);
        b = false;
        for (int i = 0; i < 100 && !b; i++) b = OV.weakCompareAndSetAcquire("a", "b");
        p("unsafe wcas-ref-acq " + b + " " + ov);
        b = false;
        for (int i = 0; i < 100 && !b; i++) b = IV.weakCompareAndSetRelease(2, 4);
        p("unsafe wcas-int-rel " + b + " " + iv);
        p("unsafe cae " + IV.compareAndExchangeAcquire(4, 5) + " " + IV.compareAndExchangeRelease(9, 6) + " "
                + LV.compareAndExchangeAcquire(11L, 12L) + " " + OV.compareAndExchangeRelease("b", "c") + " " + iv + " " + lv + " " + ov);
        IV.setRelease(7); LV.setRelease(13L); OV.setRelease("d");
        p("unsafe acq " + IV.getAcquire() + " " + LV.getAcquire() + " " + OV.getAcquire() + " "
                + IV.getOpaque() + " " + OV.getOpaque());
        IV.setOpaque(8); OV.setOpaque("e");
        p("unsafe opaque " + iv + " " + ov);
        VarHandle.acquireFence(); VarHandle.releaseFence(); VarHandle.loadLoadFence(); VarHandle.storeStoreFence(); VarHandle.fullFence();
        p("unsafe fences ok");
        AtomicReference<String> r = new AtomicReference<>("x");
        b = false;
        for (int i = 0; i < 100 && !b; i++) b = r.weakCompareAndSetVolatile("x", "y");
        p("unsafe atomic-ref " + b + " " + r.get() + " " + r.getAcquire() + " " + r.getOpaque());
        r.setRelease("z"); r.lazySet("w");
        p("unsafe atomic-ref-set " + r.get());
        AtomicInteger ai = new AtomicInteger(3);
        b = false;
        for (int i = 0; i < 100 && !b; i++) b = ai.weakCompareAndSetPlain(3, 4);
        p("unsafe atomic-int " + b + " " + ai.get() + " " + ai.getAcquire() + " " + ai.compareAndExchangeAcquire(4, 5) + " " + ai);
        int[] arr = {1, 2, 3};
        VarHandle AH = MethodHandles.arrayElementVarHandle(int[].class);
        AH.setRelease(arr, 1, 20);
        p("unsafe array " + AH.getAcquire(arr, 1) + " " + AH.compareAndExchangeAcquire(arr, 2, 3, 30) + " " + Arrays.toString(arr));
        try { AH.getAcquire(arr, 5); p("unsafe array-oob none"); }
        catch (Throwable t) { p("unsafe array-oob " + t.getClass().getName()); }
    }

    // ---- jdk-scalar-atomics --------------------------------------------------------------

    static void atom() throws Throwable {
        AtomicLong al = new AtomicLong();
        AtomicInteger ai = new AtomicInteger(5);
        AtomicBoolean ab = new AtomicBoolean();
        AtomicReference<String> ar = new AtomicReference<>();
        p("atom init " + al + " " + ai + " " + ab + " " + ar);
        p("atom ops " + al.incrementAndGet() + " " + al.getAndAdd(5) + " " + al.addAndGet(-2) + " "
                + al.compareAndSet(4, 9) + " " + al.getAndUpdate(x -> x * 2) + " " + al.accumulateAndGet(3, Long::sum) + " " + al.get());
        p("atom int " + ai.getAndIncrement() + " " + ai.decrementAndGet() + " " + ai.getAndSet(1) + " "
                + ai.updateAndGet(x -> x + 10) + " " + ai.intValue() + " " + ai.longValue());
        p("atom bool " + ab.compareAndSet(false, true) + " " + ab.getAndSet(false) + " " + ab.get());
        p("atom ref " + ar.compareAndSet(null, "a") + " " + ar.getAndSet("b") + " " + ar.updateAndGet(s -> s + "!") + " " + ar);
        AtomicLong shared = new AtomicLong();
        Thread[] ts = new Thread[4];
        for (int i = 0; i < ts.length; i++) {
            ts[i] = new Thread(() -> { for (int k = 0; k < 20_000; k++) shared.incrementAndGet(); });
            ts[i].start();
        }
        for (Thread t : ts) t.join();
        p("atom contended " + shared.get());
        p("atom classes " + al.getClass().getName() + " " + ar.getClass().getSuperclass().getName());
    }

    // ---- jdk-uri ---------------------------------------------------------------------------

    static String u(URI x) {
        return x + " s=" + x.getScheme() + " a=" + x.getAuthority() + " h=" + x.getHost() + " port=" + x.getPort()
                + " p=" + x.getPath() + " rp=" + x.getRawPath() + " q=" + x.getQuery() + " f=" + x.getFragment()
                + " ssp=" + x.getSchemeSpecificPart() + " abs=" + x.isAbsolute() + " opq=" + x.isOpaque();
    }

    static void uri() throws Throwable {
        URI base = URI.create("http://user@example.test:8080/a/b/c?x=1#frag");
        p("uri create " + u(base));
        p("uri resolve " + u(base.resolve("../d%20e?y=2")));
        p("uri normalize " + u(URI.create("http://h/a/./b/../c").normalize()));
        p("uri relativize " + URI.create("http://h/a/").relativize(URI.create("http://h/a/b/c")));
        p("uri opaque " + u(URI.create("mailto:someone@example.test")));
        p("uri eq " + URI.create("HTTP://H/%2f").equals(URI.create("http://h/%2F")) + " "
                + (URI.create("http://h/x").hashCode() == new URI("http", "h", "/x", null).hashCode())
                + " " + URI.create("http://h/a").compareTo(URI.create("http://h/b")));
        p("uri multi " + new URI("http", "u", "h", 81, "/p q", "a=b c", "f g"));
        try { new URI("http://h/a b"); p("uri bad none"); } catch (URISyntaxException e) { p("uri bad " + ex(e)); }
        try { base.resolve((String) null); p("uri null none"); } catch (Throwable t) { p("uri null " + t.getClass().getName()); }
        try { URI.create("rel/x").toURL(); p("uri tourl-rel none"); } catch (Throwable t) { p("uri tourl-rel " + ex(t)); }
        Path dir = Files.createTempDirectory("nio21w3 uri");
        // nio21 w4 L1: the file is deleted in `finally` too. A row that threw
        // used to skip `Files.delete(f)`, and the `finally`'s directory delete
        // then replaced the row's own exception with
        // DirectoryNotEmptyException (the w3 `+jdk-uri` host run).
        Path fp = dir.resolve("a b.txt");
        try {
            Path f = Files.writeString(fp, "hi");
            URI pu = f.toUri();
            p("uri path-touri " + pu.getScheme() + " " + pu.getRawPath().endsWith("/a%20b.txt") + " "
                    + pu.getPath().endsWith("/a b.txt") + " " + pu.getAuthority() + " " + pu.isOpaque());
            p("uri dir-touri " + dir.toUri().getRawPath().endsWith("/") + " " + Path.of(dir.toUri()).equals(dir));
            URI fu = f.toFile().toURI();
            p("uri file-touri " + fu.getScheme() + " " + fu.getPath().endsWith("/a b.txt") + " " + new File(fu).getName());
            URI lu = f.toUri().toURL().toURI();
            p("uri url-touri " + lu.equals(pu) + " " + lu.getRawPath().equals(pu.getRawPath()));
            try {
                p("uri resolved-path " + Path.of(dir.toUri().resolve("a%20b.txt")).getFileName() + " "
                        + Files.readString(Path.of(dir.toUri().resolve("a%20b.txt"))));
            } catch (Exception e) {
                p("uri resolved-path " + ex(e));
            }
            Files.delete(f);
        } finally {
            Files.deleteIfExists(fp);
            Files.delete(dir);
        }
        p("uri tourl " + URI.create("http://h:9/x?q").toURL() + " " + URI.create("http://h:9/x").toURL().getPort());
    }

    // ---- jdk-byte-array-streams ------------------------------------------------------------

    static void baos() throws Throwable {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        b.write('a');
        b.write("bcd".getBytes(StandardCharsets.US_ASCII));
        b.write("xyz123".getBytes(StandardCharsets.US_ASCII), 3, 2);
        p("baos " + b.size() + " " + b + " " + Arrays.toString(b.toByteArray()) + " " + b.toString(StandardCharsets.UTF_8));
        p("baos charset-name " + b.toString("UTF-16BE").length());
        try { b.toString("no-such"); p("baos bad-charset none"); } catch (UnsupportedEncodingException e) { p("baos bad-charset " + ex(e)); }
        ByteArrayOutputStream c = new ByteArrayOutputStream(1);
        b.writeTo(c);
        b.reset();
        b.close();
        b.write('z');
        p("baos reset " + b.size() + " " + b + " copy=" + c);
        try { new ByteArrayOutputStream(-1); p("baos neg none"); } catch (IllegalArgumentException e) { p("baos neg " + ex(e)); }
        try { b.write(null, 0, 1); p("baos null none"); } catch (Throwable t) { p("baos null " + t.getClass().getName()); }
        try { b.write(new byte[2], 1, 2); p("baos oob none"); } catch (Throwable t) { p("baos oob " + t.getClass().getName()); }
        ByteArrayOutputStream big = new ByteArrayOutputStream();
        for (int i = 0; i < 100_000; i++) big.write(i);
        long h = 0;
        for (byte x : big.toByteArray()) h = h * 31 + x;
        p("baos big " + big.size() + " " + h);
        ByteArrayOutputStream sub = new ByteArrayOutputStream() { @Override public void write(int x) { super.write(x + 1); } };
        new PrintStream(sub, true, StandardCharsets.UTF_8).print("ab");
        sub.write('a');
        p("baos sub " + sub + " " + sub.getClass().getSuperclass().getName());
        ObjectOutputStream oos = new ObjectOutputStream(big = new ByteArrayOutputStream());
        oos.writeObject(List.of(1, "two"));
        oos.close();
        p("baos oos " + new ObjectInputStream(new ByteArrayInputStream(big.toByteArray())).readObject());
    }

    // ---- HttpURLConnection fixed-length body stream ----------------------------------------

    static HttpServer server(List<String> bodies) throws IOException {
        HttpServer s = HttpServer.create(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0), 0);
        s.createContext("/up", x -> {
            bodies.add(new String(x.getRequestBody().readAllBytes(), StandardCharsets.UTF_8));
            byte[] r = "ok".getBytes(StandardCharsets.UTF_8);
            x.sendResponseHeaders(200, r.length);
            x.getResponseBody().write(r);
            x.close();
        });
        s.start();
        return s;
    }

    static OutputStream fixedStream(HttpServer s, HttpURLConnection[] out) throws IOException {
        URL url = URI.create("http://127.0.0.1:" + s.getAddress().getPort() + "/up").toURL();
        HttpURLConnection c = (HttpURLConnection) url.openConnection();
        c.setDoOutput(true);
        c.setRequestMethod("POST");
        c.setFixedLengthStreamingMode(5);
        out[0] = c;
        return c.getOutputStream();
    }

    static void huc() throws Throwable {
        List<String> bodies = Collections.synchronizedList(new ArrayList<>());
        HttpServer s = server(bodies);
        try {
            HttpURLConnection[] c = new HttpURLConnection[1];
            OutputStream o = fixedStream(s, c);
            p("huc stream " + o.getClass().getName());
            o.write('h');
            o.write("ello".getBytes(StandardCharsets.UTF_8));
            o.close();
            p("huc response " + c[0].getResponseCode() + " " + new String(c[0].getInputStream().readAllBytes(), StandardCharsets.UTF_8)
                    + " body=" + bodies);
            HttpURLConnection plain = (HttpURLConnection) URI.create("http://127.0.0.1:" + s.getAddress().getPort() + "/up").toURL().openConnection();
            plain.setDoOutput(true);
            OutputStream po = plain.getOutputStream();
            po.write("buffered".getBytes(StandardCharsets.UTF_8));
            po.close();
            po.write('X');
            p("huc buffered-stream " + po.getClass().getName());
            p("huc buffered " + plain.getResponseCode() + " " + plain.getRequestMethod() + " body=" + bodies.get(bodies.size() - 1));
        } finally {
            s.stop(0);
        }
    }

    static void hucbaos() throws Throwable {
        HttpServer s = server(Collections.synchronizedList(new ArrayList<>()));
        try {
            HttpURLConnection[] c = new HttpURLConnection[1];
            OutputStream o = fixedStream(s, c);
            p("hucbaos instanceof-baos " + (o instanceof ByteArrayOutputStream) + " " + o.getClass().getSuperclass().getName());
            o.write("hello".getBytes(StandardCharsets.UTF_8));
            o.close();
            p("hucbaos response " + c[0].getResponseCode());
        } finally {
            s.stop(0);
        }
    }

    // ---- bench ------------------------------------------------------------------------------

    static long sink;

    static void bench() throws Throwable {
        int iters = Integer.getInteger("nio21.iters", 200_000);
        int warm = Integer.getInteger("nio21.warm", 3);
        String sel = System.getProperty("nio21.rows", "all");
        Map<String, Runnable> rs = new LinkedHashMap<>();
        rs.put("bench-interrupted", () -> { for (int i = 0; i < iters; i++) if (Thread.interrupted()) sink++; });
        rs.put("bench-isinterrupted", () -> { Thread t = Thread.currentThread(); for (int i = 0; i < iters; i++) if (t.isInterrupted()) sink++; });
        rs.put("bench-wcas", () -> { for (int i = 0; i < iters; i++) { long v = (long) LV.getVolatile(); LV.weakCompareAndSetPlain(v, v + 1); } sink += lv; });
        rs.put("bench-acq", () -> { for (int i = 0; i < iters; i++) { IV.setRelease(i); sink += (int) IV.getAcquire(); } });
        AtomicLong al = new AtomicLong();
        rs.put("bench-atomic", () -> { for (int i = 0; i < iters; i++) al.incrementAndGet(); sink += al.get(); });
        rs.put("bench-uri", () -> { for (int i = 0; i < iters / 20; i++) sink += URI.create("http://h:" + (i & 1023) + "/p?q").getPort(); });
        rs.put("bench-baos", () -> { ByteArrayOutputStream b = new ByteArrayOutputStream(); for (int i = 0; i < iters; i++) b.write(i); sink += b.size(); });
        for (Map.Entry<String, Runnable> e : rs.entrySet()) {
            if (!sel.equals("all") && !Arrays.asList(sel.split(",")).contains(e.getKey())) continue;
            for (int w = 0; w < warm; w++) e.getValue().run();
            long t0 = System.nanoTime();
            e.getValue().run();
            long dt = System.nanoTime() - t0;
            int n = e.getKey().equals("bench-uri") ? iters / 20 : iters;
            p(e.getKey() + " " + n + " " + String.format(Locale.US, "%.1f", dt / (double) n) + " ns/op");
        }
        if (sink == 42) p("");
    }
}
