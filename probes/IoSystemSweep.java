import java.io.*;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.util.*;
import java.util.concurrent.*;

/** PrintStream, System, URL, Throwable, CopyOnWriteArrayList/Set,
 *  DataInput/OutputStream, HashSet and the ArrayList.subList / TreeMap.keySet
 *  views --- ~210 more rows off the bridge-kind retirement surface.
 *
 *  PrintStream is exercised over a ByteArrayOutputStream, never System.out:
 *  the point is what it WRITES, and routing it through the console would put
 *  the host's encoding in the diff. System reads only VM-independent facts.
 *  Values are hex-escaped so the diff cannot depend on stdout encoding. */
public class IoSystemSweep {
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
    interface ThrowingRun { void run() throws Exception; }

    static void printStream() throws Exception {
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        PrintStream ps = new PrintStream(bo, true, "UTF-8");
        ps.print("s"); ps.print(1); ps.print(2L); ps.print(1.5); ps.print(true);
        ps.print('c'); ps.print(new char[]{'x', 'y'}); ps.print((Object) null);
        ps.println();
        ps.println("line"); ps.println(7); ps.println(3.25); ps.println(false);
        ps.printf(Locale.ROOT, "%s-%d-%05.2f%n", "f", 9, 1.5);
        ps.format(Locale.ROOT, "[%3s|%-3s]%n", "a", "b");
        ps.append('A').append("BC").append("DEF", 1, 2);
        ps.write('Z');
        ps.flush();
        p("printStream bytes", bo.toString("UTF-8"));
        p("checkError", ps.checkError());
        ps.close();
        p("checkError after close", ps.checkError());
        // a PrintStream must SWALLOW an IOException from its sink, not throw
        PrintStream bad = new PrintStream(new OutputStream() {
            public void write(int b) throws IOException { throw new IOException("sink"); }
        });
        bad.println("x");
        p("swallows sink IOException", bad.checkError());
        t("null charset name", () -> new PrintStream(new ByteArrayOutputStream(), true, "no-such-cs"));
    }

    static void system() {
        p("lineSeparator is \\r\\n or \\n",
          System.lineSeparator().equals("\r\n") || System.lineSeparator().equals("\n"));
        p("getProperty java.version non-empty", !System.getProperty("java.version", "").isEmpty());
        p("getProperty absent default", System.getProperty("no.such.prop", "dflt"));
        p("getProperty absent null", System.getProperty("no.such.prop"));
        System.setProperty("probe.key", "probe.value");
        p("setProperty round-trip", System.getProperty("probe.key"));
        p("clearProperty returns old", System.clearProperty("probe.key"));
        p("cleared now null", System.getProperty("probe.key"));
        p("getenv absent", System.getenv("NO_SUCH_ENV_VAR_XYZ"));
        p("identityHashCode stable", System.identityHashCode(this0) == System.identityHashCode(this0));
        p("identityHashCode of null", System.identityHashCode(null));
        p("nanoTime monotone-ish", System.nanoTime() <= System.nanoTime());
        p("currentTimeMillis positive", System.currentTimeMillis() > 0);
        int[] src = {1, 2, 3, 4, 5}, dst = new int[5];
        System.arraycopy(src, 1, dst, 0, 3);
        p("arraycopy", Arrays.toString(dst));
        int[] ov = {1, 2, 3, 4, 5};
        System.arraycopy(ov, 0, ov, 1, 4);
        p("arraycopy overlapping", Arrays.toString(ov));
        t("arraycopy oob", () -> System.arraycopy(src, 0, dst, 0, 99));
        t("arraycopy type mismatch", () -> System.arraycopy(src, 0, new String[5], 0, 1));
        t("arraycopy null", () -> System.arraycopy(null, 0, dst, 0, 1));
        t("getProperty null key", () -> System.getProperty(null));
    }
    static final Object this0 = new Object();

