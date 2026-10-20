import java.io.*;
import java.lang.foreign.*;
import java.lang.invoke.*;
import java.lang.constant.*;
import java.lang.management.*;
import java.nio.*;
import java.nio.file.*;
import java.text.*;
import java.time.*;
import java.time.format.*;
import java.util.*;
import java.util.concurrent.*;
import javax.crypto.spec.GCMParameterSpec;

/**
 * SBR2 W7 TAIL family probe: ByteArrayInputStream, GCMParameterSpec, ResourceBundle,
 * CalendarDataUtility (through java.time/Calendar names), System.Logger, ClassDesc concat,
 * FileSystems.getDefault, buffers/Bits/segments, byte CAS, allocateMemory.
 * Every row is one line "name = result"; a second round of the same rows prints "drift" lines if
 * warmed-up behaviour differs from the cold one.
 */
public class Sbr2W7TailFam {
    static final Map<String, String> cold = new LinkedHashMap<>();
    static boolean second;
    static int drift;

    static void row(String n, Object v) {
        String s = String.valueOf(v);
        if (!second) { cold.put(n, s); System.out.println(n + " = " + s); }
        else if (!s.equals(cold.get(n))) { drift++; System.out.println("DRIFT " + n + " = " + s + " (cold " + cold.get(n) + ")"); }
    }
    static String ex(Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }

    interface T { Object run() throws Throwable; }
    static void t(String n, T r) {
        // segment-backed rows are opt-in (TAIL_SEG=1): they crash some builds outright, and a crash
        // hides every row after it
        if (n.startsWith("seg.") && System.getenv("TAIL_SEG") == null) return;
        try { row(n, r.run()); } catch (Throwable e) { row(n, "EX " + ex(e)); }
    }

    public static class Bundle extends ListResourceBundle {
        protected Object[][] getContents() { return new Object[][] {{"k", "v"}, {"only.base", "b"}}; }
    }
    public static class Bundle_fr extends ListResourceBundle {
        protected Object[][] getContents() { return new Object[][] {{"k", "v-fr"}}; }
    }
    public static class Bundle_fr_CA extends ListResourceBundle {
        protected Object[][] getContents() { return new Object[][] {{"k2", "v-fr-ca"}}; }
    }
    static volatile byte vb;
    static class BH { volatile byte b = 3; volatile short s = 7; volatile boolean f; volatile char c = 'a'; }

