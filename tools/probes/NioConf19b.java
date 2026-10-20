import java.io.*;
import java.net.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.charset.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;

/** NIO cluster conformance, part 2 (round 19, nio19): async channels, Channels adapters, TLS ALPN /
 *  client auth / resumption, HTTP streaming, charset corner cases. -Dnio19.rows=a,b; -Dnio19.ks=pkcs12 */
public class NioConf19b {
    static String rows = System.getProperty("nio19.rows", "all");
    static boolean on(String r) { return rows.equals("all") || Arrays.asList(rows.split(",")).contains(r); }
    static String ex(Throwable t) { return t.getClass().getName() + ":" + t.getMessage(); }
    interface Fact { Object get() throws Throwable; }
    static String f(Fact x) { try { return String.valueOf(x.get()); } catch (Throwable t) { return "!" + ex(t); } }

    public static void main(String[] a) throws Throwable {
        String[] all = {"afc", "asock", "adapters", "bufmisc", "charset2", "alpn", "clientauth", "trustnull", "tmthrows", "resume", "httpstream", "httpheaders"};
        for (String r : all) {
            if (!on(r)) continue;
            try { NioConf19b.class.getDeclaredMethod(r).invoke(null); }
            catch (java.lang.reflect.InvocationTargetException e) { System.out.println(r + " THREW " + ex(e.getCause())); }
        }
        System.out.println("done");
    }

    static void afc() throws Exception {
        Path p = Files.createTempFile("nio19b", ".bin");
        try (AsynchronousFileChannel c = AsynchronousFileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            int w = c.write(ByteBuffer.wrap("async-file-data".getBytes()), 0).get(10, TimeUnit.SECONDS);
            ByteBuffer r = ByteBuffer.allocateDirect(32);
            int n = c.read(r, 6).get(10, TimeUnit.SECONDS);
            r.flip();
            byte[] b = new byte[r.remaining()];
            r.get(b);
            CompletableFuture<Integer> cf = new CompletableFuture<>();
            c.read(ByteBuffer.allocate(4), 100, null, new CompletionHandler<Integer, Object>() {
                public void completed(Integer v, Object att) { cf.complete(v); }
                public void failed(Throwable t, Object att) { cf.completeExceptionally(t); }
            });
            FileLock l = c.lock(0, 4, true).get(10, TimeUnit.SECONDS);
            System.out.println("afc " + w + "," + n + "," + new String(b) + "," + cf.get(10, TimeUnit.SECONDS) + "," + c.size() + "," + l.isShared() + "," + f(() -> c.read(ByteBuffer.allocate(1).asReadOnlyBuffer(), 0))
                    + "," + f(() -> c.read(ByteBuffer.allocate(1), -1)) + "," + f(() -> c.truncate(4).size()));
            l.release();
            c.force(false);
        }
        Files.delete(p);
    }

