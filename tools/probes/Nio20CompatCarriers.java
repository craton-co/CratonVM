import java.io.*;
import java.net.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;

/** nio20 wave 3 L1: the --compatible carrier rows (file channel options, async accept / timeout,
 *  chunked request framing). Deterministic text, one fact line per row; diff against HotSpot 25.
 *  -Dnio20.rows=a,b (default all). */
public class Nio20CompatCarriers {
    static String rows = System.getProperty("nio20.rows", "all");
    static boolean on(String r) { return rows.equals("all") || Arrays.asList(rows.split(",")).contains(r); }
    static String ex(Throwable t) { return t.getClass().getName() + ":" + t.getMessage(); }
    interface Fact { Object get() throws Throwable; }
    static String f(Fact x) { try { return String.valueOf(x.get()); } catch (Throwable t) { return "!" + ex(t); } }

    public static void main(String[] a) throws Throwable {
        String[] all = {"fcopts", "avail", "accept", "atimeout", "chunked"};
        for (String r : all) {
            if (!on(r)) continue;
            try { Nio20CompatCarriers.class.getDeclaredMethod(r).invoke(null); }
            catch (java.lang.reflect.InvocationTargetException e) { System.out.println(r + " THREW " + ex(e.getCause())); }
        }
        System.out.println("done");
    }

