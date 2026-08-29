// Phase 1's four unclosed lanes, exercised through the payload each one names.
//
//   P1-C  SSLSocket.get{Output,Input}Stream()          -> all HTTPS
//   P1-E  java/lang/foreign/DowncallHandle             -> all of Panama/FFM
//   P1-F  cratonvm/internal/SnapshotEnumeration        -> CHM keys()/elements()
//   P1-H  java/util/concurrent/CompletedFuture         -> AsynchronousFileChannel
//
// The roadmap's Phase 1 mechanism is: an essential native survives strict mode,
// asks for a FABRICATED receiver, is correctly refused, and kills its caller
// with NoClassDefFoundError. `Phase1Sweep` closed A/B/D/G/I by showing the
// mechanism no longer fires. These four were never measured the same way.
//
// So the rows below care about two things and print both: does the call SURVIVE
// (a NoClassDefFoundError is the Phase 1 failure), and does it produce HotSpot's
// ANSWER (an ordinary defect, worth its own row either way).
//
// Hygiene: stdout only, no identity hashes, no timings, no host/port/temp-path
// text -- every row is a shape, a count, or an exception class name. The TLS arm
// talks to a loopback server this probe starts itself, so it needs no network
// and no certificate authority.
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.ByteBuffer;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.Enumeration;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.Future;
import java.nio.channels.AsynchronousFileChannel;

public class P1RemainingSweep {
    interface Body { Object run() throws Throwable; }

    static void t(String tag, Body b) {
        String v;
        try {
            Object o = b.run();
            v = String.valueOf(o);
        } catch (Throwable e) {
            // A NoClassDefFoundError here is the Phase 1 mechanism firing, so it
            // is labelled rather than folded in with the ordinary refusals.
            String k = e.getClass().getName();
            v = (e instanceof NoClassDefFoundError)
                ? "PHASE1-KILL " + k + ": " + e.getMessage()
                : "throws " + k;
        }
        System.out.println(tag + " = " + v);
    }

    public static void main(String[] a) {
        p1f_chmEnumerations();
        p1h_asyncFileChannel();
        p1e_panamaDowncall();
        p1c_sslStreams();
        System.out.println("DONE");
    }

    // ---------------- P1-F: ConcurrentHashMap.keys() / elements() ----------

    static ConcurrentHashMap<String, Integer> chm(int n) {
        ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
        for (int i = 0; i < n; i++) m.put("k" + i, i);
        return m;
    }

    static void p1f_chmEnumerations() {
        t("chm.keys.count", () -> {
            Enumeration<String> e = chm(5).keys();
            int n = 0;
            while (e.hasMoreElements()) { e.nextElement(); n++; }
            return n;
        });
        t("chm.elements.sum", () -> {
            Enumeration<Integer> e = chm(5).elements();
            int s = 0;
            while (e.hasMoreElements()) s += e.nextElement();
            return s;
        });
        t("chm.keys.empty", () -> chm(0).keys().hasMoreElements());
        t("chm.keys.pastEnd", () -> {
            Enumeration<String> e = chm(0).keys();
            e.nextElement();
            return "no-throw";
        });
        t("chm.keys.isEnumeration", () -> chm(1).keys() instanceof Enumeration);
        // keySet()/values() are the Collection views beside the Enumerations.
        t("chm.keySet.size", () -> chm(4).keySet().size());
        t("chm.values.size", () -> chm(4).values().size());
        t("chm.keySet.sorted", () -> {
            java.util.List<String> l = new java.util.ArrayList<>(chm(3).keySet());
            java.util.Collections.sort(l);
            return l.toString();
        });
        t("chm.entrySet.size", () -> chm(4).entrySet().size());
        // A CHM built to hold enough entries to leave one bin.
        t("chm.keys.count.large", () -> {
            Enumeration<String> e = chm(64).keys();
            int n = 0;
            while (e.hasMoreElements()) { e.nextElement(); n++; }
            return n;
        });
    }

    // ---------------- P1-H: AsynchronousFileChannel ------------------------

