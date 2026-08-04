import java.lang.invoke.*;
import java.lang.reflect.*;
import java.math.*;
import java.nio.charset.*;
import java.text.*;
import java.time.*;
import java.time.format.*;
import java.util.*;
import java.util.concurrent.atomic.*;
import java.util.concurrent.locks.*;
import java.util.function.*;
import java.util.regex.*;
import java.util.zip.*;
import java.io.*;

/**
 * Second breadth workload for --jdk-only, covering what
 * JdkOnlyCensusLoadProbe does not: reflection, method handles, lambdas,
 * regex, time, text formatting, charsets, zip, serialization, atomics and
 * locks, exceptions and stack traces, class loading, records and enums.
 *
 * Same discipline as the first probe and for the same reason -- the first
 * two strict runs of that one HUNG, and a killed run produces no artefact
 * and reads exactly like a job still going. Every section is caught, every
 * blocking call is bounded, and the last line is a tally.
 *
 * Each section prints values, not "ok": the interesting strict-mode failures
 * so far have been wrong numbers (FileChannel.size() returning 0), not
 * exceptions, and a section that prints "ok" cannot show that.
 */
public class JdkOnlyBreadthProbe {
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

    public static void main(String[] args) {
        section("reflection", JdkOnlyBreadthProbe::reflection);
        section("methodhandles", JdkOnlyBreadthProbe::methodHandles);
        section("lambdas", JdkOnlyBreadthProbe::lambdas);
        section("regex", JdkOnlyBreadthProbe::regex);
        section("time", JdkOnlyBreadthProbe::time);
        section("textformat", JdkOnlyBreadthProbe::textFormat);
        section("charset", JdkOnlyBreadthProbe::charset);
        section("zip", JdkOnlyBreadthProbe::zip);
        section("serialization", JdkOnlyBreadthProbe::serialization);
        section("atomics", JdkOnlyBreadthProbe::atomics);
        section("locks", JdkOnlyBreadthProbe::locks);
        section("exceptions", JdkOnlyBreadthProbe::exceptions);
        section("classloading", JdkOnlyBreadthProbe::classLoading);
        section("records", JdkOnlyBreadthProbe::records);
        section("bignum", JdkOnlyBreadthProbe::bignum);
        System.out.println("PROBE2 sections=" + sections + " failed=" + failed);
    }

    public static class Bean {
        private int value = 7;
        public String label = "L";
        public Bean() { }
        public Bean(int v) { this.value = v; }
        public int getValue() { return value; }
        public static String stat(String s) { return s + "!"; }
    }

    static void reflection() {
        Class<?> c = Bean.class;
        Method[] ms = c.getDeclaredMethods();
        Field[] fs = c.getDeclaredFields();
        Constructor<?>[] cs = c.getDeclaredConstructors();
        try {
            Object b = c.getDeclaredConstructor(int.class).newInstance(42);
            int v = (Integer) c.getMethod("getValue").invoke(b);
            Object s = c.getMethod("stat", String.class).invoke(null, "x");
            Field f = c.getDeclaredField("value");
            f.setAccessible(true);
            int direct = f.getInt(b);
            System.out.println("reflection methods=" + ms.length + " fields=" + fs.length
                    + " ctors=" + cs.length + " v=" + v + " stat=" + s + " direct=" + direct
                    + " name=" + c.getName() + " simple=" + c.getSimpleName()
                    + " arr=" + int[].class.getName()
                    + " iface=" + Runnable.class.isInterface());
        } catch (ReflectiveOperationException e) {
            throw new RuntimeException(e);
        }
    }