    static void urls() throws Exception {
        String[] xs = { "http://host/path?q=1#f", "https://u:p@h:8443/a/b",
                        "file:/C:/tmp/f.txt", "jar:file:/a.jar!/b/C.class", "http://host" };
        for (String s : xs) {
            URL u;
            try { u = new URL(s); } catch (Exception e) { p("[" + s + "] CTOR", "THREW " + e.getClass().getName()); continue; }
            String k = "[" + s + "]";
            p(k + " getProtocol", u.getProtocol());
            p(k + " getHost", u.getHost());
            p(k + " getPort", u.getPort());
            p(k + " getDefaultPort", u.getDefaultPort());
            p(k + " getPath", u.getPath());
            p(k + " getFile", u.getFile());
            p(k + " getQuery", u.getQuery());
            p(k + " getRef", u.getRef());
            p(k + " getUserInfo", u.getUserInfo());
            p(k + " getAuthority", u.getAuthority());
            p(k + " toString", u.toString());
            p(k + " toExternalForm", u.toExternalForm());
            p(k + " toURI", safeToUri(u));
        }
        p("URL equals same", new URL("http://h/p").equals(new URL("http://h/p")));
        p("URL relative ctor", new URL(new URL("http://h/a/b"), "c").toString());
        p("URL relative absolute", new URL(new URL("http://h/a/b"), "/c").toString());
        t("URL bad protocol", () -> new URL("nosuchproto://h/"));
    }
    static String safeToUri(URL u) {
        try { return String.valueOf(u.toURI()); }
        catch (Throwable t) { return "THREW " + t.getClass().getName(); }
    }

    static void throwables() {
        Throwable t1 = new IllegalStateException("msg");
        p("getMessage", t1.getMessage());
        p("getLocalizedMessage", t1.getLocalizedMessage());
        p("toString", t1.toString());
        p("getCause", t1.getCause());
        Throwable t2 = new RuntimeException("outer", t1);
        p("cause toString", t2.getCause().toString());
        p("initCause on set", safeInit(t2, t1));
        Throwable t3 = new Exception();
        p("no-arg message", t3.getMessage());
        p("no-arg toString", t3.toString());
        t3.initCause(t1);
        p("initCause then getCause", t3.getCause().toString());
        t("initCause twice", () -> t3.initCause(t1));
        t("initCause self", () -> new Exception().initCause(null));
        Throwable sup = new Exception("main");
        sup.addSuppressed(new Exception("s1"));
        p("suppressed count", sup.getSuppressed().length);
        p("suppressed msg", sup.getSuppressed()[0].getMessage());
        t("addSuppressed self", () -> sup.addSuppressed(sup));
        StackTraceElement[] st = t1.getStackTrace();
        p("stack non-empty", st.length > 0);
        p("stack top is this class", st.length > 0 && st[0].getClassName().equals(IoSystemSweep.class.getName()));
        t1.setStackTrace(new StackTraceElement[0]);
        p("setStackTrace empty", t1.getStackTrace().length);
        p("fillInStackTrace returns this", t1.fillInStackTrace() == t1);
    }
    static String safeInit(Throwable t, Throwable c) {
        try { t.initCause(c); return "no-throw"; }
        catch (Throwable e) { return "THREW " + e.getClass().getName(); }
    }

