import java.io.*;
import java.nio.ByteBuffer;
import java.nio.channels.AsynchronousFileChannel;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
import java.util.function.*;

/** The NINE Phase 1 lanes from `docs/feature-designs/jdk-only-completion-
 *  roadmap.md`, asked of the running VM.
 *
 *  The mechanism the roadmap names is one shape: a native registered in the
 *  ESSENTIAL set survives strict mode, runs, asks for a FABRICATED receiver,
 *  gets the refusal `--jdk-only` exists to give, and dies as
 *  `NoClassDefFoundError` at the application's call site. The refusal is
 *  correct; the survival of its caller is the defect.
 *
 *  So every lane below is written to produce that signature if it is still
 *  open: a `NoClassDefFoundError` (or a `cratonvm/internal/...` class name)
 *  where HotSpot answers an ordinary value.
 *
 *    P1-A  Atomic*FieldUpdater.newUpdater  -> the whole java.sql package
 *    P1-B  System.getenv() no-arg          -> Spring's AbstractEnvironment
 *    P1-C  SSLSocket.get{Out,In}putStream  -> no HTTPS
 *    P1-D  Consumer.andThen / Predicate.and|or|negate
 *    P1-E  Linker.downcallHandle           -> all of Panama/FFM
 *    P1-F  ConcurrentHashMap.keys()/elements()
 *    P1-G  Hashtable.keys()/elements()
 *    P1-H  AsynchronousFileChannel read/write
 *    P1-I  Runtime.exec, all six overloads
 *
 *  DETERMINISM: no network is dialled -- the P1-C lane builds the socket
 *  factory and asks for the stream on an UNCONNECTED socket, which is where
 *  the fabrication happens; a connect would put a remote host in the diff.
 *  Nothing prints an identity hash, a path, a thread name or a timing.
 */