    static void all() throws Throwable {
        // ---- ByteArrayInputStream
        t("bais.read()", () -> { var in = new ByteArrayInputStream(new byte[]{1, (byte) 0xff, 3}); return in.read() + "," + in.read() + "," + in.read() + "," + in.read(); });
        t("bais.read(b,o,l)", () -> { var in = new ByteArrayInputStream("hello world".getBytes()); byte[] b = new byte[8]; int n = in.read(b, 2, 5); return n + ":" + Arrays.toString(b) + " avail=" + in.available(); });
        t("bais.read(b,o,l) eof", () -> { var in = new ByteArrayInputStream(new byte[0]); return in.read(new byte[4], 0, 4) + "/" + in.read(new byte[4], 0, 0); });
        t("bais.read null", () -> new ByteArrayInputStream(new byte[3]).read(null, 0, 1));
        t("bais.read neg off", () -> new ByteArrayInputStream(new byte[3]).read(new byte[3], -1, 1));
        t("bais.read big len", () -> new ByteArrayInputStream(new byte[3]).read(new byte[3], 1, 5));
        t("bais.read neg len", () -> new ByteArrayInputStream(new byte[3]).read(new byte[3], 0, -1));
        t("bais.close+read", () -> { var in = new ByteArrayInputStream(new byte[]{9}); in.close(); return in.read(); });
        t("bais.mark/reset", () -> { var in = new ByteArrayInputStream(new byte[]{1, 2, 3, 4}); in.read(); in.mark(0); in.read(); in.read(); in.reset(); return in.read(); });
        t("bais.skip", () -> { var in = new ByteArrayInputStream(new byte[]{1, 2, 3, 4}); return in.skip(3) + "," + in.skip(9) + "," + in.skip(-1); });
        t("bais.readAllBytes", () -> Arrays.toString(new ByteArrayInputStream(new byte[]{1, 2, 3}).readAllBytes()));
        t("bais.readNBytes", () -> Arrays.toString(new ByteArrayInputStream(new byte[]{1, 2, 3}).readNBytes(2)));
        t("bais.transferTo", () -> { var o = new ByteArrayOutputStream(); long n = new ByteArrayInputStream(new byte[]{1, 2, 3}).transferTo(o); return n + Arrays.toString(o.toByteArray()); });
        t("bais.range ctor", () -> { var in = new ByteArrayInputStream(new byte[]{1, 2, 3, 4, 5}, 1, 3); return Arrays.toString(in.readAllBytes()); });
        t("bais.stream via reader", () -> new BufferedReader(new InputStreamReader(new ByteArrayInputStream("l1\nl2\n".getBytes()))).lines().toList());
        // ---- GCMParameterSpec
        t("gcm.basic", () -> { var g = new GCMParameterSpec(128, new byte[]{1, 2, 3, 4}); return g.getTLen() + Arrays.toString(g.getIV()); });
        t("gcm.offset", () -> { var g = new GCMParameterSpec(96, new byte[]{1, 2, 3, 4, 5}, 1, 3); return g.getTLen() + Arrays.toString(g.getIV()); });
        t("gcm.iv copy", () -> { byte[] iv = {1, 2}; var g = new GCMParameterSpec(64, iv); iv[0] = 9; byte[] r = g.getIV(); r[1] = 8; return Arrays.toString(g.getIV()); });
        t("gcm.neg tlen", () -> new GCMParameterSpec(-1, new byte[2]).getTLen());
        t("gcm.null iv", () -> new GCMParameterSpec(128, null).getTLen());
        t("gcm.bad range", () -> new GCMParameterSpec(128, new byte[2], 1, 5).getTLen());
        t("gcm.neg off", () -> new GCMParameterSpec(128, new byte[2], -1, 1).getTLen());
        t("gcm.cipher", () -> {
            var c = javax.crypto.Cipher.getInstance("AES/GCM/NoPadding");
            var k = new javax.crypto.spec.SecretKeySpec(new byte[16], "AES");
            c.init(javax.crypto.Cipher.ENCRYPT_MODE, k, new GCMParameterSpec(128, new byte[12]));
            c.updateAAD(new byte[]{1});
            byte[] ct = c.doFinal("hello".getBytes());
            c.init(javax.crypto.Cipher.DECRYPT_MODE, k, new GCMParameterSpec(128, new byte[12]));
            c.updateAAD(new byte[]{1});
            return ct.length + ":" + new String(c.doFinal(ct)) + ":" + c.getParameters().getParameterSpec(GCMParameterSpec.class).getTLen();
        });
        // ---- ResourceBundle
        t("rb.base", () -> ResourceBundle.getBundle("Sbr2W7TailFam$Bundle", Locale.ROOT).getString("k"));
        t("rb.fr", () -> { var b = ResourceBundle.getBundle("Sbr2W7TailFam$Bundle", Locale.FRENCH); return b.getString("k") + "/" + b.getString("only.base") + "/" + b.getLocale(); });
        t("rb.fr_CA", () -> { var b = ResourceBundle.getBundle("Sbr2W7TailFam$Bundle", Locale.CANADA_FRENCH); return b.getString("k2") + "/" + b.getString("k") + "/" + b.getString("only.base") + "/[" + b.getLocale() + "]/" + b.keySet().stream().sorted().toList(); });
        t("rb.missing", () -> ResourceBundle.getBundle("Sbr2W7TailFam$Nope", Locale.ROOT));
        t("rb.missing key", () -> ResourceBundle.getBundle("Sbr2W7TailFam$Bundle", Locale.ROOT).getString("zz"));
        t("rb.same instance", () -> ResourceBundle.getBundle("Sbr2W7TailFam$Bundle", Locale.FRENCH) == ResourceBundle.getBundle("Sbr2W7TailFam$Bundle", Locale.FRENCH));
        t("rb.jdk", () -> ResourceBundle.getBundle("sun.util.resources.CalendarData", Locale.ROOT).getString("firstDayOfWeek"));
        t("rb.text", () -> java.text.DateFormatSymbols.getInstance(Locale.FRANCE).getMonths()[0]);
        // ---- CalendarDataUtility / java.time text
        t("time.month", () -> DateTimeFormatter.ofPattern("MMMM MMM", Locale.GERMAN).format(LocalDate.of(2024, 3, 5)));
        t("time.dow", () -> DateTimeFormatter.ofPattern("EEEE EEE", Locale.FRENCH).format(LocalDate.of(2024, 3, 5)));
        t("time.ampm era", () -> DateTimeFormatter.ofPattern("a G", Locale.US).format(LocalDateTime.of(2024, 3, 5, 15, 0)));
        t("time.ampm ja", () -> DateTimeFormatter.ofPattern("a G", Locale.JAPAN).format(LocalDateTime.of(2024, 3, 5, 15, 0)));
        t("time.quarter", () -> DateTimeFormatter.ofPattern("QQQ QQQQ", Locale.US).format(LocalDate.of(2024, 3, 5)));
        t("time.parse", () -> LocalDate.parse("5 mars 2024", DateTimeFormatter.ofPattern("d MMMM yyyy", Locale.FRENCH)));
        t("time.style", () -> DateTimeFormatter.ofLocalizedDate(FormatStyle.FULL).withLocale(Locale.ITALY).format(LocalDate.of(2024, 3, 5)));
        t("cal.names", () -> { var c = Calendar.getInstance(Locale.US); c.set(2024, Calendar.MARCH, 5); return c.getDisplayName(Calendar.MONTH, Calendar.LONG, Locale.US) + "," + c.getDisplayName(Calendar.DAY_OF_WEEK, Calendar.SHORT, Locale.GERMANY) + "," + c.getDisplayNames(Calendar.MONTH, Calendar.SHORT, Locale.FRANCE).size(); });
        t("cal.month names ja", () -> DayOfWeek.MONDAY.getDisplayName(java.time.format.TextStyle.FULL, Locale.JAPAN) + Month.MAY.getDisplayName(java.time.format.TextStyle.SHORT, Locale.KOREA));
        t("cal.ja era", () -> java.time.chrono.JapaneseDate.of(2024, 3, 5).toString() + java.time.chrono.JapaneseEra.HEISEI.getDisplayName(java.time.format.TextStyle.FULL, Locale.JAPAN));
        // ---- LazyLoggers / System.Logger
        t("logger.class", () -> { System.Logger l = System.getLogger("sbr2.tail"); return l.getName() + "/" + l.isLoggable(System.Logger.Level.INFO) + "/" + l.isLoggable(System.Logger.Level.DEBUG); });
        t("logger.same", () -> System.getLogger("sbr2.tail") == System.getLogger("sbr2.tail"));
        t("logger.null", () -> System.getLogger(null));
        t("logger.bundle", () -> System.getLogger("sbr2.tail2", null).getName());
        // ---- EventHelper: Security.setProperty consults isLoggingSecurity
        t("security.prop", () -> { java.security.Security.setProperty("sbr2.tail.prop", "x"); return java.security.Security.getProperty("sbr2.tail.prop"); });
        t("security.providers", () -> java.security.Security.getProviders().length > 0);
        t("security.digest", () -> java.security.MessageDigest.getInstance("SHA-256").digest(new byte[0]).length);
        // ---- ConstantUtils.concat
        t("cd.array", () -> ClassDesc.of("java.lang.String").arrayType().descriptorString() + ClassDesc.ofDescriptor("[[I").componentType());
        t("cd.nested", () -> ClassDesc.of("a.b.C").nested("D", "E").descriptorString());
        t("cd.mtd", () -> MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int, ClassDesc.of("x.Y")).descriptorString());
        t("cd.bad", () -> ClassDesc.of("a..b"));
        t("cd.of pkg", () -> ClassDesc.ofInternalName("a/b/C").packageName() + "," + ClassDesc.of("a.b.C").displayName());
        t("cd.resolve", () -> ClassDesc.of("java.util.List").resolveConstantDesc(MethodHandles.lookup()));
        // ---- FileSystems.getDefault
        t("fs.default", () -> FileSystems.getDefault().provider().getScheme() + FileSystems.getDefault().getSeparator() + FileSystems.getDefault().isOpen() + FileSystems.getDefault().isReadOnly());
        t("fs.same", () -> FileSystems.getDefault() == FileSystems.getDefault());
        t("fs.path", () -> FileSystems.getDefault().getPath("/a", "b", "c").toString());
        t("fs.matcher", () -> FileSystems.getDefault().getPathMatcher("glob:*.java").matches(Paths.get("x.java")));
        t("fs.matcher bad", () -> FileSystems.getDefault().getPathMatcher("nope:*"));
        t("fs.close", () -> { FileSystems.getDefault().close(); return "closed"; });
        t("fs.roots", () -> FileSystems.getDefault().getRootDirectories().iterator().next().toString());
        t("fs.uri", () -> FileSystems.getFileSystem(URI_ROOT).provider().getScheme());
        t("fs.newFs", () -> FileSystems.newFileSystem(java.net.URI.create("file:///"), Map.of()));
        t("fs.views", () -> new TreeSet<>(FileSystems.getDefault().supportedFileAttributeViews()));
        // ---- buffers / Bits
        t("buf.direct", () -> { var b = ByteBuffer.allocateDirect(64); b.putInt(0, 0x01020304); return b.isDirect() + "," + b.order() + "," + b.get(0) + "," + b.capacity(); });
        t("buf.pool", () -> {
            var before = poolCount();
            var keep = new ArrayList<ByteBuffer>();
            for (int i = 0; i < 4; i++) keep.add(ByteBuffer.allocateDirect(1024));
            return (poolCount() - before) + "," + keep.size();
        });
        t("buf.heap", () -> { var b = ByteBuffer.wrap(new byte[8]); b.putLong(1L << 40); b.flip(); return b.getLong() + "," + b.hasArray() + "," + b.isReadOnly(); });
        t("buf.heap.ro", () -> { var b = ByteBuffer.allocate(8).asReadOnlyBuffer(); b.put((byte) 1); return "no"; });
        t("buf.slice", () -> { var b = ByteBuffer.allocateDirect(16); b.position(4); var s = b.slice(); s.put((byte) 7); return b.get(4) + "," + s.capacity() + "," + s.isDirect(); });
        t("buf.dup views", () -> { var b = ByteBuffer.allocate(16); return b.asIntBuffer().capacity() + "," + b.duplicate().capacity() + "," + b.asCharBuffer().isDirect(); });
        t("seg.ofBuffer heap", () -> { var b = ByteBuffer.allocate(16); b.put(0, (byte) 5); var s = MemorySegment.ofBuffer(b); return s.byteSize() + "," + s.get(ValueLayout.JAVA_BYTE, 0) + "," + s.isNative(); });
        t("seg.ofBuffer direct", () -> { var b = ByteBuffer.allocateDirect(16); b.put(0, (byte) 6); var s = MemorySegment.ofBuffer(b); return s.byteSize() + "," + s.get(ValueLayout.JAVA_BYTE, 0) + "," + s.isNative(); });
        t("seg.asByteBuffer", () -> { try (var a = Arena.ofConfined()) { var s = a.allocate(16); var b = s.asByteBuffer(); b.put(0, (byte) 9); return s.get(ValueLayout.JAVA_BYTE, 0) + "," + b.isDirect() + "," + b.capacity(); } });
        t("seg.asByteBuffer closed", () -> { ByteBuffer b; try (var a = Arena.ofConfined()) { b = a.allocate(16).asByteBuffer(); } return b.get(0); });
        t("seg.asByteBuffer closed put", () -> { ByteBuffer b; try (var a = Arena.ofConfined()) { b = a.allocate(16).asByteBuffer(); } b.put((byte) 1); return "ok"; });
        t("seg.asByteBuffer heap", () -> { var s = MemorySegment.ofArray(new byte[8]); var b = s.asByteBuffer(); b.put(1, (byte) 4); return s.get(ValueLayout.JAVA_BYTE, 1) + "," + b.hasArray(); });
        t("seg.wrongthread", () -> {
            try (var a = Arena.ofConfined()) {
                var b = a.allocate(16).asByteBuffer();
                var r = new String[1];
                var th = new Thread(() -> { try { b.get(0); r[0] = "ok"; } catch (Throwable e) { r[0] = ex(e); } });
                th.start(); th.join(); return r[0];
            }
        });
        t("seg.shared buffer", () -> { try (var a = Arena.ofShared()) { var b = a.allocate(16).asByteBuffer(); var r = new String[1]; var th = new Thread(() -> { try { b.put(0, (byte) 3); r[0] = "ok"; } catch (Throwable e) { r[0] = ex(e); } }); th.start(); th.join(); return r[0] + b.get(0); } });
        t("seg.buffer mapping", () -> { var b = ByteBuffer.allocate(4); return MemorySegment.ofBuffer(b.asReadOnlyBuffer()).isReadOnly(); });
        t("buf.charset", () -> new String(ByteBuffer.wrap("héllo".getBytes("UTF-8")).array(), "UTF-8"));
        t("buf.direct big", () -> ByteBuffer.allocateDirect(-1));
        t("buf.direct order", () -> { var b = ByteBuffer.allocateDirect(8).order(ByteOrder.LITTLE_ENDIAN); b.putInt(1); return b.get(0) + "," + b.getInt(0); });
        t("buf.direct fill", () -> { var b = ByteBuffer.allocateDirect(1 << 16); for (int i = 0; i < b.capacity(); i++) b.put(i, (byte) i); long s = 0; for (int i = 0; i < b.capacity(); i++) s += b.get(i); return s; });
        t("buf.copy", () -> { var d = ByteBuffer.allocateDirect(8); d.put(new byte[]{1, 2, 3, 4}); var h = ByteBuffer.allocate(8); d.flip(); h.put(d); return Arrays.toString(h.array()); });
        // ---- byte CAS via VarHandle, Unsafe allocation
        t("cas.byte field", () -> { var vh = MethodHandles.lookup().findVarHandle(BH.class, "b", byte.class); var o = new BH(); return vh.compareAndSet(o, (byte) 3, (byte) 4) + "," + vh.compareAndSet(o, (byte) 3, (byte) 5) + "," + o.b + "," + vh.getAndAdd(o, (byte) 2) + "," + o.b; });
        t("cas.short field", () -> { var vh = MethodHandles.lookup().findVarHandle(BH.class, "s", short.class); var o = new BH(); return vh.compareAndSet(o, (short) 7, (short) 8) + "," + o.s; });
        t("cas.boolean field", () -> { var vh = MethodHandles.lookup().findVarHandle(BH.class, "f", boolean.class); var o = new BH(); return vh.compareAndSet(o, false, true) + "," + o.f + "," + vh.getAndSet(o, false) + "," + o.f; });
        t("cas.char field", () -> { var vh = MethodHandles.lookup().findVarHandle(BH.class, "c", char.class); var o = new BH(); return vh.compareAndSet(o, 'a', 'b') + "," + o.c; });
        t("cas.byte static", () -> { vb = 0; var vh = MethodHandles.lookup().findStaticVarHandle(Sbr2W7TailFam.class, "vb", byte.class); return vh.compareAndSet((byte) 0, (byte) 1) + "," + vb; });
        t("cas.byte neighbours", () -> { var o1 = new BH(); var o2 = new BH(); var vh = MethodHandles.lookup().findVarHandle(BH.class, "b", byte.class); vh.compareAndSet(o1, (byte) 3, (byte) 99); return o1.b + "," + o1.s + "," + o1.c + "," + o1.f + "/" + o2.b; });
        t("cas.byte concurrent", () -> {
            var o = new BH(); var vh = MethodHandles.lookup().findVarHandle(BH.class, "b", byte.class);
            var ts = new ArrayList<Thread>();
            for (int i = 0; i < 4; i++) { var th = new Thread(() -> { for (int j = 0; j < 500; j++) { byte v; do { v = (byte) vh.getVolatile(o); } while (!vh.compareAndSet(o, v, (byte) (v + 1))); } }); ts.add(th); th.start(); }
            for (var th : ts) th.join();
            return o.b + "," + o.s + "," + o.c;
        });
        t("cas.byte array view", () -> { var vh = MethodHandles.arrayElementVarHandle(byte[].class); var a = new byte[4]; return vh.compareAndSet(a, 1, (byte) 0, (byte) 7) + Arrays.toString(a); });
        t("unsafe.alloc", () -> {
            var f = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe"); f.setAccessible(true);
            var u = (sun.misc.Unsafe) f.get(null);
            long p = u.allocateMemory(16); u.putLong(p, 0x1122334455667788L); long v = u.getLong(p); u.putByte(p + 8, (byte) 1);
            long p2 = u.reallocateMemory(p, 32); long v2 = u.getLong(p2); u.setMemory(p2, 32, (byte) 0); long v3 = u.getLong(p2); u.freeMemory(p2);
            return Long.toHexString(v) + "," + Long.toHexString(v2) + "," + v3;
        });
        t("unsafe.alloc neg", () -> { var f = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe"); f.setAccessible(true); return ((sun.misc.Unsafe) f.get(null)).allocateMemory(-1); });
        t("unsafe.alloc zero", () -> { var f = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe"); f.setAccessible(true); return ((sun.misc.Unsafe) f.get(null)).allocateMemory(0); });
        t("arena.alloc", () -> { try (var a = Arena.ofConfined()) { var s = a.allocate(24, 8); s.set(ValueLayout.JAVA_LONG, 8, 42L); return s.get(ValueLayout.JAVA_LONG, 8) + "," + s.byteSize() + "," + (s.address() % 8); } });
        t("arena.global", () -> { var s = Arena.global().allocate(8); s.set(ValueLayout.JAVA_INT, 0, 5); return s.get(ValueLayout.JAVA_INT, 0); });
        t("arena.auto", () -> { var s = Arena.ofAuto().allocate(8); s.set(ValueLayout.JAVA_INT, 4, 6); return s.get(ValueLayout.JAVA_INT, 4); });
        t("seg.alloc neg", () -> { try (var a = Arena.ofConfined()) { return a.allocate(-1); } });
        t("seg.oob", () -> { try (var a = Arena.ofConfined()) { try { return a.allocate(4).get(ValueLayout.JAVA_LONG, 0); } catch (IndexOutOfBoundsException e) { return "IOOBE " + e.getMessage().replaceAll("address: 0x[0-9a-f]+", "address: X"); } } });
        t("seg.closed", () -> { MemorySegment s; try (var a = Arena.ofConfined()) { s = a.allocate(4); } return s.get(ValueLayout.JAVA_BYTE, 0); });
    }
    static final java.net.URI URI_ROOT = java.net.URI.create("file:///");

    static long poolCount() {
        for (var p : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class))
            if (p.getName().equals("direct")) return p.getCount() * 1000000L + p.getMemoryUsed();
        return -1;
    }

    public static void main(String[] a) throws Throwable {
        all();
        System.out.println("-- pool " + (poolCount() > 0));
        second = true;
        for (int i = 0; i < 40; i++) all();
        System.out.println("drift " + drift);
    }
}