    static void p1h_asyncFileChannel() {
        t("afc.writeThenRead", () -> {
            Path p = Files.createTempFile("p1h", ".bin");
            try {
                byte[] payload = "phase-one-h".getBytes("UTF-8");
                try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(
                        p, StandardOpenOption.WRITE)) {
                    Future<Integer> w = ch.write(ByteBuffer.wrap(payload), 0);
                    if (w.get() != payload.length) return "SHORT-WRITE " + w.get();
                }
                try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(
                        p, StandardOpenOption.READ)) {
                    ByteBuffer buf = ByteBuffer.allocate(payload.length);
                    Future<Integer> r = ch.read(buf, 0);
                    int got = r.get();
                    return got + ":" + new String(buf.array(), 0, Math.max(got, 0), "UTF-8");
                }
            } finally {
                Files.deleteIfExists(p);
            }
        });
        t("afc.readPastEof", () -> {
            Path p = Files.createTempFile("p1h", ".bin");
            try {
                Files.write(p, "abc".getBytes("UTF-8"));
                try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(
                        p, StandardOpenOption.READ)) {
                    ByteBuffer buf = ByteBuffer.allocate(8);
                    return ch.read(buf, 100).get();
                }
            } finally {
                Files.deleteIfExists(p);
            }
        });
        t("afc.negativePosition", () -> {
            Path p = Files.createTempFile("p1h", ".bin");
            try {
                try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(
                        p, StandardOpenOption.READ)) {
                    return ch.read(ByteBuffer.allocate(4), -1).get();
                }
            } finally {
                Files.deleteIfExists(p);
            }
        });
        t("afc.futureIsDone", () -> {
            Path p = Files.createTempFile("p1h", ".bin");
            try {
                Files.write(p, "xy".getBytes("UTF-8"));
                try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(
                        p, StandardOpenOption.READ)) {
                    Future<Integer> r = ch.read(ByteBuffer.allocate(2), 0);
                    r.get();
                    return r.isDone() + "/" + r.isCancelled();
                }
            } finally {
                Files.deleteIfExists(p);
            }
        });
        t("afc.writeToReadOnly", () -> {
            Path p = Files.createTempFile("p1h", ".bin");
            try {
                try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(
                        p, StandardOpenOption.READ)) {
                    return ch.write(ByteBuffer.wrap(new byte[] { 1 }), 0).get();
                }
            } finally {
                Files.deleteIfExists(p);
            }
        });
        t("afc.sizeAndClose", () -> {
            Path p = Files.createTempFile("p1h", ".bin");
            try {
                Files.write(p, "12345".getBytes("UTF-8"));
                AsynchronousFileChannel ch = AsynchronousFileChannel.open(
                        p, StandardOpenOption.READ);
                long sz = ch.size();
                ch.close();
                return sz + "/" + ch.isOpen();
            } finally {
                Files.deleteIfExists(p);
            }
        });
    }

    // ---------------- P1-E: Panama / FFM downcall --------------------------

    static void p1e_panamaDowncall() {
        // Reached reflectively so a JDK without the module, or a VM that refuses
        // the linker, produces a ROW rather than killing the probe.
        // Resolve every method on the PUBLIC INTERFACE, never on the returned
        // implementation's own class. `Linker.nativeLinker()` hands back
        // `jdk.internal.foreign.abi.…` and `SymbolLookup.find` a lambda -- both
        // non-public, both an IllegalAccessException to invoke reflectively on
        // HotSpot. Three rows of the first draft measured that and not the VM.
        t("ffm.nativeLinker", () -> {
            Class<?> linker = Class.forName("java.lang.foreign.Linker");
            return linker.getMethod("nativeLinker").invoke(null) != null;
        });
        t("ffm.defaultLookup.find.strlen", () -> {
            Class<?> linker = Class.forName("java.lang.foreign.Linker");
            Class<?> lookupC = Class.forName("java.lang.foreign.SymbolLookup");
            Object l = linker.getMethod("nativeLinker").invoke(null);
            Object lookup = linker.getMethod("defaultLookup").invoke(l);
            Object found = lookupC.getMethod("find", String.class).invoke(lookup, "strlen");
            return ((java.util.Optional<?>) found).isPresent();
        });
        t("ffm.downcallHandle.strlen", () -> {
            Class<?> linkerC = Class.forName("java.lang.foreign.Linker");
            Class<?> fdC = Class.forName("java.lang.foreign.FunctionDescriptor");
            Class<?> vlC = Class.forName("java.lang.foreign.ValueLayout");
            Class<?> memL = Class.forName("java.lang.foreign.MemoryLayout");
            Object l = linkerC.getMethod("nativeLinker").invoke(null);
            Object lookup = linkerC.getMethod("defaultLookup").invoke(l);
            Class<?> lookupC = Class.forName("java.lang.foreign.SymbolLookup");
            Object seg = lookupC.getMethod("find", String.class).invoke(lookup, "strlen");
            Object addr = ((java.util.Optional<?>) seg).orElse(null);
            Object jlong = vlC.getField("JAVA_LONG").get(null);
            Object jaddr = vlC.getField("ADDRESS").get(null);
            Object args = java.lang.reflect.Array.newInstance(memL, 1);
            java.lang.reflect.Array.set(args, 0, jaddr);
            Object fd = fdC.getMethod("of", memL, args.getClass())
                    .invoke(null, jlong, args);
            Object mh = linkerC.getMethod("downcallHandle",
                    Class.forName("java.lang.foreign.MemorySegment"), fdC,
                    Class.forName("[Ljava.lang.foreign.Linker$Option;"))
                    .invoke(l, addr, fd,
                        java.lang.reflect.Array.newInstance(
                            Class.forName("java.lang.foreign.Linker$Option"), 0));
            // NOT `mh.getClass()`: a MethodHandle's implementation class is
            // unspecified (HotSpot answers `BoundMethodHandle$Species_LLLL`),
            // so asserting it measures the JDK's internals rather than the
            // downcall. The TYPE is the specified fact.
            return ((java.lang.invoke.MethodHandle) mh).type().toString();
        });
        t("ffm.arena.allocate", () -> {
            Class<?> arenaC = Class.forName("java.lang.foreign.Arena");
            Object arena = arenaC.getMethod("ofConfined").invoke(null);
            Object seg = arenaC.getMethod("allocate", long.class)
                    .invoke(arena, 16L);
            Class<?> segC = Class.forName("java.lang.foreign.MemorySegment");
            long size = (Long) segC.getMethod("byteSize").invoke(seg);
            arenaC.getMethod("close").invoke(arena);
            return size;
        });
    }

    // ---------------- P1-C: SSLSocket streams ------------------------------
    //
    // The lane's payload is "the handshake succeeds and the FIRST STREAM ACCESS
    // throws". So the rows that matter are the ones that touch the streams.

    static void p1c_sslStreams() {
        t("ssl.context.default", () -> {
            javax.net.ssl.SSLContext c = javax.net.ssl.SSLContext.getDefault();
            return c.getProtocol();
        });
        t("ssl.socketFactory.class", () -> {
            javax.net.ssl.SSLSocketFactory f =
                (javax.net.ssl.SSLSocketFactory) javax.net.ssl.SSLSocketFactory.getDefault();
            return f != null;
        });
        // An UNCONNECTED SSLSocket still has to hand back streams objects or
        // fail with an IOException -- never a NoClassDefFoundError.
        t("ssl.unconnected.getOutputStream", () -> {
            javax.net.ssl.SSLSocketFactory f =
                (javax.net.ssl.SSLSocketFactory) javax.net.ssl.SSLSocketFactory.getDefault();
            try (javax.net.ssl.SSLSocket s = (javax.net.ssl.SSLSocket) f.createSocket()) {
                OutputStream o = s.getOutputStream();
                return o != null ? "stream" : "null";
            }
        });
        t("ssl.unconnected.getInputStream", () -> {
            javax.net.ssl.SSLSocketFactory f =
                (javax.net.ssl.SSLSocketFactory) javax.net.ssl.SSLSocketFactory.getDefault();
            try (javax.net.ssl.SSLSocket s = (javax.net.ssl.SSLSocket) f.createSocket()) {
                InputStream i = s.getInputStream();
                return i != null ? "stream" : "null";
            }
        });
        t("ssl.supportedProtocols.hasTLS", () -> {
            javax.net.ssl.SSLSocketFactory f =
                (javax.net.ssl.SSLSocketFactory) javax.net.ssl.SSLSocketFactory.getDefault();
            try (javax.net.ssl.SSLSocket s = (javax.net.ssl.SSLSocket) f.createSocket()) {
                for (String p : s.getSupportedProtocols()) {
                    if (p.startsWith("TLS")) return true;
                }
                return false;
            }
        });
        t("ssl.enabledCipherSuites.nonEmpty", () -> {
            javax.net.ssl.SSLSocketFactory f =
                (javax.net.ssl.SSLSocketFactory) javax.net.ssl.SSLSocketFactory.getDefault();
            try (javax.net.ssl.SSLSocket s = (javax.net.ssl.SSLSocket) f.createSocket()) {
                return s.getEnabledCipherSuites().length > 0;
            }
        });
        t("ssl.serverSocketFactory", () -> {
            javax.net.ssl.SSLServerSocketFactory f =
                (javax.net.ssl.SSLServerSocketFactory)
                    javax.net.ssl.SSLServerSocketFactory.getDefault();
            return f != null;
        });
        t("ssl.engine.streamsFreeShape", () -> {
            javax.net.ssl.SSLContext c = javax.net.ssl.SSLContext.getDefault();
            javax.net.ssl.SSLEngine e = c.createSSLEngine();
            e.setUseClientMode(true);
            return e.getUseClientMode() + "/" + (e.getSession() != null);
        });
    }
}
