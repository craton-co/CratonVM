// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.net.httpserver.HttpServer;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpHeaders;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.TreeMap;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentLinkedDeque;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Round nio20 wave 5, lane L1: the {@code java.net.http.HttpClient} per-request path taken apart
 * (`docs/internal/fixed-bugs/r20-jdk-http-client-throughput-is-ten-to-fifty-x-hotspot-CLOSED-20261012.md`).
 *
 * <p>{@code SbrW5HttpBench hc.get} is 468 us/req on HotSpot, 5 475 under {@code --jdk-only} and
 * 15 370 under {@code --compatible} (row {@code jdk-http-client} ON), CPU-bound. The rows below
 * time, in isolation, the java.base pieces the JDK client crosses per request, so the
 * {@code --compatible / --jdk-only} ratio of each row names the piece that carries the
 * {@code --compatible}-only factor, and the {@code --jdk-only / HotSpot} ratio the JIT's:
 *
 * <ul>
 *   <li>{@code hc}      -- the whole thing: {@code client.send(GET, ofByteArray())} against an
 *       in-process {@code HttpServer} (keep-alive, 1 KiB body). ns per request.</li>
 *   <li>{@code hdr}     -- {@code HttpHeaders.of(map, filter)} over six response-shaped headers,
 *       then {@code firstValue}, {@code firstValueAsLong}, {@code allValues} and a
 *       {@code map().entrySet()} walk: what {@code Http1HeaderParser} / {@code Http1Response}
 *       do per response (a {@code TreeMap(CASE_INSENSITIVE_ORDER)} underneath).</li>
 *   <li>{@code tmap}    -- that {@code TreeMap} alone: six puts, six case-folded gets, one
 *       entry walk.</li>
 *   <li>{@code cf}      -- a {@code CompletableFuture} chain on one thread, the
 *       {@code MultiExchange} / {@code Exchange} shape: {@code thenCompose}, {@code thenApply},
 *       {@code handle}, {@code whenComplete}, then {@code complete} and {@code join}.</li>
 *   <li>{@code cfx}     -- {@code complete} on a worker thread, {@code get()} on this one (the
 *       {@code HttpClientImpl.send} wait: {@code waitingGet}, park / unpark).</li>
 *   <li>{@code uri}     -- {@code URI.create} of the request URI plus the accessors
 *       {@code HttpRequestImpl} / {@code Http1Request} read.</li>
 *   <li>{@code isa}     -- {@code new InetSocketAddress("127.0.0.1", port)} as a
 *       {@code HashMap} key (the {@code ConnectionPool} lookup).</li>
 *   <li>{@code conc}    -- {@code ConcurrentLinkedDeque} offer / poll, {@code AtomicLong},
 *       {@code ReentrantLock} (the {@code SequentialScheduler} / {@code SocketTube} queues).</li>
 *   <li>{@code exec}    -- a hand-off to a cached thread pool and back (the client's executor).</li>
 *   <li>{@code bbuf}    -- {@code ByteBuffer.wrap} / {@code allocate}, {@code put}, {@code flip},
 *       {@code slice}, a {@code List<ByteBuffer>} remaining() sum (the {@code Utils} helpers).</li>
 *   <li>{@code hstr}    -- the header-name string work: {@code toLowerCase(Locale.ROOT)},
 *       {@code equalsIgnoreCase}, a {@code StringBuilder} request line.</li>
 * </ul>
 *
 * <pre>
 *   javac -d /tmp/p20 tools/probes/Nio20HttpHot.java
 *   java     -cp /tmp/p20 Nio20HttpHot                         # HotSpot reference
 *   cratonvm -cp /tmp/p20 Nio20HttpHot                         # --jdk-only (launcher default)
 *   cratonvm --compatible -cp /tmp/p20 Nio20HttpHot
 *   # NOTE: in-process, `--compatible` serves `hc` from the VM's Rust carrier server
 *   # (com.sun.net.httpserver is still native there unless CRATONVM_COMPAT_ALIGN=+jdk-http-server),
 *   # `--jdk-only` from the JDK's ServerImpl: the two modes then time different servers.
 *   # Same server for every arm: run it on HotSpot and point the client at it.
 *   java -cp /tmp/p20 Nio20HttpHot serve 18080 &amp;
 *   cratonvm --compatible -cp /tmp/p20 -Dnio20.rows=hc -Dnio20.url=http://127.0.0.1:18080/get Nio20HttpHot
 *   CRATONVM_COMPAT_ALIGN=+jdk-http-server cratonvm --compatible -cp /tmp/p20 -Dnio20.rows=hc Nio20HttpHot
 *   # the natives each row crosses, per mode (diff the two dumps' invocation columns):
 *   CRATONVM_CENSUS_EXACT_INVOCATIONS=1 cratonvm --compatible --dump-native-registry /tmp/hc-c.json \
 *       -cp /tmp/p20 -Dnio20.rows=hc Nio20HttpHot
 *   CRATONVM_CENSUS_EXACT_INVOCATIONS=1 cratonvm --dump-native-registry /tmp/hc-j.json \
 *       -cp /tmp/p20 -Dnio20.rows=hc Nio20HttpHot
 * </pre>
 *
 * {@code -Dnio20.rows=a,b} selects rows (default all), {@code -Dnio20.n=} scales the op counts
 * (default 1.0). Output: one {@code check <row> <value>} line per row (identical in every VM and
 * mode), then one {@code <row> <N> ns/op} line per row (median of five timed runs after two
 * warm-up runs).
 */
public class Nio20HttpHot {
    static final String ROWS = System.getProperty("nio20.rows", "all");
    static final double SCALE = Double.parseDouble(System.getProperty("nio20.n", "1.0"));

    static boolean on(String row) {
        if (ROWS.equals("all")) return true;
        for (String r : ROWS.split(",")) if (r.trim().equals(row)) return true;
        return false;
    }

    static int ops(int n) {
        return Math.max(1, (int) (n * SCALE));
    }

    interface Row {
        long run(int n) throws Exception;
    }

    static final List<String> TIMES = new ArrayList<>();

    static void time(String name, int n, Row row) throws Exception {
        if (!on(name)) return;
        long check = row.run(n);
        System.out.println("check " + name + " " + check);
        row.run(n);
        long[] t = new long[5];
        for (int i = 0; i < t.length; i++) {
            long t0 = System.nanoTime();
            long c = row.run(n);
            t[i] = System.nanoTime() - t0;
            if (c != check) System.out.println("DRIFT " + name + " " + c + " != " + check);
        }
        java.util.Arrays.sort(t);
        TIMES.add(name + " " + (t[2] / n) + " ns/op");
    }

    // ---- hc ---------------------------------------------------------------------------------

    static HttpServer server;
    static HttpClient client;
    static HttpRequest hget;
    static int port;
    static final byte[] PAYLOAD = new byte[1024];

    static void startServer() throws Exception {
        String url = System.getProperty("nio20.url");
        client = HttpClient.newBuilder().version(HttpClient.Version.HTTP_1_1).build();
        if (url != null) {
            // An out-of-process server (see `serve` in main): the client is timed
            // against the SAME server in every VM and mode.
            hget = HttpRequest.newBuilder(URI.create(url)).build();
            return;
        }
        server = newServer(0);
        port = server.getAddress().getPort();
        hget = HttpRequest.newBuilder(URI.create("http://127.0.0.1:" + port + "/get")).build();
    }

    static HttpServer newServer(int bindPort) throws Exception {
        for (int i = 0; i < PAYLOAD.length; i++) PAYLOAD[i] = (byte) ('a' + i % 26);
        HttpServer server =
                HttpServer.create(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), bindPort), 0);
        server.createContext("/get", ex -> {
            ex.getResponseHeaders().add("X-Probe", "nio20");
            ex.sendResponseHeaders(200, PAYLOAD.length);
            try (OutputStream o = ex.getResponseBody()) {
                o.write(PAYLOAD);
            }
        });
        server.start();
        return server;
    }

    static long hc(int n) throws Exception {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            HttpResponse<byte[]> r = client.send(hget, HttpResponse.BodyHandlers.ofByteArray());
            byte[] b = r.body();
            sum += r.statusCode() + b.length + b[b.length - 1]
                    + r.headers().firstValue("x-probe").map(String::length).orElse(-1);
        }
        return sum;
    }

    // ---- hdr / tmap -------------------------------------------------------------------------

    static final Map<String, List<String>> RAW = new LinkedHashMap<>();

    static {
        RAW.put("Date", List.of("Thu, 08 Oct 2026 10:00:00 GMT"));
        RAW.put("Content-length", List.of("1024"));
        RAW.put("Content-Type", List.of("application/octet-stream"));
        RAW.put("X-probe", List.of("nio20"));
        RAW.put("Connection", List.of("keep-alive"));
        RAW.put("Set-Cookie", List.of("a=1", "b=2"));
    }

    static long hdr(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            HttpHeaders h = HttpHeaders.of(RAW, (k, v) -> true);
            sum += h.firstValueAsLong("content-length").orElse(-1);
            sum += h.firstValue("CONNECTION").map(String::length).orElse(-1);
            sum += h.allValues("set-cookie").size();
            sum += h.firstValue("transfer-encoding").isPresent() ? 7 : 3;
            for (Map.Entry<String, List<String>> e : h.map().entrySet()) {
                sum += e.getKey().length() + e.getValue().size();
            }
        }
        return sum;
    }

    static final String[] NAMES = {"Date", "Content-length", "Content-Type", "X-probe", "Connection", "Host"};
    static final String[] ASKS = {"date", "CONTENT-LENGTH", "content-type", "x-Probe", "connection", "HOST"};

    static long tmap(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            TreeMap<String, String> m = new TreeMap<>(String.CASE_INSENSITIVE_ORDER);
            for (String s : NAMES) m.put(s, s);
            for (String s : ASKS) {
                String v = m.get(s);
                sum += v == null ? -1 : v.length();
            }
            for (Map.Entry<String, String> e : m.entrySet()) sum += e.getKey().charAt(0);
        }
        return sum;
    }

    // ---- cf / cfx ---------------------------------------------------------------------------

    static long cf(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            final int k = i & 15;
            CompletableFuture<Integer> start = new CompletableFuture<>();
            CompletableFuture<Integer> out = start
                    .thenCompose(v -> CompletableFuture.completedFuture(v + 1))
                    .thenApply(v -> v * 2)
                    .handle((v, t) -> t == null ? v : -1)
                    .whenComplete((v, t) -> { });
            start.complete(k);
            sum += out.join();
        }
        return sum;
    }

    static long cfx(int n) throws Exception {
        SynchronousQueue<CompletableFuture<Integer>> q = new SynchronousQueue<>();
        Thread w = new Thread(() -> {
            try {
                for (int i = 0; ; i++) {
                    CompletableFuture<Integer> f = q.take();
                    if (f.isCancelled()) return;
                    f.complete(i & 7);
                }
            } catch (InterruptedException e) {
                // done
            }
        }, "nio20-cfx");
        w.setDaemon(true);
        w.start();
        long sum = 0;
        for (int i = 0; i < n; i++) {
            CompletableFuture<Integer> f = new CompletableFuture<>();
            q.put(f);
            sum += f.get();
        }
        CompletableFuture<Integer> stop = new CompletableFuture<>();
        stop.cancel(false);
        q.put(stop);
        w.join();
        return sum;
    }

    // ---- uri / isa --------------------------------------------------------------------------

    static long uri(int n) {
        long sum = 0;
        String s = "http://127.0.0.1:" + 8080 + "/get?x=" + 1;
        for (int i = 0; i < n; i++) {
            URI u = URI.create(s);
            sum += u.getHost().length() + u.getPort() + u.getRawPath().length() + u.getScheme().length()
                    + (u.getRawQuery() == null ? 0 : u.getRawQuery().length())
                    + (u.getRawUserInfo() == null ? 1 : 0) + (u.isAbsolute() ? 2 : 0);
        }
        return sum;
    }

    static long isa(int n) {
        HashMap<InetSocketAddress, Integer> pool = new HashMap<>();
        pool.put(new InetSocketAddress("127.0.0.1", 8080), 1);
        long sum = 0;
        for (int i = 0; i < n; i++) {
            InetSocketAddress a = new InetSocketAddress("127.0.0.1", 8080 + (i & 1));
            Integer v = pool.get(a);
            sum += (v == null ? 0 : v) + a.getPort() + (a.isUnresolved() ? 100 : 0);
        }
        return sum;
    }

    // ---- conc / exec ------------------------------------------------------------------------

    static long conc(int n) {
        ConcurrentLinkedDeque<Integer> d = new ConcurrentLinkedDeque<>();
        AtomicLong a = new AtomicLong();
        ReentrantLock lock = new ReentrantLock();
        long sum = 0;
        for (int i = 0; i < n; i++) {
            d.offer(i & 31);
            d.offer((i + 1) & 31);
            lock.lock();
            try {
                sum += d.poll();
            } finally {
                lock.unlock();
            }
            sum += d.pollFirst();
            sum += a.incrementAndGet() & 1;
            if (a.compareAndSet(i + 1, i + 1)) sum += 2;
        }
        return sum;
    }

    static ExecutorService pool;

    static long exec(int n) throws Exception {
        long[] box = new long[1];
        for (int i = 0; i < n; i++) {
            CountDownLatch l = new CountDownLatch(1);
            final int k = i & 7;
            pool.execute(() -> {
                box[0] += k;
                l.countDown();
            });
            l.await();
        }
        return box[0];
    }

    // ---- bbuf / hstr ------------------------------------------------------------------------

    static long bbuf(int n) {
        long sum = 0;
        byte[] src = PAYLOAD;
        for (int i = 0; i < n; i++) {
            ByteBuffer a = ByteBuffer.wrap(src, i & 63, 256);
            ByteBuffer b = ByteBuffer.allocate(512);
            b.put(a);
            b.flip();
            ByteBuffer c = b.slice(16, 64);
            List<ByteBuffer> l = List.of(b, c);
            long rem = 0;
            for (ByteBuffer x : l) rem += x.remaining();
            sum += rem + c.get(3) + b.get();
        }
        return sum;
    }

    static long hstr(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            StringBuilder sb = new StringBuilder(64);
            sb.append("GET ").append("/get").append(" HTTP/1.1\r\n");
            for (String s : NAMES) {
                String lc = s.toLowerCase(Locale.ROOT);
                if (lc.equalsIgnoreCase("content-length")) sum += 3;
                sb.append(s).append(": ").append(lc.length()).append("\r\n");
            }
            sum += sb.length() + sb.toString().indexOf("Host");
        }
        return sum;
    }

    public static void main(String[] args) throws Exception {
        if (args.length >= 1 && args[0].equals("serve")) {
            // `java -cp . Nio20HttpHot serve 18080`: the server alone, until killed.
            int p = args.length >= 2 ? Integer.parseInt(args[1]) : 18080;
            newServer(p);
            System.out.println("serving http://127.0.0.1:" + p + "/get");
            Thread.currentThread().join();
            return;
        }
        if (on("hc")) startServer();
        if (on("exec")) pool = Executors.newCachedThreadPool(r -> {
            Thread t = new Thread(r, "nio20-exec");
            t.setDaemon(true);
            return t;
        });
        // 200 requests per run, 7 runs (check, warm-up, five timed): 1 400 requests.
        // It was 2 000 per run (14 000 requests), which at the `--compatible`
        // carrier-server cost (~15 ms/request, `SbrW5HttpBench hc.get`) is ~210 s
        // for this row alone and ran the probe past a 300 s host timeout.
        time("hc", ops(200), Nio20HttpHot::hc);
        time("hdr", ops(50_000), Nio20HttpHot::hdr);
        time("tmap", ops(100_000), Nio20HttpHot::tmap);
        time("cf", ops(100_000), Nio20HttpHot::cf);
        time("cfx", ops(20_000), Nio20HttpHot::cfx);
        time("uri", ops(100_000), Nio20HttpHot::uri);
        time("isa", ops(100_000), Nio20HttpHot::isa);
        time("conc", ops(200_000), Nio20HttpHot::conc);
        time("exec", ops(20_000), Nio20HttpHot::exec);
        time("bbuf", ops(200_000), Nio20HttpHot::bbuf);
        time("hstr", ops(100_000), Nio20HttpHot::hstr);
        for (String t : TIMES) System.out.println(t);
        if (server != null) server.stop(0);
        if (pool != null) pool.shutdownNow();
        System.exit(0);
    }
}