    static void asock() throws Exception {
        try (AsynchronousServerSocketChannel ss = AsynchronousServerSocketChannel.open().bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0))) {
            Future<AsynchronousSocketChannel> af = ss.accept();
            AsynchronousSocketChannel c = AsynchronousSocketChannel.open();
            c.connect(ss.getLocalAddress()).get(10, TimeUnit.SECONDS);
            AsynchronousSocketChannel s = af.get(10, TimeUnit.SECONDS);
            int w = c.write(ByteBuffer.wrap("async-sock".getBytes())).get(10, TimeUnit.SECONDS);
            ByteBuffer r = ByteBuffer.allocate(64);
            int n = 0;
            while (n < w) n += s.read(r).get(10, TimeUnit.SECONDS);
            CompletableFuture<Integer> eof = new CompletableFuture<>();
            c.shutdownOutput();
            s.read(ByteBuffer.allocate(8), 5, TimeUnit.SECONDS, null, new CompletionHandler<Integer, Object>() {
                public void completed(Integer v, Object a) { eof.complete(v); }
                public void failed(Throwable t, Object a) { eof.completeExceptionally(t); }
            });
            CompletableFuture<String> to = new CompletableFuture<>();
            c.read(ByteBuffer.allocate(8), 200, TimeUnit.MILLISECONDS, null, new CompletionHandler<Integer, Object>() {
                public void completed(Integer v, Object a) { to.complete("completed " + v); }
                public void failed(Throwable t, Object a) { to.complete(t.getClass().getName()); }
            });
            System.out.println("asock " + w + "," + n + "," + new String(r.array(), 0, n) + "," + eof.get(10, TimeUnit.SECONDS) + "," + to.get(10, TimeUnit.SECONDS)
                    + "," + f(() -> c.read(ByteBuffer.allocate(8)).get(1, TimeUnit.SECONDS)).replaceAll(":.*", "") + "," + c.isOpen());
            s.close();
            c.close();
        }
    }

    static void adapters() throws Exception {
        Path p = Files.createTempFile("nio19c", ".txt");
        Files.writeString(p, "line1\nlíne2\n😀 3\n", StandardCharsets.UTF_8);
        try (SeekableByteChannel ch = Files.newByteChannel(p)) {
            BufferedReader r = new BufferedReader(Channels.newReader(ch, StandardCharsets.UTF_8));
            String l1 = r.readLine(), l2 = r.readLine(), l3 = r.readLine(), l4 = r.readLine();
            System.out.println("adapters-reader " + l1 + "|" + l2 + "|" + l3.length() + "|" + l4 + "|" + ch.position());
        }
        try (InputStream in = Channels.newInputStream(Files.newByteChannel(p))) {
            byte[] b = in.readNBytes(7);
            System.out.println("adapters-in " + new String(b, StandardCharsets.UTF_8) + "," + in.available() + "," + in.skip(3) + "," + in.read());
        }
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        WritableByteChannel wc = Channels.newChannel(bo);
        int w1 = wc.write(ByteBuffer.wrap("abc".getBytes()));
        int w2 = wc.write(ByteBuffer.allocateDirect(2).put((byte) 'x').put((byte) 'y').flip());
        ReadableByteChannel rc = Channels.newChannel(new ByteArrayInputStream("readable".getBytes()));
        ByteBuffer rb = ByteBuffer.allocate(4);
        int r1 = rc.read(rb);
        Writer w = Channels.newWriter(wc, "UTF-16BE");
        w.write("é");
        w.flush();
        System.out.println("adapters-chan " + w1 + "," + w2 + "," + bo.toString(StandardCharsets.ISO_8859_1).length() + "," + r1 + "," + new String(rb.array()) + "," + wc.isOpen() + "," + f(() -> { wc.close(); return wc.write(ByteBuffer.allocate(1)); }));
        try (FileChannel fc = FileChannel.open(p, StandardOpenOption.WRITE, StandardOpenOption.APPEND)) {
            fc.write(ByteBuffer.wrap("tail".getBytes()));
            System.out.println("adapters-append " + fc.position() + "," + fc.size() + "," + f(() -> fc.write(ByteBuffer.wrap("x".getBytes()), 0)));
        }
        Files.delete(p);
    }

    static void bufmisc() {
        ByteBuffer z = ByteBuffer.allocateDirect(0);
        ByteBuffer big = ByteBuffer.allocate(1 << 20);
        big.putLong(big.capacity() - 8, Long.MIN_VALUE);
        LongBuffer lb = big.asLongBuffer();
        CharBuffer cb = CharBuffer.allocate(4);
        System.out.println("bufmisc " + z.capacity() + "," + z.isDirect() + "," + f(() -> z.get()) + "," + lb.get(lb.limit() - 1) + "," + big.order(ByteOrder.nativeOrder()).order()
                + "," + f(() -> ByteBuffer.allocateDirect(-1)) + "," + f(() -> cb.put("toolong")) + "," + cb.position() + "," + f(() -> CharBuffer.wrap(new char[3], 1, 3))
                + "," + ByteBuffer.wrap(new byte[]{1, 2, 3}).asReadOnlyBuffer().compareTo(ByteBuffer.wrap(new byte[]{1, 2, 4})) + "," + ShortBuffer.wrap(new short[]{-1}).compareTo(ShortBuffer.wrap(new short[]{1}))
                + "," + ByteBuffer.wrap(new byte[10]).position(3).slice().arrayOffset() + "," + f(() -> ByteBuffer.wrap(new byte[4]).getFloat(1)));
        ByteBuffer dup = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN);
        dup.putInt(0, 0x01020304);
        System.out.println("bufmisc-order " + dup.duplicate().order() + "," + Integer.toHexString(dup.duplicate().getInt(0)) + "," + Integer.toHexString(dup.asReadOnlyBuffer().getInt(0)) + "," + Integer.toHexString(dup.slice().getInt(0))
                + "," + Integer.toHexString(dup.getInt(0)) + "," + dup.asIntBuffer().order());
    }

    static void charset2() throws Exception {
        CharsetEncoder e = StandardCharsets.UTF_8.newEncoder().onMalformedInput(CodingErrorAction.REPLACE);
        ByteBuffer o = e.encode(CharBuffer.wrap("a\uD800b\uDC00c"));
        CharsetDecoder d = Charset.forName("windows-1252").newDecoder().onUnmappableCharacter(CodingErrorAction.REPORT);
        String undef = f(() -> d.decode(ByteBuffer.wrap(new byte[]{(byte) 0x81})));
        CharsetDecoder sj = Charset.forName("Shift_JIS").newDecoder();
        String jp = sj.decode(ByteBuffer.wrap("日本語テキスト".getBytes("Shift_JIS"))).toString();
        String rt = new String("Ünïcödé €".getBytes("UTF-16LE"), "UTF-16LE");
        System.out.println("charset2 " + Arrays.toString(Arrays.copyOf(o.array(), o.limit())) + "," + undef + "," + jp.length() + "," + jp.equals("日本語テキスト") + "," + rt
                + "," + Charset.forName("UTF-8").contains(StandardCharsets.US_ASCII) + "," + StandardCharsets.US_ASCII.contains(StandardCharsets.UTF_8) + "," + Charset.availableCharsets().containsKey("UTF-8")
                + "," + new String(new byte[]{(byte) 0xEF, (byte) 0xBB, (byte) 0xBF, 'x'}, StandardCharsets.UTF_8).length() + "," + StandardCharsets.UTF_16.decode(ByteBuffer.wrap(new byte[]{(byte) 0xFF, (byte) 0xFE, 'a', 0})));
        StringWriter sw = new StringWriter();
        try (PrintWriter pw = new PrintWriter(new OutputStreamWriter(new FilterOutputStream(new ByteArrayOutputStream()), StandardCharsets.ISO_8859_1))) {
            pw.print("ok");
        }
        Reader r = new InputStreamReader(new ByteArrayInputStream(new byte[]{'a', (byte) 0xC3}), StandardCharsets.UTF_8);
        char[] cs = new char[4];
        int n = r.read(cs);
        int n2 = r.read(cs, 1, 3);
        System.out.println("charset2-trunc " + n + "," + (int) cs[0] + "," + n2 + "," + (int) cs[1] + "," + r.read());
    }

    static javax.net.ssl.SSLContext ctx() throws Exception {
        java.security.KeyStore k = java.security.KeyStore.getInstance("PKCS12");
        try (InputStream in = new FileInputStream(System.getProperty("nio19.ks"))) { k.load(in, "changeit".toCharArray()); }
        javax.net.ssl.KeyManagerFactory kmf = javax.net.ssl.KeyManagerFactory.getInstance("SunX509");
        kmf.init(k, "changeit".toCharArray());
        javax.net.ssl.TrustManagerFactory tmf = javax.net.ssl.TrustManagerFactory.getInstance("PKIX");
        tmf.init(k);
        javax.net.ssl.SSLContext c = javax.net.ssl.SSLContext.getInstance("TLS");
        c.init(kmf.getKeyManagers(), tmf.getTrustManagers(), null);
        return c;
    }

    static String handshake(javax.net.ssl.SSLEngine cl, javax.net.ssl.SSLEngine sv) throws Exception {
        int pk = Math.max(cl.getSession().getPacketBufferSize(), sv.getSession().getPacketBufferSize()) * 2;
        ByteBuffer c2s = ByteBuffer.allocate(pk), s2c = ByteBuffer.allocate(pk);
        ByteBuffer ci = ByteBuffer.allocate(pk), si = ByteBuffer.allocate(pk);
        ByteBuffer empty = ByteBuffer.allocate(0);
        String err = "";
        for (int i = 0; i < 200; i++) {
            try {
                cl.wrap(empty, c2s); sv.wrap(empty, s2c);
                c2s.flip(); s2c.flip();
                cl.unwrap(s2c, ci); sv.unwrap(c2s, si);
                c2s.compact(); s2c.compact();
                Runnable t;
                while ((t = cl.getDelegatedTask()) != null) t.run();
                while ((t = sv.getDelegatedTask()) != null) t.run();
            } catch (javax.net.ssl.SSLException e) { err = e.getClass().getSimpleName(); break; }
            if (cl.getHandshakeStatus() == javax.net.ssl.SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING
                    && sv.getHandshakeStatus() == javax.net.ssl.SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING && i > 3) break;
        }
        return err;
    }

    static void alpn() throws Exception {
        if (System.getProperty("nio19.ks") == null) { System.out.println("alpn skipped"); return; }
        javax.net.ssl.SSLContext c = ctx();
        javax.net.ssl.SSLEngine cl = c.createSSLEngine("localhost", 443), sv = c.createSSLEngine();
        cl.setUseClientMode(true); sv.setUseClientMode(false);
        javax.net.ssl.SSLParameters cp = cl.getSSLParameters();
        cp.setApplicationProtocols(new String[]{"h2", "http/1.1"});
        cl.setSSLParameters(cp);
        javax.net.ssl.SSLParameters sp = sv.getSSLParameters();
        sp.setApplicationProtocols(new String[]{"http/1.1", "h2"});
        sv.setSSLParameters(sp);
        String err = handshake(cl, sv);
        System.out.println("alpn " + err + "," + cl.getApplicationProtocol() + "," + sv.getApplicationProtocol() + "," + cl.getSession().getProtocol()
                + "," + Arrays.asList(cl.getSSLParameters().getApplicationProtocols()) + "," + cl.getSSLParameters().getEndpointIdentificationAlgorithm());
        javax.net.ssl.SSLEngine cl2 = c.createSSLEngine("localhost", 443), sv2 = c.createSSLEngine();
        cl2.setUseClientMode(true); sv2.setUseClientMode(false);
        sv2.setHandshakeApplicationProtocolSelector((e, l) -> l.contains("special") ? "special" : null);
        javax.net.ssl.SSLParameters cp2 = cl2.getSSLParameters();
        cp2.setApplicationProtocols(new String[]{"x", "special"});
        cl2.setSSLParameters(cp2);
        System.out.println("alpn-selector " + handshake(cl2, sv2) + "," + cl2.getApplicationProtocol() + "," + sv2.getApplicationProtocol());
    }

    static void clientauth() throws Exception {
        if (System.getProperty("nio19.ks") == null) { System.out.println("clientauth skipped"); return; }
        javax.net.ssl.SSLContext c = ctx();
        javax.net.ssl.SSLEngine cl = c.createSSLEngine("localhost", 443), sv = c.createSSLEngine();
        cl.setUseClientMode(true); sv.setUseClientMode(false);
        sv.setNeedClientAuth(true);
        String err = handshake(cl, sv);
        System.out.println("clientauth " + err + "," + sv.getNeedClientAuth() + "," + f(() -> sv.getSession().getPeerCertificates().length) + "," + f(() -> sv.getSession().getPeerPrincipal().getName()));
        javax.net.ssl.SSLContext noKey = javax.net.ssl.SSLContext.getInstance("TLS");
        javax.net.ssl.TrustManagerFactory tmf = javax.net.ssl.TrustManagerFactory.getInstance("PKIX");
        java.security.KeyStore k = java.security.KeyStore.getInstance("PKCS12");
        try (InputStream in = new FileInputStream(System.getProperty("nio19.ks"))) { k.load(in, "changeit".toCharArray()); }
        tmf.init(k);
        noKey.init(null, tmf.getTrustManagers(), null);
        javax.net.ssl.SSLEngine cl2 = noKey.createSSLEngine("localhost", 443), sv2 = c.createSSLEngine();
        cl2.setUseClientMode(true); sv2.setUseClientMode(false);
        sv2.setNeedClientAuth(true);
        System.out.println("clientauth-missing " + handshake(cl2, sv2));
        javax.net.ssl.SSLContext untrusting = javax.net.ssl.SSLContext.getInstance("TLS");
        java.security.KeyStore empty = java.security.KeyStore.getInstance("PKCS12");
        empty.load(null, null);
        javax.net.ssl.TrustManagerFactory et = javax.net.ssl.TrustManagerFactory.getInstance("PKIX");
        et.init(empty);
        untrusting.init(null, et.getTrustManagers(), null);
        javax.net.ssl.SSLEngine cl3 = untrusting.createSSLEngine("localhost", 443), sv3 = c.createSSLEngine();
        cl3.setUseClientMode(true); sv3.setUseClientMode(false);
        System.out.println("clientauth-untrusted " + handshake(cl3, sv3));
    }

    /** nio20 wave 5 (L5): {@code init(kms, null, sr)} is SunJSSE's DEFAULT trust manager
     *  ({@code cacerts} / {@code javax.net.ssl.trustStore}), never the roots of the keystore the
     *  caller just loaded: a client built that way rejects the probe's self-signed server
     *  (HotSpot: SSLHandshakeException). The second line is the control (same keystore as the
     *  explicit TrustManager[]: the handshake completes). */
    static void trustnull() throws Exception {
        if (System.getProperty("nio19.ks") == null) { System.out.println("trustnull skipped"); return; }
        javax.net.ssl.SSLContext server = ctx();
        java.security.KeyStore k = java.security.KeyStore.getInstance("PKCS12");
        try (InputStream in = new FileInputStream(System.getProperty("nio19.ks"))) { k.load(in, "changeit".toCharArray()); }
        javax.net.ssl.KeyManagerFactory kmf = javax.net.ssl.KeyManagerFactory.getInstance("SunX509");
        kmf.init(k, "changeit".toCharArray());
        javax.net.ssl.SSLContext c = javax.net.ssl.SSLContext.getInstance("TLS");
        c.init(kmf.getKeyManagers(), null, null);
        javax.net.ssl.SSLEngine cl = c.createSSLEngine("localhost", 443), sv = server.createSSLEngine();
        cl.setUseClientMode(true); sv.setUseClientMode(false);
        System.out.println("trust-null " + handshake(cl, sv));
        javax.net.ssl.SSLEngine cl2 = server.createSSLEngine("localhost", 443), sv2 = server.createSSLEngine();
        cl2.setUseClientMode(true); sv2.setUseClientMode(false);
        System.out.println("trust-null-control " + handshake(cl2, sv2) + "," + cl2.getSession().getProtocol());
    }

    /** nio20 wave 5 (L5): a client {@code X509TrustManager} that throws an unchecked exception.
     *  JSSE fails the client with {@code SSLException("(internal_error) Unhandled exception")} and
     *  sends the server an {@code internal_error} alert (the server's line names it). */
    static void tmthrows() throws Exception {
        if (System.getProperty("nio19.ks") == null) { System.out.println("tmthrows skipped"); return; }
        javax.net.ssl.SSLContext server = ctx();
        javax.net.ssl.X509TrustManager throwing = new javax.net.ssl.X509TrustManager() {
            public void checkClientTrusted(java.security.cert.X509Certificate[] ch, String t) { throw new IllegalStateException("probe"); }
            public void checkServerTrusted(java.security.cert.X509Certificate[] ch, String t) { throw new IllegalStateException("probe"); }
            public java.security.cert.X509Certificate[] getAcceptedIssuers() { return new java.security.cert.X509Certificate[0]; }
        };
        javax.net.ssl.SSLContext c = javax.net.ssl.SSLContext.getInstance("TLS");
        c.init(null, new javax.net.ssl.TrustManager[]{throwing}, null);
        javax.net.ssl.SSLEngine cl = c.createSSLEngine("localhost", 443), sv = server.createSSLEngine();
        cl.setUseClientMode(true); sv.setUseClientMode(false);
        int pk = Math.max(cl.getSession().getPacketBufferSize(), sv.getSession().getPacketBufferSize()) * 2;
        ByteBuffer c2s = ByteBuffer.allocate(pk), s2c = ByteBuffer.allocate(pk);
        ByteBuffer ci = ByteBuffer.allocate(pk), si = ByteBuffer.allocate(pk);
        ByteBuffer empty = ByteBuffer.allocate(0);
        String clErr = null, svErr = null;
        for (int i = 0; i < 200 && (clErr == null || svErr == null); i++) {
            try { cl.wrap(empty, c2s); } catch (javax.net.ssl.SSLException e) { if (clErr == null) clErr = ex(e); }
            try { sv.wrap(empty, s2c); } catch (javax.net.ssl.SSLException e) { if (svErr == null) svErr = ex(e); }
            c2s.flip(); s2c.flip();
            try { cl.unwrap(s2c, ci); } catch (javax.net.ssl.SSLException e) { if (clErr == null) clErr = ex(e); }
            try { sv.unwrap(c2s, si); } catch (javax.net.ssl.SSLException e) { if (svErr == null) svErr = ex(e); }
            c2s.compact(); s2c.compact();
            Runnable t;
            while ((t = cl.getDelegatedTask()) != null) t.run();
            while ((t = sv.getDelegatedTask()) != null) t.run();
            if (cl.isOutboundDone() && sv.isInboundDone() && c2s.position() == 0) break;
        }
        System.out.println("tmthrows client " + clErr);
        System.out.println("tmthrows server " + svErr);
    }

    static void resume() throws Exception {
        if (System.getProperty("nio19.ks") == null) { System.out.println("resume skipped"); return; }
        javax.net.ssl.SSLContext c = ctx();
        String[] out = new String[2];
        for (int i = 0; i < 2; i++) {
            javax.net.ssl.SSLEngine cl = c.createSSLEngine("localhost", 4433), sv = c.createSSLEngine();
            cl.setUseClientMode(true); sv.setUseClientMode(false);
            cl.setEnabledProtocols(new String[]{"TLSv1.2"}); sv.setEnabledProtocols(new String[]{"TLSv1.2"});
            String err = handshake(cl, sv);
            out[i] = err + ":" + Arrays.hashCode(cl.getSession().getId());
        }
        System.out.println("resume " + out[0].split(":")[0] + "," + out[1].split(":")[0] + "," + out[0].split(":")[1].equals(out[1].split(":")[1]) + "," + (c.getClientSessionContext().getIds().hasMoreElements()));
    }

    static void httpstream() throws Exception {
        com.sun.net.httpserver.HttpServer srv = com.sun.net.httpserver.HttpServer.create(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0), 0);
        srv.createContext("/chunked", ex -> {
            ex.sendResponseHeaders(200, 0);
            try (OutputStream o = ex.getResponseBody()) { for (int i = 0; i < 50; i++) { o.write(("chunk" + i + "\n").getBytes()); o.flush(); } }
        });
        srv.createContext("/upload", ex -> {
            byte[] b = ex.getRequestBody().readAllBytes();
            byte[] r = (b.length + ":" + Arrays.hashCode(b) + ":" + ex.getRequestHeaders().getFirst("Transfer-encoding")).getBytes();
            ex.sendResponseHeaders(200, r.length); ex.getResponseBody().write(r); ex.close();
        });
        srv.createContext("/head", ex -> { ex.getResponseHeaders().add("X-Len", "5"); ex.sendResponseHeaders(204, -1); ex.close(); });
        ExecutorService es = Executors.newFixedThreadPool(2);
        srv.setExecutor(es);
        srv.start();
        String base = "http://127.0.0.1:" + srv.getAddress().getPort();
        try {
            java.net.http.HttpClient hc = java.net.http.HttpClient.newHttpClient();
            var lines = hc.send(java.net.http.HttpRequest.newBuilder(URI.create(base + "/chunked")).build(), java.net.http.HttpResponse.BodyHandlers.ofLines()).body();
            long cnt = lines.count();
            byte[] big = new byte[300_000];
            new Random(7).nextBytes(big);
            var up = hc.send(java.net.http.HttpRequest.newBuilder(URI.create(base + "/upload")).POST(java.net.http.HttpRequest.BodyPublishers.ofInputStream(() -> new ByteArrayInputStream(big))).build(),
                    java.net.http.HttpResponse.BodyHandlers.ofString());
            var hd = hc.send(java.net.http.HttpRequest.newBuilder(URI.create(base + "/head")).method("HEAD", java.net.http.HttpRequest.BodyPublishers.noBody()).build(), java.net.http.HttpResponse.BodyHandlers.ofString());
            var is = hc.send(java.net.http.HttpRequest.newBuilder(URI.create(base + "/chunked")).build(), java.net.http.HttpResponse.BodyHandlers.ofInputStream()).body();
            byte[] first = is.readNBytes(7);
            is.close();
            HttpURLConnection u = (HttpURLConnection) new URL(base + "/upload").openConnection();
            u.setDoOutput(true);
            u.setChunkedStreamingMode(1000);
            try (OutputStream o = u.getOutputStream()) { o.write(big, 0, 12345); }
            String ur = new String(u.getInputStream().readAllBytes());
            System.out.println("httpstream " + cnt + "," + up.body() + "," + hd.statusCode() + "," + hd.headers().firstValue("x-len").orElse("-") + "," + hd.body().isEmpty() + "," + new String(first).trim() + "," + ur);
        } finally {
            srv.stop(0);
            es.shutdownNow();
        }
    }

    static void httpheaders() throws Exception {
        try (ServerSocket ss = new ServerSocket(0, 5, InetAddress.getLoopbackAddress())) {
            Thread t = new Thread(() -> {
                for (int k = 0; k < 2; k++) {
                    try (Socket s = ss.accept()) {
                        BufferedReader r = new BufferedReader(new InputStreamReader(s.getInputStream(), StandardCharsets.ISO_8859_1));
                        StringBuilder req = new StringBuilder();
                        String line;
                        while ((line = r.readLine()) != null && !line.isEmpty()) if (line.toLowerCase().startsWith("x-")) req.append(line).append(';');
                        String body = "hdrs=" + req;
                        String resp = "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=UTF-8\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nX-Multi: one\r\nX-Multi: two\r\nContent-Length: " + body.length() + "\r\nConnection: close\r\n\r\n" + body;
                        s.getOutputStream().write(resp.getBytes(StandardCharsets.ISO_8859_1));
                    } catch (Exception e) { }
                }
            });
            t.setDaemon(true);
            t.start();
            String url = "http://127.0.0.1:" + ss.getLocalPort() + "/h";
            var r = java.net.http.HttpClient.newHttpClient().send(java.net.http.HttpRequest.newBuilder(URI.create(url)).header("X-A", "1").header("X-A", "2").header("X-B", "b").build(), java.net.http.HttpResponse.BodyHandlers.ofString());
            HttpURLConnection u = (HttpURLConnection) new URL(url).openConnection();
            u.addRequestProperty("X-C", "c");
            String ub = new String(u.getInputStream().readAllBytes());
            System.out.println("httpheaders " + r.body() + "," + r.headers().allValues("set-cookie") + "," + r.headers().allValues("x-multi") + "," + r.headers().firstValue("content-type").orElse("-")
                    + "," + f(() -> java.net.http.HttpRequest.newBuilder(URI.create(url)).header("Host", "x"))
                    + "|" + ub + "," + u.getHeaderFields().get("X-Multi") + "," + u.getContentType());
        }
    }
}
