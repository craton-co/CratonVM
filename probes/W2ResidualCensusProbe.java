import java.io.*;
import java.lang.reflect.*;
import java.net.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.locks.ReentrantReadWriteLock;

/**
 * The workload that reaches the classes the wave-2 layout residuals name, and
 * that `JdkOnlyCensusLoadProbe` does NOT reach.
 *
 * `JdkOnlyCensusLoadProbe` defines 151 modelled classes and none of them are
 * `java/net/URI`, `java/net/Proxy`, `java/util/HashMap$Node`,
 * `jdk/internal/math/FloatingDecimal$1` or
 * `ReentrantReadWriteLock$Sync$ThreadLocalHoldCounter` — so its silence on
 * those rows says nothing at all. Every section here exists to make one of
 * them load and be written through a native.
 *
 * It is a census workload, not an assertion suite: it prints one paired line
 * per behaviour so a transcript can be diffed byte-for-byte against HotSpot,
 * and every section is wrapped so one unsupported corner cannot truncate the
 * run and make a short census read like a clean one.
 */
public class W2ResidualCensusProbe {
    static int sections = 0, failed = 0;

    static void section(String name, Runnable body) {
        sections++;
        try {
            body.run();
        } catch (Throwable t) {
            failed++;
            System.out.println("SECTION-FAILED " + name + ": " + t);
        }
    }

    public static void main(String[] args) throws Exception {
        section("uri", W2ResidualCensusProbe::uri);
        section("proxy", W2ResidualCensusProbe::proxy);
        section("mapnodes", W2ResidualCensusProbe::mapnodes);
        section("floatingdecimal", W2ResidualCensusProbe::floatingDecimal);
        section("rrwl", W2ResidualCensusProbe::rrwl);
        section("readerwriter", W2ResidualCensusProbe::readerWriter);
        section("classmirror", W2ResidualCensusProbe::classMirror);
        System.out.println("W2RESIDUAL sections=" + sections + " failed=" + failed);
    }