    static void methodHandles() {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            MethodHandle mh = l.findVirtual(Bean.class, "getValue",
                    MethodType.methodType(int.class));
            int v = (int) mh.invoke(new Bean(9));
            MethodHandle st = l.findStatic(Bean.class, "stat",
                    MethodType.methodType(String.class, String.class));
            String s = (String) st.invoke("y");
            VarHandle vh = MethodHandles.arrayElementVarHandle(int[].class);
            int[] arr = new int[4];
            vh.set(arr, 1, 5);
            System.out.println("methodhandles v=" + v + " s=" + s + " vh=" + vh.get(arr, 1)
                    + " type=" + mh.type());
        } catch (Throwable t) {
            throw new RuntimeException(t);
        }
    }

    static void lambdas() {
        Function<Integer, Integer> sq = x -> x * x;
        BiFunction<Integer, Integer, Integer> add = Integer::sum;
        Supplier<List<String>> sup = ArrayList::new;
        Predicate<String> p = String::isEmpty;
        UnaryOperator<String> u = String::trim;
        Comparator<String> byLen = Comparator.comparingInt(String::length);
        List<String> l = new ArrayList<>(List.of("ccc", "a", "bb"));
        l.sort(byLen);
        Runnable r = () -> { };
        r.run();
        System.out.println("lambdas sq=" + sq.apply(6) + " add=" + add.apply(3, 4)
                + " sup=" + sup.get().size() + " p=" + p.test("") + " u=[" + u.apply("  z  ")
                + "] sorted=" + l + " comp=" + sq.andThen(x -> x + 1).apply(2)
                + " id=" + Function.identity().apply("i"));
    }

    static void regex() {
        Pattern p = Pattern.compile("(\\w+)-(\\d+)");
        Matcher m = p.matcher("alpha-12 beta-345 gamma-6");
        StringBuilder sb = new StringBuilder();
        int n = 0;
        while (m.find()) { n++; sb.append(m.group(1)).append(':').append(m.group(2)).append(' '); }
        String repl = "a1b2c3".replaceAll("\\d", "#");
        boolean full = Pattern.matches("[a-z]+", "abc");
        System.out.println("regex n=" + n + " groups=" + sb.toString().trim()
                + " repl=" + repl + " full=" + full
                + " split=" + Arrays.toString("x,y,,z".split(","))
                + " quote=" + Pattern.quote("a.b"));
    }

    static void time() {
        LocalDate d = LocalDate.of(2026, 8, 4);
        LocalDateTime dt = LocalDateTime.of(2026, 8, 4, 13, 45, 30);
        Instant i = Instant.ofEpochSecond(1_700_000_000L);
        Duration du = Duration.ofMinutes(90);
        Period pe = Period.between(LocalDate.of(2026, 1, 1), d);
        ZonedDateTime z = dt.atZone(ZoneId.of("UTC"));
        System.out.println("time d=" + d + " dt=" + dt + " i=" + i
                + " plus=" + d.plusDays(30) + " dow=" + d.getDayOfWeek()
                + " dur=" + du + " period=" + pe.getMonths()
                + " z=" + z.toInstant()
                + " fmt=" + dt.format(DateTimeFormatter.ISO_LOCAL_DATE_TIME)
                + " epoch=" + i.toEpochMilli());
    }

    static void textFormat() {
        NumberFormat nf = NumberFormat.getInstance(Locale.US);
        DecimalFormat df = new DecimalFormat("#,##0.00");
        SimpleDateFormat sdf = new SimpleDateFormat("yyyy-MM-dd HH:mm:ss", Locale.US);
        sdf.setTimeZone(TimeZone.getTimeZone("UTC"));
        String formatted = sdf.format(new Date(1_700_000_000_000L));
        System.out.println("textformat nf=" + nf.format(1234567.891)
                + " df=" + df.format(1234.5)
                + " sdf=" + formatted
                + " str=" + String.format(Locale.US, "%,d|%05.2f|%s|%x", 1234567, 3.14159, "s", 255)
                + " loc=" + Locale.US
                + " cmp=" + "a".compareTo("b"));
    }

    static void charset() {
        try {
            String s = "héllo — wörld ✓";
            byte[] u8 = s.getBytes(StandardCharsets.UTF_8);
            byte[] u16 = s.getBytes(StandardCharsets.UTF_16);
            byte[] l1 = "hello".getBytes(StandardCharsets.ISO_8859_1);
            String back8 = new String(u8, StandardCharsets.UTF_8);
            String back16 = new String(u16, StandardCharsets.UTF_16);
            System.out.println("charset u8=" + u8.length + " u16=" + u16.length
                    + " l1=" + l1.length + " roundtrip8=" + back8.equals(s)
                    + " roundtrip16=" + back16.equals(s)
                    + " default=" + Charset.defaultCharset().name()
                    + " codepoints=" + s.codePointCount(0, s.length()));
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
    }

    static void zip() {
        try {
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            try (ZipOutputStream zo = new ZipOutputStream(bo)) {
                zo.putNextEntry(new ZipEntry("a.txt"));
                zo.write("hello zip content here".getBytes(StandardCharsets.UTF_8));
                zo.closeEntry();
            }
            byte[] zipped = bo.toByteArray();
            int entries = 0, bytes = 0;
            try (ZipInputStream zi = new ZipInputStream(new ByteArrayInputStream(zipped))) {
                ZipEntry e;
                while ((e = zi.getNextEntry()) != null) {
                    entries++;
                    byte[] buf = new byte[64];
                    int k;
                    while ((k = zi.read(buf)) > 0) bytes += k;
                }
            }
            Deflater def = new Deflater();
            def.setInput("aaaaaaaaaaaaaaaaaaaaaaaa".getBytes(StandardCharsets.UTF_8));
            def.finish();
            byte[] out = new byte[64];
            int dn = def.deflate(out);
            def.end();
            CRC32 crc = new CRC32();
            crc.update("abc".getBytes(StandardCharsets.UTF_8));
            System.out.println("zip zipped>0=" + (zipped.length > 0) + " entries=" + entries
                    + " bytes=" + bytes + " deflated>0=" + (dn > 0) + " crc=" + crc.getValue());
        } catch (IOException e) {
            throw new RuntimeException(e);
        }
    }

    public static class Payload implements Serializable {
        private static final long serialVersionUID = 1L;
        int n; String s; List<String> l;
        Payload(int n, String s, List<String> l) { this.n = n; this.s = s; this.l = l; }
    }

    static void serialization() {
        try {
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            try (ObjectOutputStream oo = new ObjectOutputStream(bo)) {
                oo.writeObject(new Payload(5, "five", new ArrayList<>(List.of("a", "b"))));
            }
            byte[] raw = bo.toByteArray();
            Payload back;
            try (ObjectInputStream oi = new ObjectInputStream(new ByteArrayInputStream(raw))) {
                back = (Payload) oi.readObject();
            }
            System.out.println("serialization bytes=" + (raw.length > 0) + " n=" + back.n
                    + " s=" + back.s + " l=" + back.l);
        } catch (IOException | ClassNotFoundException e) {
            throw new RuntimeException(e);
        }
    }

    static void atomics() {
        AtomicInteger ai = new AtomicInteger(1);
        AtomicLong al = new AtomicLong(1L);
        AtomicBoolean ab = new AtomicBoolean(false);
        AtomicReference<String> ar = new AtomicReference<>("a");
        LongAdder la = new LongAdder();
        for (int i = 0; i < 100; i++) { ai.incrementAndGet(); al.addAndGet(2); la.increment(); }
        System.out.println("atomics ai=" + ai.get() + " al=" + al.get()
                + " cas=" + ab.compareAndSet(false, true) + " ab=" + ab.get()
                + " ar=" + ar.compareAndSet("a", "b") + "/" + ar.get()
                + " adder=" + la.sum()
                + " acc=" + ai.accumulateAndGet(5, Integer::max));
    }

    static void locks() {
        ReentrantLock rl = new ReentrantLock();
        ReentrantReadWriteLock rw = new ReentrantReadWriteLock();
        int n = 0;
        rl.lock();
        try { n += rl.getHoldCount(); rl.lock(); try { n += rl.getHoldCount(); } finally { rl.unlock(); } }
        finally { rl.unlock(); }
        rw.readLock().lock();
        try { n += rw.getReadLockCount(); } finally { rw.readLock().unlock(); }
        rw.writeLock().lock();
        try { n += rw.isWriteLocked() ? 1 : 0; } finally { rw.writeLock().unlock(); }
        Object mon = new Object();
        synchronized (mon) { n += 1; }
        System.out.println("locks n=" + n + " tryLock=" + rl.tryLock() + " held=" + rl.isLocked());
        if (rl.isHeldByCurrentThread()) rl.unlock();
    }

    static void exceptions() {
        int caught = 0, depth = 0, suppressed = 0;
        String msg = "";
        try {
            try {
                throw new IllegalStateException("inner");
            } catch (IllegalStateException e) {
                caught++;
                throw new RuntimeException("outer", e);
            }
        } catch (RuntimeException e) {
            caught++;
            msg = e.getMessage() + "/" + e.getCause().getMessage();
            depth = e.getStackTrace().length;
        }
        try (AutoCloseable ac = () -> { throw new IllegalArgumentException("close"); }) {
            throw new IllegalStateException("body");
        } catch (Exception e) {
            caught++;
            suppressed = e.getSuppressed().length;
        }
        try { Object o = "s"; Integer i = (Integer) o; } catch (ClassCastException e) { caught++; }
        try { int[] a = new int[1]; int x = a[3]; } catch (ArrayIndexOutOfBoundsException e) { caught++; }
        try { String s = null; s.length(); } catch (NullPointerException e) { caught++; }
        System.out.println("exceptions caught=" + caught + " msg=" + msg
                + " depth>0=" + (depth > 0) + " suppressed=" + suppressed);
    }

    static void classLoading() {
        try {
            Class<?> c = Class.forName("java.util.ArrayList");
            ClassLoader sys = ClassLoader.getSystemClassLoader();
            Class<?> mine = Class.forName("JdkOnlyBreadthProbe", true, sys);
            System.out.println("classloading c=" + c.getName()
                    + " loaderOfJdk=" + (c.getClassLoader() == null ? "bootstrap" : "other")
                    + " mine=" + mine.getSimpleName()
                    + " sysNonNull=" + (sys != null)
                    + " parent=" + (sys.getParent() != null)
                    + " super=" + Bean.class.getSuperclass().getName()
                    + " res=" + (JdkOnlyBreadthProbe.class.getResource("/java/lang/Object.class") != null));
        } catch (ClassNotFoundException e) {
            throw new RuntimeException(e);
        }
    }

    enum Colour { RED, GREEN, BLUE }

    record Point(int x, int y) { }

    static void records() {
        Point p = new Point(3, 4);
        Point q = new Point(3, 4);
        EnumMap<Colour, Integer> em = new EnumMap<>(Colour.class);
        for (Colour c : Colour.values()) em.put(c, c.ordinal());
        EnumSet<Colour> es = EnumSet.of(Colour.RED, Colour.BLUE);
        System.out.println("records p=" + p + " eq=" + p.equals(q)
                + " hashEq=" + (p.hashCode() == q.hashCode())
                + " x=" + p.x() + " comps=" + Point.class.getRecordComponents().length
                + " enum=" + Colour.valueOf("GREEN") + " em=" + em
                + " es=" + es + " esSize=" + es.size());
    }

    static void bignum() {
        BigInteger a = new BigInteger("123456789012345678901234567890");
        BigDecimal b = new BigDecimal("3.14159265358979323846");
        System.out.println("bignum mul=" + a.multiply(BigInteger.TWO)
                + " mod=" + a.mod(BigInteger.valueOf(97))
                + " pow=" + BigInteger.TWO.pow(64)
                + " round=" + b.setScale(5, RoundingMode.HALF_UP)
                + " add=" + b.add(BigDecimal.ONE)
                + " cmp=" + b.compareTo(BigDecimal.TEN)
                + " bits=" + a.bitLength());
    }
}