public class Phase1Sweep {
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    /** Prints the THROWN TYPE, which is the whole signal for this probe. */
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Throwable; }
    /** A class NAME is the other signal: a `cratonvm/internal/...` answer is a
     *  fabricated receiver even when nothing throws. */
    static String cls(Object o) { return o == null ? "null" : o.getClass().getName(); }

    // ---- P1-A: the updaters, and java.sql behind them ------------------
    static class Node { volatile String s = "a"; volatile int i = 1; }
    static void p1a() {
        t("P1-A ref newUpdater", () ->
            AtomicReferenceFieldUpdater.newUpdater(Node.class, String.class, "s"));
        t("P1-A int newUpdater", () ->
            AtomicIntegerFieldUpdater.newUpdater(Node.class, "i"));
        try {
            AtomicReferenceFieldUpdater<Node, String> u =
                AtomicReferenceFieldUpdater.newUpdater(Node.class, String.class, "s");
            p("P1-A updater class", cls(u));
            Node n = new Node();
            p("P1-A updater get", u.get(n));
            p("P1-A updater cas", u.compareAndSet(n, "a", "b"));
        } catch (Throwable e) {
            p("P1-A updater use", "THREW " + e.getClass().getName());
        }
        // THE CLAIM: java.sql.SQLException holds a static
        // AtomicReferenceFieldUpdater, so a refused fabrication makes the whole
        // java.sql package unloadable.
        t("P1-A load java.sql.SQLException", () -> Class.forName("java.sql.SQLException"));
        t("P1-A load java.sql.DriverManager", () -> Class.forName("java.sql.DriverManager"));
        t("P1-A new SQLException", () -> new java.sql.SQLException("x"));
        try {
            java.sql.SQLException a = new java.sql.SQLException("first");
            java.sql.SQLException b = new java.sql.SQLException("second");
            a.setNextException(b);                       // the updater's own path
            p("P1-A SQLException chain", a.getNextException().getMessage());
            int n = 0;
            for (Throwable x : a) n++;
            p("P1-A SQLException iterator count", n);
            p("P1-A SQLException message", a.getMessage());
        } catch (Throwable e) {
            p("P1-A SQLException chain", "THREW " + e.getClass().getName());
        }
    }

    // ---- P1-B: System.getenv() -----------------------------------------
    static void p1b() {
        try {
            Map<String, String> env = System.getenv();
            p("P1-B getenv class", cls(env));
            p("P1-B getenv non-empty", !env.isEmpty());
            p("P1-B getenv is a Map", env instanceof Map);
            // Spring's AbstractEnvironment does exactly this pair.
            p("P1-B getenv keySet class ok", env.keySet() != null);
            p("P1-B getenv entrySet iterable", env.entrySet().iterator() != null);
            t("P1-B getenv put refused", () -> env.put("k", "v"));
            t("P1-B getenv remove refused", () -> env.remove("PATH"));
            t("P1-B getenv clear refused", () -> env.clear());
            p("P1-B getenv containsKey absent", env.containsKey("NO_SUCH_VAR_XYZ"));
            p("P1-B getenv get absent", env.get("NO_SUCH_VAR_XYZ"));
            p("P1-B getenv(String) absent", System.getenv("NO_SUCH_VAR_XYZ"));
        } catch (Throwable e) {
            p("P1-B getenv", "THREW " + e.getClass().getName());
        }
    }

    // ---- P1-C: SSLSocket streams ---------------------------------------
    static void p1c() {
        try {
            javax.net.ssl.SSLSocketFactory f =
                (javax.net.ssl.SSLSocketFactory) javax.net.ssl.SSLSocketFactory.getDefault();
            p("P1-C factory class ok", f != null);
            javax.net.ssl.SSLSocket s = (javax.net.ssl.SSLSocket) f.createSocket();
            p("P1-C unconnected socket built", s != null);
            p("P1-C isConnected", s.isConnected());
            p("P1-C supported protocols non-empty", s.getSupportedProtocols().length > 0);
            p("P1-C enabled protocols non-empty", s.getEnabledProtocols().length > 0);
            p("P1-C supported ciphers non-empty", s.getSupportedCipherSuites().length > 0);
            // The stream accessors are the lane. On an UNCONNECTED socket the
            // JDK throws SocketException -- a fabricated stream would instead
            // come back, or die as NoClassDefFoundError.
            t("P1-C getOutputStream unconnected", () -> s.getOutputStream());
            t("P1-C getInputStream unconnected", () -> s.getInputStream());
            s.close();
            p("P1-C closed", s.isClosed());
        } catch (Throwable e) {
            p("P1-C", "THREW " + e.getClass().getName());
        }
        // A plain socket, as the control: if THIS is broken the lane is not
        // SSL-specific and the P1-C diagnosis is wrong.
        try (java.net.Socket plain = new java.net.Socket()) {
            t("P1-C plain getOutputStream unconnected", () -> plain.getOutputStream());
        } catch (Throwable e) {
            p("P1-C plain socket", "THREW " + e.getClass().getName());
        }
    }

    // ---- P1-D: default methods on the functional interfaces ------------
    static void p1d() {
        try {
            StringBuilder sink = new StringBuilder();
            Consumer<String> one = sink::append;
            Consumer<String> both = one.andThen(x -> sink.append(x.toUpperCase()));
            p("P1-D andThen class is a Consumer", both instanceof Consumer);
            both.accept("ab");
            p("P1-D andThen ran both", sink.toString());
        } catch (Throwable e) {
            p("P1-D Consumer.andThen", "THREW " + e.getClass().getName());
        }
        try {
            Predicate<String> notEmpty = x -> !x.isEmpty();
            Predicate<String> shortish = x -> x.length() < 5;
            p("P1-D and", notEmpty.and(shortish).test("abc"));
            p("P1-D and false", notEmpty.and(shortish).test("abcdefgh"));
            p("P1-D or", notEmpty.or(shortish).test(""));
            p("P1-D negate", notEmpty.negate().test(""));
            p("P1-D isEqual", Predicate.isEqual("x").test("x"));
            p("P1-D not", Predicate.not(notEmpty).test(""));
        } catch (Throwable e) {
            p("P1-D Predicate", "THREW " + e.getClass().getName());
        }
        try {
            Function<Integer, Integer> f = x -> x + 1;
            p("P1-D andThen fn", f.andThen(x -> x * 2).apply(3));
            p("P1-D compose fn", f.compose((Integer x) -> x * 2).apply(3));
            p("P1-D identity", Function.identity().apply("i"));
            p("P1-D BiFunction andThen",
              ((java.util.function.BiFunction<Integer, Integer, Integer>) Integer::sum)
                  .andThen(x -> x * 10).apply(1, 2));
            p("P1-D Supplier", ((Supplier<String>) () -> "s").get());
            p("P1-D UnaryOperator identity", UnaryOperator.identity().apply("u"));
            // and through a stream, which is where it actually bites
            p("P1-D stream with composed predicate",
              Arrays.asList("", "a", "bb", "ccccc").stream()
                    .filter(((Predicate<String>) x -> !x.isEmpty()).and(x -> x.length() < 5))
                    .count());
        } catch (Throwable e) {
            p("P1-D Function", "THREW " + e.getClass().getName());
        }
    }

    // ---- P1-E: Panama / FFM --------------------------------------------
    static void p1e() {
        try {
            Class<?> linker = Class.forName("java.lang.foreign.Linker");
            p("P1-E Linker loads", linker != null);
            Object nat = linker.getMethod("nativeLinker").invoke(null);
            p("P1-E nativeLinker non-null", nat != null);
            Class<?> fd = Class.forName("java.lang.foreign.FunctionDescriptor");
            Class<?> vl = Class.forName("java.lang.foreign.ValueLayout");
            Object jInt = vl.getField("JAVA_INT").get(null);
            Object desc = fd.getMethod("of", Class.forName("[Ljava.lang.foreign.MemoryLayout;")
                    .getComponentType(),
                    Class.forName("[Ljava.lang.foreign.MemoryLayout;"))
                .invoke(null, jInt, java.lang.reflect.Array.newInstance(
                    Class.forName("java.lang.foreign.MemoryLayout"), 0));
            p("P1-E FunctionDescriptor built", desc != null);
        } catch (Throwable e) {
            p("P1-E FFM", "THREW " + e.getClass().getName());
        }
        // The simplest FFM shape that does not need a real symbol: an arena and
        // a segment. If the fabrication lane is open this is where it shows.
        try {
            Class<?> arenaC = Class.forName("java.lang.foreign.Arena");
            Object arena = arenaC.getMethod("ofConfined").invoke(null);
            p("P1-E Arena class", cls(arena));
            Object seg = arenaC.getMethod("allocate", long.class).invoke(arena, 16L);
            p("P1-E segment class", cls(seg));
            // Through the INTERFACE, not the impl class: `NativeMemorySegmentImpl`
            // is not exported, so `seg.getClass().getMethod(..)` reflects an
            // inaccessible member and dies with IllegalAccessException on
            // HotSpot too. That would have been a probe bug reported as a VM
            // difference.
            p("P1-E segment byteSize",
              Class.forName("java.lang.foreign.MemorySegment")
                   .getMethod("byteSize").invoke(seg));
            arenaC.getMethod("close").invoke(arena);
            p("P1-E arena closed", true);
        } catch (Throwable e) {
            p("P1-E Arena", "THREW " + e.getClass().getName()
              + (e.getCause() == null ? "" : " cause " + e.getCause().getClass().getName()));
        }
    }

    // ---- P1-F / P1-G: the two Enumeration lanes ------------------------
    static void p1f() {
        ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
        m.put("a", 1); m.put("b", 2); m.put("c", 3);
        drain("P1-F chm keys", m.keys());
        drain("P1-F chm elements", m.elements());
        Hashtable<String, Integer> h = new Hashtable<>();
        h.put("a", 1); h.put("b", 2); h.put("c", 3);
        drain("P1-G hashtable keys", h.keys());
        drain("P1-G hashtable elements", h.elements());
        Vector<String> v = new Vector<>(Arrays.asList("x", "y"));
        drain("P1-G vector elements", v.elements());
        drain("P1-G Collections.enumeration", Collections.enumeration(Arrays.asList("p", "q")));
        drain("P1-G empty enumeration", Collections.emptyEnumeration());
    }
    /** Drains with a HARD CAP: the failure this is looking for is an
     *  enumeration that never says false, and an uncapped drain would hang the
     *  probe instead of reporting. */
    static void drain(String tag, Enumeration<?> e) {
        try {
            List<String> got = new ArrayList<>();
            int guard = 0;
            while (e.hasMoreElements()) {
                got.add(String.valueOf(e.nextElement()));
                if (++guard > 64) { p(tag, "NEVER TERMINATED"); return; }
            }
            Collections.sort(got);
            p(tag, got.toString());
        } catch (Throwable x) {
            p(tag, "THREW " + x.getClass().getName());
        }
    }

    // ---- P1-H: AsynchronousFileChannel ---------------------------------
    static void p1h() {
        Path f = null;
        try {
            f = Files.createTempFile("p1h", ".bin");
            final Path path = f;
            try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(
                    path, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
                p("P1-H channel class", cls(ch));
                ByteBuffer w = ByteBuffer.wrap(new byte[]{1, 2, 3, 4});
                Future<Integer> fw = ch.write(w, 0);
                p("P1-H write future class", cls(fw));
                p("P1-H wrote", fw.get(30, TimeUnit.SECONDS));
                p("P1-H future isDone", fw.isDone());
                p("P1-H future isCancelled", fw.isCancelled());
                ch.force(true);
                ByteBuffer r = ByteBuffer.allocate(4);
                Future<Integer> fr = ch.read(r, 0);
                p("P1-H read", fr.get(30, TimeUnit.SECONDS));
                p("P1-H bytes", Arrays.toString(r.array()));
                p("P1-H size", ch.size());
            }
        } catch (Throwable e) {
            p("P1-H", "THREW " + e.getClass().getName()
              + (e.getCause() == null ? "" : " cause " + e.getCause().getClass().getName()));
        } finally {
            try { if (f != null) Files.deleteIfExists(f); } catch (IOException ignored) { }
        }
    }

    // ---- P1-I: Runtime.exec, all six overloads -------------------------
    /** The roadmap says this row CARRIES ITS OWN CONTROL: `ProcessBuilder.start`
     *  shares the same mint and was re-tagged `SyntheticStub`, so it passes,
     *  while the six ambient-`Bridge` `Runtime.exec` overloads fail. The pinned
     *  half and its twin, in one run.
     *
     *  Only the EXIT CODE is printed. A spawned process's stdout carries the
     *  host's shell, code page and locale into the diff; an exit code the child
     *  was told to produce carries none of that. */
    static void p1i() {
        String[] argv = winOrUnix();
        String cmdline = String.join(" ", argv);
        File cwd = new File(System.getProperty("java.io.tmpdir"));
        String[] env = {"CRATONVM_PROBE_VAR=1"};
        Runtime rt = Runtime.getRuntime();

        exitOf("P1-I exec(String)", () -> rt.exec(cmdline));
        exitOf("P1-I exec(String[])", () -> rt.exec(argv));
        exitOf("P1-I exec(String,String[])", () -> rt.exec(cmdline, env));
        exitOf("P1-I exec(String[],String[])", () -> rt.exec(argv, env));
        exitOf("P1-I exec(String,String[],File)", () -> rt.exec(cmdline, env, cwd));
        exitOf("P1-I exec(String[],String[],File)", () -> rt.exec(argv, env, cwd));
        // THE CONTROL: same mint, re-tagged, must pass whatever the six do.
        exitOf("P1-I ProcessBuilder.start", () -> new ProcessBuilder(argv).start());

        // The refusals, which must not become fabrications either.
        t("P1-I exec empty string", () -> rt.exec(""));
        t("P1-I exec null string", () -> rt.exec((String) null));
        t("P1-I exec empty array", () -> rt.exec(new String[0]));
    }
    /** `cmd /c exit 7` on Windows, `sh -c exit 7` elsewhere: a child that is
     *  told exactly what to return, so the assertion is the VM's plumbing and
     *  not the host's shell. */
    static String[] winOrUnix() {
        String os = System.getProperty("os.name", "").toLowerCase(Locale.ROOT);
        return os.contains("win") ? new String[]{"cmd", "/c", "exit 7"}
                                  : new String[]{"sh", "-c", "exit 7"};
    }
    interface ProcSupplier { Process get() throws Exception; }
    static void exitOf(String tag, ProcSupplier s) {
        Process pr = null;
        try {
            pr = s.get();
            if (!pr.waitFor(60, TimeUnit.SECONDS)) { p(tag, "DID NOT EXIT"); return; }
            p(tag, "exit " + pr.exitValue());
        } catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName());
        } finally {
            if (pr != null && pr.isAlive()) pr.destroyForcibly();
        }
    }

    public static void main(String[] a) {
        p1a();
        p1b();
        p1c();
        p1d();
        p1e();
        p1f();
        p1h();
        p1i();
        System.out.println("DONE Phase1Sweep");
    }
}
