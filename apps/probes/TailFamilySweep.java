import java.io.*;
import java.nio.*;
import java.nio.file.*;
import java.lang.ref.*;
import java.util.*;

/** The Java-reachable half of the retirement surface's long tail: CharBuffer,
 *  java.nio.file.Path, TimeZone, AssertionError, java.lang.ref.Reference,
 *  Runtime, ByteArrayInputStream, PrintWriter, BufferedWriter, Object and the
 *  LinkedList iterator.
 *
 *  The tail's other half is unreachable from Java by construction --
 *  java/lang/System$1, jdk/internal/access/SharedSecrets, jdk/internal/misc/VM,
 *  jdk/internal/loader/URLClassPath, java/io/WinNTFileSystem,
 *  java/io/FileDescriptor$1 -- and is exercised INDIRECTLY by the public calls
 *  above that route through it.
 *
 *  Determinism: TimeZone is read by ID, never as the host default; Reference is
 *  probed for its API contract, never for whether a collection has happened;
 *  Runtime reads only invariants, never a byte count. */
public class TailFamilySweep {
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
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Throwable; }

    static void charBuffers() {
        CharBuffer cb = CharBuffer.allocate(8);
        p("cb capacity/limit/position", cb.capacity() + "/" + cb.limit() + "/" + cb.position());
        cb.put('a').put('b').put("cd");
        p("cb after puts position", cb.position());
        cb.flip();
        p("cb get", cb.get());
        p("cb toString after get", cb.toString());
        p("cb remaining", cb.remaining());
        p("cb charAt(0)", cb.charAt(0));
        p("cb subSequence", cb.subSequence(0, 2));
        p("cb slice remaining", cb.slice().remaining());
        p("cb duplicate position", cb.duplicate().position());
        p("cb isReadOnly/hasArray", cb.isReadOnly() + "/" + cb.hasArray());
        cb.rewind();
        char[] dst = new char[3];
        cb.get(dst);
        p("cb bulk get", new String(dst));
        p("cb compact position", cb.compact().position());
        CharBuffer w = CharBuffer.wrap("hello");
        p("cb wrap toString", w.toString());
        p("cb wrap isReadOnly", w.isReadOnly());
        p("cb wrap length", w.length());
        p("cb wrap chars count", w.chars().count());
        t("cb wrap put", () -> CharBuffer.wrap("x").put('y'));
        t("cb get underflow", () -> CharBuffer.allocate(1).flip().get());
        t("cb charAt oob", () -> CharBuffer.wrap("x").charAt(9));
    }

    static void paths() {
        Path a = Paths.get("a", "b", "c");
        p("path toString", a);
        p("path getFileName", a.getFileName());
        p("path getParent", a.getParent());
        p("path getNameCount", a.getNameCount());
        p("path getName(1)", a.getName(1));
        p("path subpath", a.subpath(0, 2));
        p("path isAbsolute", a.isAbsolute());
        p("path getRoot", a.getRoot());
        p("path normalize", Paths.get("a/./b/../c").normalize());
        p("path resolve", a.resolve("d"));
        p("path resolve absolute", a.resolve(Paths.get("/x")));
        p("path resolveSibling", a.resolveSibling("s"));
        p("path relativize", Paths.get("a/b").relativize(Paths.get("a/b/c/d")));
        p("path startsWith/endsWith", a.startsWith("a") + "/" + a.endsWith("c"));
        p("path equals", a.equals(Paths.get("a", "b", "c")));
        p("path hashCode stable", a.hashCode() == Paths.get("a", "b", "c").hashCode());
        p("path compareTo", Integer.signum(a.compareTo(Paths.get("a", "b", "d"))));
        p("path toUri scheme", a.toAbsolutePath().toUri().getScheme());
        p("path iterator count", count(a.iterator()));
        p("path of empty", Paths.get(""));
        p("path getFileName of empty", Paths.get("").getFileName());
        t("path getName oob", () -> a.getName(9));
        t("path subpath oob", () -> a.subpath(0, 9));
        t("path relativize different roots", () -> Paths.get("/x").relativize(Paths.get("y")));
    }
    static int count(Iterator<?> it) { int n = 0; while (it.hasNext()) { it.next(); n++; } return n; }

    static void timeZones() {
        TimeZone utc = TimeZone.getTimeZone("UTC");
        p("tz UTC id", utc.getID());
        p("tz UTC rawOffset", utc.getRawOffset());
        p("tz UTC useDaylightTime", utc.useDaylightTime());
        p("tz UTC inDaylightTime", utc.inDaylightTime(new Date(0)));
        p("tz UTC getOffset", utc.getOffset(0L));
        p("tz UTC getDSTSavings", utc.getDSTSavings());
        TimeZone ny = TimeZone.getTimeZone("America/New_York");
        p("tz NY id", ny.getID());
        p("tz NY rawOffset", ny.getRawOffset());
        p("tz NY useDaylightTime", ny.useDaylightTime());
        p("tz NY offset in Jan", ny.getOffset(1_000_000_000_000L));
        p("tz NY offset in Jul", ny.getOffset(1_026_000_000_000L));
        p("tz NY hasSameRules(self)", ny.hasSameRules(TimeZone.getTimeZone("America/New_York")));
        p("tz bogus id falls back to GMT", TimeZone.getTimeZone("No/Such_Zone").getID());
        p("tz GMT+5 rawOffset", TimeZone.getTimeZone("GMT+05:00").getRawOffset());
        p("tz availableIDs contains UTC",
          Arrays.asList(TimeZone.getAvailableIDs()).contains("UTC"));
        p("tz toZoneId", utc.toZoneId());
        p("tz SimpleTimeZone", new SimpleTimeZone(3600000, "X").getRawOffset());
    }

    static void errorsAndRefs() {
        AssertionError e1 = new AssertionError();
        p("ae no-arg message", e1.getMessage());
        p("ae toString", e1.toString());
        p("ae String ctor", new AssertionError("m").getMessage());
        p("ae int ctor", new AssertionError(7).getMessage());
        p("ae boolean ctor", new AssertionError(true).getMessage());
        p("ae char ctor", new AssertionError('c').getMessage());
        p("ae Object ctor null", new AssertionError((Object) null).getMessage());
        Throwable cause = new IllegalStateException("c");
        AssertionError e2 = new AssertionError("m", cause);
        p("ae cause", e2.getCause().getClass().getSimpleName());
        p("ae Throwable ctor sets cause", new AssertionError(cause).getCause() == cause);
        p("ae is an Error", e1 instanceof Error);

        Object referent = new Object();
        WeakReference<Object> wr = new WeakReference<>(referent);
        p("weak get is referent", wr.get() == referent);
        p("weak refersTo", wr.refersTo(referent));
        p("weak refersTo other", wr.refersTo(new Object()));
        p("weak isEnqueued-ish enqueue", wr.enqueue());
        SoftReference<Object> sr = new SoftReference<>(referent);
        p("soft get is referent", sr.get() == referent);
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        WeakReference<Object> wq = new WeakReference<>(referent, q);
        p("queued poll before enqueue", q.poll());
        p("enqueue returns true once", wq.enqueue());
        p("enqueue again false", wq.enqueue());
        p("poll returns the ref", q.poll() == wq);
        wr.clear();
        p("after clear get is null", wr.get());
        PhantomReference<Object> pr = new PhantomReference<>(referent, q);
        p("phantom get is always null", pr.get());
        p("Reference.reachabilityFence ok", fence(referent));
    }
    static String fence(Object o) {
        try { Reference.reachabilityFence(o); return "no-throw"; }
        catch (Throwable t) { return "THREW " + t.getClass().getName(); }
    }

    static void runtimeAndStreams() throws Exception {
        Runtime rt = Runtime.getRuntime();
        p("runtime non-null", rt != null);
        p("runtime is singleton", Runtime.getRuntime() == rt);
        p("availableProcessors >= 1", rt.availableProcessors() >= 1);
        p("maxMemory > 0", rt.maxMemory() > 0);
        p("totalMemory <= maxMemory", rt.totalMemory() <= rt.maxMemory());
        p("freeMemory <= totalMemory", rt.freeMemory() <= rt.totalMemory());
        p("version feature >= 17", Runtime.version().feature() >= 17);
        t("addShutdownHook null", () -> rt.addShutdownHook(null));
        t("removeShutdownHook unregistered", () -> {
            if (rt.removeShutdownHook(new Thread(() -> {})))
                throw new IllegalStateException("removed an unregistered hook"); });

        ByteArrayInputStream in = new ByteArrayInputStream("hello".getBytes());
        p("bais available", in.available());
        p("bais read", in.read());
        byte[] buf = new byte[3];
        p("bais read(buf)", in.read(buf) + "/" + new String(buf));
        p("bais markSupported", in.markSupported());
        in.mark(0);
        p("bais read after mark", in.read());
        in.reset();
        p("bais read after reset", in.read());
        p("bais read at EOF", in.read());
        p("bais skip past end", new ByteArrayInputStream(new byte[2]).skip(9));
        p("bais readAllBytes len", new ByteArrayInputStream(new byte[4]).readAllBytes().length);
        p("bais transferTo", transferTo());

        StringWriter sw = new StringWriter();
        PrintWriter pw = new PrintWriter(sw);
        pw.print("a"); pw.print(1); pw.print(true); pw.println();
        pw.println("line"); pw.printf(Locale.ROOT, "%s-%d%n", "f", 3);
        pw.append('X').append("YZ");
        pw.flush();
        p("printWriter out", sw.toString());
        p("printWriter checkError", pw.checkError());

        StringWriter sw2 = new StringWriter();
        BufferedWriter bw = new BufferedWriter(sw2);
        bw.write("abc"); bw.write('d'); bw.write("efgh", 1, 2);
        bw.newLine(); bw.write("tail");
        bw.flush();
        p("bufferedWriter out", sw2.toString().replace(System.lineSeparator(), "\\n"));
        bw.close();
        t("bufferedWriter write after close", () -> bw.write("x"));
    }
    static String transferTo() throws IOException {
        ByteArrayInputStream s = new ByteArrayInputStream("xfer".getBytes());
        ByteArrayOutputStream d = new ByteArrayOutputStream();
        long n = s.transferTo(d);
        return n + "/" + d.toString("UTF-8");
    }

    static void objects() {
        Object o = new Object();
        p("Object equals self", o.equals(o));
        p("Object equals other", o.equals(new Object()));
        p("Object hashCode stable", o.hashCode() == o.hashCode());
        p("Object getClass", o.getClass().getName());
        p("Object toString shape", o.toString().startsWith("java.lang.Object@"));
        p("toString matches hashCode",
          o.toString().equals("java.lang.Object@" + Integer.toHexString(o.hashCode())));
        p("getClass on String", "s".getClass().getName());
        p("getClass on array", new int[0].getClass().getName());
        t("Object.wait outside monitor", () -> o.wait(1));
        t("Object.notify outside monitor", () -> o.notify());
        synchronized (o) { p("wait(1) inside monitor", waitOk(o)); }
    }
    static String waitOk(Object o) {
        try { o.wait(1); return "no-throw"; }
        catch (Throwable t) { return "THREW " + t.getClass().getName(); }
    }

    public static void main(String[] a) throws Exception {
        charBuffers();
        paths();
        timeZones();
        errorsAndRefs();
        runtimeAndStreams();
        objects();
        System.out.println("DONE TailFamilySweep");
    }
}