    static void fcopts() throws Exception {
        Path p = Files.createTempFile("nio20c", ".bin");
        Files.write(p, "0123".getBytes());
        String createNew = f(() -> { try (FileChannel c = FileChannel.open(p, StandardOpenOption.WRITE, StandardOpenOption.CREATE_NEW)) { return c.size(); } }).replaceAll(":.*", "");
        String readAppend = f(() -> FileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.APPEND));
        String appendTrunc = f(() -> FileChannel.open(p, StandardOpenOption.WRITE, StandardOpenOption.APPEND, StandardOpenOption.TRUNCATE_EXISTING));
        Path missing = p.resolveSibling(p.getFileName() + ".missing");
        String appendMissing = f(() -> FileChannel.open(missing, StandardOpenOption.APPEND)).replaceAll(":.*", "");
        long pos, size;
        try (FileChannel c = FileChannel.open(p, StandardOpenOption.APPEND)) {
            c.write(ByteBuffer.wrap("ab".getBytes()), 0);
            pos = c.position();
            size = c.size();
        }
        System.out.println("fcopts " + createNew + "," + readAppend + "," + appendTrunc + "," + appendMissing + "," + pos + "," + size + "," + new String(Files.readAllBytes(p)));
        Files.delete(p);
    }

    static void avail() throws Exception {
        Path p = Files.createTempFile("nio20d", ".bin");
        Files.write(p, new byte[100]);
        try (SeekableByteChannel ch = Files.newByteChannel(p); InputStream in = Channels.newInputStream(ch)) {
            int a0 = in.available();
            in.readNBytes(30);
            int a1 = in.available();
            ch.position(95);
            System.out.println("avail " + a0 + "," + a1 + "," + in.available());
        }
        Files.delete(p);
    }

    static void accept() throws Exception {
        try (AsynchronousServerSocketChannel unbound = AsynchronousServerSocketChannel.open()) {
            String nb = f(() -> unbound.accept()).replaceAll(":.*", "");
            try (AsynchronousServerSocketChannel ss = AsynchronousServerSocketChannel.open().bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0))) {
                Future<AsynchronousSocketChannel> af = ss.accept();
                boolean doneEarly = af.isDone();
                try (AsynchronousSocketChannel c = AsynchronousSocketChannel.open()) {
                    c.connect(ss.getLocalAddress()).get(10, TimeUnit.SECONDS);
                    AsynchronousSocketChannel s = af.get(10, TimeUnit.SECONDS);
                    c.write(ByteBuffer.wrap("hi".getBytes())).get(10, TimeUnit.SECONDS);
                    ByteBuffer b = ByteBuffer.allocate(8);
                    int n = s.read(b).get(10, TimeUnit.SECONDS);
                    System.out.println("accept " + nb + "," + doneEarly + "," + s.isOpen() + "," + (s.getRemoteAddress() != null) + "," + n + "," + new String(b.array(), 0, n));
                    s.close();
                }
            }
        }
    }

    static void atimeout() throws Exception {
        try (AsynchronousServerSocketChannel ss = AsynchronousServerSocketChannel.open().bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0))) {
            CompletableFuture<AsynchronousSocketChannel> acc = new CompletableFuture<>();
            ss.accept(null, new CompletionHandler<AsynchronousSocketChannel, Object>() {
                public void completed(AsynchronousSocketChannel v, Object o) { acc.complete(v); }
                public void failed(Throwable t, Object o) { acc.completeExceptionally(t); }
            });
            try (AsynchronousSocketChannel c = AsynchronousSocketChannel.open()) {
                c.connect(ss.getLocalAddress()).get(10, TimeUnit.SECONDS);
                AsynchronousSocketChannel s = acc.get(10, TimeUnit.SECONDS);
                CompletableFuture<String> to = new CompletableFuture<>();
                c.read(ByteBuffer.allocate(8), 150, TimeUnit.MILLISECONDS, null, new CompletionHandler<Integer, Object>() {
                    public void completed(Integer v, Object a) { to.complete("completed " + v); }
                    public void failed(Throwable t, Object a) { to.complete(t.getClass().getName() + ":" + t.getMessage()); }
                });
                System.out.println("atimeout " + to.get(10, TimeUnit.SECONDS) + "," + c.isOpen());
                s.close();
            }
        }
    }

    static void chunked() throws Exception {
        com.sun.net.httpserver.HttpServer srv = com.sun.net.httpserver.HttpServer.create(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0), 0);
        srv.createContext("/up", ex -> {
            byte[] b = ex.getRequestBody().readAllBytes();
            byte[] r = (b.length + ":" + ex.getRequestHeaders().getFirst("Transfer-encoding") + ":" + ex.getRequestHeaders().getFirst("Content-length")).getBytes();
            ex.sendResponseHeaders(200, r.length); ex.getResponseBody().write(r); ex.close();
        });
        ExecutorService es = Executors.newFixedThreadPool(2);
        srv.setExecutor(es);
        srv.start();
        String base = "http://127.0.0.1:" + srv.getAddress().getPort() + "/up";
        try {
            java.net.http.HttpClient hc = java.net.http.HttpClient.newBuilder().version(java.net.http.HttpClient.Version.HTTP_1_1).build();
            byte[] body = new byte[5000];
            String known = hc.send(java.net.http.HttpRequest.newBuilder(URI.create(base)).POST(java.net.http.HttpRequest.BodyPublishers.ofByteArray(body)).build(), java.net.http.HttpResponse.BodyHandlers.ofString()).body();
            String stream = hc.send(java.net.http.HttpRequest.newBuilder(URI.create(base)).POST(java.net.http.HttpRequest.BodyPublishers.ofInputStream(() -> new ByteArrayInputStream(body))).build(), java.net.http.HttpResponse.BodyHandlers.ofString()).body();
            HttpURLConnection u = (HttpURLConnection) new URL(base).openConnection();
            u.setDoOutput(true);
            u.setChunkedStreamingMode(0);
            try (OutputStream o = u.getOutputStream()) { o.write(body, 0, 4097); }
            String huc = new String(u.getInputStream().readAllBytes());
            HttpURLConnection u2 = (HttpURLConnection) new URL(base).openConnection();
            u2.setDoOutput(true);
            try (OutputStream o = u2.getOutputStream()) { o.write(body, 0, 10); }
            String huc2 = new String(u2.getInputStream().readAllBytes());
            System.out.println("chunked " + known + "|" + stream + "|" + huc + "|" + huc2);
        } finally {
            srv.stop(0);
            es.shutdownNow();
        }
    }
}