    // ---------------------------------------------------------------- URI
    //
    // Every accessor is printed because the whole defect family is "the value
    // landed in the wrong field": a URI whose `port` reads -1 when it should
    // read 8080, or whose `authority` reads a number, is exactly what a
    // positional write onto the real layout produces.
    static void uri() {
        try {
            URI[] us = {
                new URI("http://user:pw@example.com:8080/a/b?q=1#frag"),
                URI.create("https://host/path"),
                new URI("file:///tmp/x"),
                new URI("scheme", "auth", "/p", "q", "f"),
                Paths.get("/tmp").toUri(),
                new URL("http://example.org:81/z?y=2").toURI(),
            };
            for (URI u : us) {
                System.out.println("URI " + u
                        + " scheme=" + u.getScheme()
                        + " authority=" + u.getAuthority()
                        + " userInfo=" + u.getUserInfo()
                        + " host=" + u.getHost()
                        + " port=" + u.getPort()
                        + " path=" + u.getPath()
                        + " query=" + u.getQuery()
                        + " fragment=" + u.getFragment()
                        + " ssp=" + u.getSchemeSpecificPart()
                        + " abs=" + u.isAbsolute()
                        + " opaque=" + u.isOpaque());
            }
            URI base = new URI("http://example.com/a/b/");
            System.out.println("URI resolve=" + base.resolve("c/d") + " relativize="
                    + base.relativize(new URI("http://example.com/a/b/c/d")));
            System.out.println("URI eq=" + us[0].equals(new URI(us[0].toString()))
                    + " hashEq=" + (us[0].hashCode() == new URI(us[0].toString()).hashCode()));
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    // -------------------------------------------------------------- Proxy
    //
    // `Proxy.type()` returns a `Proxy$Type` ENUM on a real JDK; a model that
    // stores the kind as an `int` cannot answer it, so this is the behavioural
    // half of census kind 4.
    static void proxy() {
        java.net.Proxy p =
                new java.net.Proxy(java.net.Proxy.Type.HTTP, new InetSocketAddress("127.0.0.1", 3128));
        System.out.println("Proxy p=" + p + " type=" + p.type() + " addr=" + p.address()
                + " isHttp=" + (p.type() == java.net.Proxy.Type.HTTP));
        System.out.println("Proxy NO_PROXY=" + java.net.Proxy.NO_PROXY
                + " type=" + java.net.Proxy.NO_PROXY.type()
                + " addr=" + java.net.Proxy.NO_PROXY.address());
        java.net.Proxy socks =
                new java.net.Proxy(java.net.Proxy.Type.SOCKS, new InetSocketAddress("127.0.0.1", 1080));
        System.out.println("Proxy socks=" + socks.type() + " eq=" + socks.equals(p));
        try {
            List<java.net.Proxy> sel =
                    ProxySelector.getDefault().select(URI.create("http://example.com/"));
            System.out.println("ProxySelector n=" + sel.size() + " first=" + sel.get(0).type());
        } catch (Throwable t) {
            System.out.println("ProxySelector unavailable: " + t.getClass().getName());
        }
    }

    // -------------------------------------------------- HashMap$Node family
    //
    // Serialization is the reader that runs REAL bytecode over our nodes
    // (`HashMap.writeObject` -> `internalWriteEntries` is not force-native), so
    // it is the one path where a wrong node slot is observable from Java.
    static void mapnodes() {
        try {
            HashMap<String, Object> m = new HashMap<>();
            for (int i = 0; i < 64; i++) {
                m.put("k" + i, i);
            }
            m.put("nullv", null);
            int sum = 0;
            for (Map.Entry<String, Object> e : m.entrySet()) {
                sum += String.valueOf(e.getKey()).length();
                if (e.getValue() instanceof Integer) {
                    sum += (Integer) e.getValue();
                }
            }
            System.out.println("Node sum=" + sum + " size=" + m.size()
                    + " get7=" + m.get("k7") + " nullv=" + m.get("nullv")
                    + " containsNullv=" + m.containsKey("nullv"));
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            try (ObjectOutputStream oo = new ObjectOutputStream(bo)) {
                oo.writeObject(m);
            }
            Object back;
            try (ObjectInputStream oi = new ObjectInputStream(new ByteArrayInputStream(bo.toByteArray()))) {
                back = oi.readObject();
            }
            System.out.println("Node roundtrip eq=" + m.equals(back) + " bytes=" + (bo.size() > 0));
            Set<String> hs = new HashSet<>(m.keySet());
            System.out.println("Node set n=" + hs.size() + " has=" + hs.contains("k3")
                    + " rm=" + hs.remove("k3") + " rmAgain=" + hs.remove("k3"));
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    // ---------------------------------------------- FloatingDecimal$1 family
    static void floatingDecimal() {
        StringBuilder sb = new StringBuilder();
        double[] ds = {0.0, -0.0, 1.0, 0.1, 1e-7, 1.5e300, Double.MIN_VALUE, Double.MAX_VALUE};
        for (double d : ds) {
            sb.append(Double.toString(d)).append('|');
        }
        String[] ss = {"0.1", "1e10", "-3.25", "0", "1.7976931348623157E308"};
        for (String s : ss) {
            sb.append(Double.parseDouble(s)).append('|').append(Float.parseFloat(s)).append('|');
        }
        System.out.println("FloatingDecimal " + sb);
    }

    // -------------------------------- ReentrantReadWriteLock hold counters
    //
    // The per-thread hold counter is a ThreadLocal subclass instantiated on the
    // first REENTRANT read acquisition, which is why one lock/unlock pair does
    // not load it.
    static void rrwl() {
        ReentrantReadWriteLock l = new ReentrantReadWriteLock();
        for (int i = 0; i < 3; i++) {
            l.readLock().lock();
            l.readLock().lock();
            try {
                // nested acquisition is what mints the hold counter
            } finally {
                l.readLock().unlock();
                l.readLock().unlock();
            }
        }
        l.writeLock().lock();
        try {
            System.out.println("RRWL writeHeld=" + l.isWriteLockedByCurrentThread()
                    + " readHold=" + l.getReadHoldCount());
        } finally {
            l.writeLock().unlock();
        }
        l.readLock().lock();
        System.out.println("RRWL readCount=" + l.getReadLockCount()
                + " holdCount=" + l.getReadHoldCount() + " fair=" + l.isFair());
        l.readLock().unlock();
    }

    // ---------------------------------------------- java.io Reader/Writer
    //
    // The kind-3 family: an fd or a wrapped stream parked on `Reader.lock`,
    // `Reader.skipBuffer` or `Writer.writeBuffer`. `Writer.write(String)` is
    // the JDK path that ALLOCATES `writeBuffer`, so it is the one that would
    // collide with an fd sitting there.
    static void readerWriter() {
        try {
            Path p = Files.createTempFile("w2res", ".txt");
            try (BufferedWriter bw = Files.newBufferedWriter(p, StandardCharsets.UTF_8)) {
                bw.write("alpha\n");
                bw.write("beta gamma\n");   // String overload -> Writer.writeBuffer
                bw.newLine();
                bw.flush();
            }
            try (BufferedReader br = Files.newBufferedReader(p, StandardCharsets.UTF_8)) {
                String line;
                StringBuilder sb = new StringBuilder();
                while ((line = br.readLine()) != null) {
                    sb.append('[').append(line).append(']');
                }
                System.out.println("RW read=" + sb);
            }
            try (OutputStreamWriter osw =
                         new OutputStreamWriter(Files.newOutputStream(p), StandardCharsets.UTF_8)) {
                osw.write("delta epsilon");
                osw.flush();
            }
            try (InputStreamReader isr =
                         new InputStreamReader(Files.newInputStream(p), StandardCharsets.UTF_8)) {
                char[] cb = new char[64];
                int n = isr.read(cb);
                System.out.println("RW isr n=" + n + " s=" + new String(cb, 0, Math.max(n, 0))
                        + " ready=" + isr.ready() + " enc=" + isr.getEncoding());
            }
            try (BufferedReader br = new BufferedReader(
                    new InputStreamReader(Files.newInputStream(p), StandardCharsets.UTF_8))) {
                System.out.println("RW chain=" + br.readLine() + " skip=" + br.skip(0));
            }
            System.out.println("RW size=" + Files.size(p));
            Files.deleteIfExists(p);
        } catch (IOException e) {
            throw new RuntimeException(e);
        }
    }

    // --------------------------------------------------- class-mirror slot 0
    //
    // Three rounds so the second and third take the real bytecode's
    // `cachedConstructor != null` fast path — the reader that the VM-internal
    // Int at `java.lang.Class` slot 0 sits on top of.
    static void classMirror() {
        try {
            for (int i = 0; i < 3; i++) {
                Constructor<Holder> c = Holder.class.getDeclaredConstructor();
                Constructor<String> s = String.class.getConstructor(String.class);
                System.out.println("Mirror round=" + i + " c=" + c.getName()
                        + " s=" + s.getParameterCount()
                        + " inst=" + c.newInstance().tag
                        + " prim=" + int.class.getName() + "/" + void.class.getName()
                        + " arr=" + int[].class.getName());
            }
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    public static class Holder {
        final String tag = "holder";
    }
}