    static void cowAndViews() {
        CopyOnWriteArrayList<String> cl = new CopyOnWriteArrayList<>(Arrays.asList("a", "b", "c"));
        p("cow list", cl);
        Iterator<String> it = cl.iterator();
        cl.add("d");
        StringBuilder sb = new StringBuilder();
        while (it.hasNext()) sb.append(it.next());
        p("cow list snapshot iterator", sb);
        p("cow list after add", cl);
        p("cow addIfAbsent", cl.addIfAbsent("a") + "/" + cl.addIfAbsent("e"));
        p("cow indexOf", cl.indexOf("c"));
        t("cow iterator remove", () -> cl.iterator().remove());

        CopyOnWriteArraySet<String> cs = new CopyOnWriteArraySet<>(Arrays.asList("a", "b"));
        p("cow set sorted", new TreeSet<>(cs));
        Iterator<String> si = cs.iterator();
        cs.add("c");
        int n = 0;
        while (si.hasNext()) { si.next(); n++; }
        p("cow set snapshot iterator count", n);
        p("cow set after add sorted", new TreeSet<>(cs));
        p("cow set duplicate add", cs.add("a"));
        p("cow set contains/size", cs.contains("b") + "/" + cs.size());
        t("cow set iterator remove", () -> cs.iterator().remove());

        List<String> base = new ArrayList<>(Arrays.asList("a", "b", "c", "d"));
        List<String> sl = base.subList(1, 3);
        p("subList", sl);
        sl.set(0, "B");
        p("subList set writes through", base);
        p("subList size", sl.size());
        p("subList indexOf", sl.indexOf("c"));
        base.add("e");
        t("subList after structural change", () -> sl.size());

        TreeMap<String, Integer> tm = new TreeMap<>();
        tm.put("a", 1); tm.put("b", 2); tm.put("c", 3);
        Set<String> ks = tm.keySet();
        p("keySet", ks);
        tm.put("d", 4);
        p("keySet is a view", ks);
        p("keySet remove writes through", ks.remove("a") + " -> " + tm);
        t("keySet add", () -> ks.add("z"));

        HashSet<String> hs = new HashSet<>(Arrays.asList("a", "b"));
        p("hashSet sorted", new TreeSet<>(hs));
        p("hashSet add dup", hs.add("a"));
        p("hashSet null allowed", hs.add(null) + "/" + hs.contains(null));
        p("hashSet size", hs.size());
        p("hashSet equals copy", hs.equals(new HashSet<>(hs)));
    }

    static void dataStreams() throws Exception {
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bo);
        out.writeBoolean(true); out.writeByte(0x7F); out.writeShort(0x1234);
        out.writeChar('Z'); out.writeInt(0x01020304); out.writeLong(0x0102030405060708L);
        out.writeFloat(1.5f); out.writeDouble(2.5d); out.writeUTF("utf\u00e9");
        out.writeBytes("ab"); out.writeChars("cd");
        out.flush();
        byte[] bytes = bo.toByteArray();
        p("DataOutput size()", out.size());
        p("DataOutput bytes hex", hex(bytes));
        DataInputStream in = new DataInputStream(new ByteArrayInputStream(bytes));
        p("readBoolean", in.readBoolean());
        p("readByte", in.readByte());
        p("readShort", in.readShort());
        p("readChar", in.readChar());
        p("readInt", in.readInt());
        p("readLong", in.readLong());
        p("readFloat", in.readFloat());
        p("readDouble", in.readDouble());
        p("readUTF", in.readUTF());
        p("available>0", in.available() > 0);
        byte[] two = new byte[2];
        in.readFully(two);
        p("readFully", new String(two, StandardCharsets.US_ASCII));
        p("skipBytes", in.skipBytes(2));
        t("readInt at EOF", () -> new DataInputStream(new ByteArrayInputStream(new byte[1])).readInt());
        t("readFully short", () -> new DataInputStream(new ByteArrayInputStream(new byte[1])).readFully(new byte[4]));
        t("readUTF garbage", () -> new DataInputStream(new ByteArrayInputStream(new byte[]{0, 5, 1})).readUTF());
    }
    static String hex(byte[] b) {
        StringBuilder s = new StringBuilder();
        for (byte x : b) s.append(String.format("%02x", x));
        return s.toString();
    }

    public static void main(String[] a) throws Exception {
        printStream();
        system();
        urls();
        throwables();
        cowAndViews();
        dataStreams();
        System.out.println("DONE IoSystemSweep");
    }
}
