import java.io.*;
import java.lang.reflect.Field;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.nio.file.attribute.*;
import java.util.*;

/** sun.misc.Unsafe and java.nio.file.Files --- the last two large families off
 *  the bridge-kind retirement surface (82 + 40 rows), diffed against HotSpot.
 *
 *  UNSAFE IS DIFFED ON BEHAVIOUR, NOT ON OFFSETS. A field offset is an
 *  implementation token: two VMs are entitled to disagree on its VALUE and
 *  still both be correct. What they must agree on is that a put at an offset is
 *  visible to the matching get, that a CAS with the wrong witness fails, and
 *  that the scale/base relationship over an array is self-consistent. Printing
 *  a raw offset would manufacture a difference out of a legal freedom.
 *
 *  FILES runs entirely inside a fresh temp directory and prints only
 *  VM-independent facts --- never an absolute path, a timestamp or a size that
 *  depends on the host. */
public class UnsafeFilesSweep {
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

    static class Holder { int i = 1; long l = 2L; Object o = "init"; boolean z; double d = 0.5; }

    static void unsafe() throws Exception {
        Field f = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
        f.setAccessible(true);
        sun.misc.Unsafe u = (sun.misc.Unsafe) f.get(null);
        p("unsafe non-null", u != null);

        // ---- array base/scale: the RELATIONSHIP, not the numbers ----------
        int base = u.arrayBaseOffset(int[].class);
        int scale = u.arrayIndexScale(int[].class);
        p("int[] scale", scale);
        p("long[] scale", u.arrayIndexScale(long[].class));
        p("byte[] scale", u.arrayIndexScale(byte[].class));
        p("base is positive", base > 0);
        int[] arr = {10, 11, 12};
        p("array get via base+scale [1]", u.getInt(arr, (long) base + scale));
        u.putInt(arr, (long) base + 2L * scale, 99);
        p("array put visible to Java", arr[2]);
        p("array put visible to unsafe", u.getInt(arr, (long) base + 2L * scale));

        // ---- object fields: put/get round-trips at a resolved offset ------
        Holder h = new Holder();
        long oi = u.objectFieldOffset(Holder.class.getDeclaredField("i"));
        long ol = u.objectFieldOffset(Holder.class.getDeclaredField("l"));
        long oo = u.objectFieldOffset(Holder.class.getDeclaredField("o"));
        long od = u.objectFieldOffset(Holder.class.getDeclaredField("d"));
        p("offsets distinct", oi != ol && ol != oo && oo != od);
        p("getInt matches field", u.getInt(h, oi) == h.i);
        p("getLong matches field", u.getLong(h, ol) == h.l);
        p("getObject matches field", u.getObject(h, oo) == h.o);
        p("getDouble matches field", u.getDouble(h, od) == h.d);
        u.putInt(h, oi, 7);   p("putInt visible to Java", h.i);
        u.putLong(h, ol, 8L); p("putLong visible to Java", h.l);
        u.putObject(h, oo, "set"); p("putObject visible to Java", h.o);
        u.putDouble(h, od, 2.5);   p("putDouble visible to Java", h.d);

        // ---- CAS: the witness must decide -------------------------------
        p("CAS int wrong witness", u.compareAndSwapInt(h, oi, 999, 5));
        p("CAS int right witness", u.compareAndSwapInt(h, oi, 7, 5));
        p("CAS int result", h.i);
        p("CAS long wrong witness", u.compareAndSwapLong(h, ol, 999L, 6L));
        p("CAS long right witness", u.compareAndSwapLong(h, ol, 8L, 6L));
        p("CAS long result", h.l);
        p("CAS obj wrong witness", u.compareAndSwapObject(h, oo, "nope", "x"));
        p("CAS obj right witness", u.compareAndSwapObject(h, oo, "set", "cas"));
        p("CAS obj result", h.o);

        // ---- volatile accessors agree with the plain ones ----------------
        p("getIntVolatile agrees", u.getIntVolatile(h, oi) == u.getInt(h, oi));
        u.putIntVolatile(h, oi, 21);
        p("putIntVolatile visible", h.i);
        u.putOrderedInt(h, oi, 22);
        p("putOrderedInt visible", h.i);

        // ---- off-heap: allocate / put / get / free -----------------------
        long mem = u.allocateMemory(32);
        p("allocateMemory non-zero", mem != 0);
        u.putLong(mem, 0x1122334455667788L);
        p("off-heap long round-trip", Long.toHexString(u.getLong(mem)));
        u.putInt(mem + 8, 0x0A0B0C0D);
        p("off-heap int round-trip", Integer.toHexString(u.getInt(mem + 8)));
        u.putByte(mem + 12, (byte) 0x5A);
        p("off-heap byte round-trip", Integer.toHexString(u.getByte(mem + 12) & 0xFF));
        u.setMemory(mem, 8, (byte) 0);
        p("setMemory zeroed", u.getLong(mem));
        long mem2 = u.reallocateMemory(mem, 64);
        p("reallocateMemory non-zero", mem2 != 0);
        u.freeMemory(mem2);
        p("freeMemory returned", "ok");
        p("addressSize", u.addressSize());
        p("pageSize is power of two", Integer.bitCount(u.pageSize()) == 1);

        t("objectFieldOffset on a static", () ->
            u.objectFieldOffset(Integer.class.getDeclaredField("MAX_VALUE")));
        // Print the VALUE, not an assertion about it: the first draft threw an
        // IllegalStateException when it was non-zero, which made the diff show
        // MY exception on one VM and the callee's on the other, and hid what
        // either actually returned. The JDK specifies 0 for a non-array.
        t("arrayIndexScale(String) value", () ->
            p("arrayIndexScale(String) =", u.arrayIndexScale(String.class)));
        t("arrayBaseOffset(String) value", () ->
            p("arrayBaseOffset(String) =", u.arrayBaseOffset(String.class)));
    }

    static void files() throws Exception {
        Path dir = Files.createTempDirectory("ufs");
        try {
            Path a = dir.resolve("a.txt");
            Files.write(a, "hello\nworld\n".getBytes(StandardCharsets.UTF_8));
            p("exists", Files.exists(a));
            p("notExists", Files.notExists(dir.resolve("nope")));
            p("isRegularFile", Files.isRegularFile(a));
            p("isDirectory(dir)", Files.isDirectory(dir));
            p("size", Files.size(a));
            p("readAllBytes len", Files.readAllBytes(a).length);
            p("readString", Files.readString(a));
            p("readAllLines", Files.readAllLines(a));
            p("lines count", Files.lines(a).count());
            p("isReadable/isWritable", Files.isReadable(a) + "/" + Files.isWritable(a));
            p("isSameFile self", Files.isSameFile(a, a));
            p("probeContentType-ish null-or-text",
              String.valueOf(Files.probeContentType(a)).contains("text")
              || Files.probeContentType(a) == null);

            Path b = dir.resolve("b.txt");
            Files.copy(a, b);
            p("copy then size equal", Files.size(a) == Files.size(b));
            t("copy onto existing without REPLACE", () -> Files.copy(a, b));
            Files.copy(a, b, StandardCopyOption.REPLACE_EXISTING);
            p("copy REPLACE_EXISTING ok", Files.exists(b));

            Path c = dir.resolve("c.txt");
            Files.move(b, c);
            p("move: src gone, dst present", !Files.exists(b) && Files.exists(c));

            Path sub = Files.createDirectories(dir.resolve("x/y/z"));
            p("createDirectories", Files.isDirectory(sub));
            t("createDirectory existing", () -> Files.createDirectory(dir.resolve("x")));

            Files.writeString(dir.resolve("d.txt"), "append-me");
            Files.writeString(dir.resolve("d.txt"), "-more", StandardOpenOption.APPEND);
            p("append", Files.readString(dir.resolve("d.txt")));

            List<String> names = new ArrayList<>();
            try (DirectoryStream<Path> s = Files.newDirectoryStream(dir)) {
                for (Path q : s) names.add(q.getFileName().toString());
            }
            Collections.sort(names);
            p("newDirectoryStream sorted", names);
            List<String> walked = new ArrayList<>();
            try (var st = Files.walk(dir)) {
                st.forEach(q -> walked.add(dir.relativize(q).toString()));
            }
            Collections.sort(walked);
            p("walk relativized sorted", walked);

            BasicFileAttributes at = Files.readAttributes(a, BasicFileAttributes.class);
            p("attrs isRegularFile/isDirectory", at.isRegularFile() + "/" + at.isDirectory());
            p("attrs size agrees", at.size() == Files.size(a));

            Files.delete(c);
            p("delete", Files.exists(c));
            p("deleteIfExists absent", Files.deleteIfExists(dir.resolve("nope")));
            t("delete absent", () -> Files.delete(dir.resolve("nope")));
            t("readAllBytes absent", () -> Files.readAllBytes(dir.resolve("nope")));
            t("size absent", () -> Files.size(dir.resolve("nope")));
            t("delete non-empty dir", () -> Files.delete(dir));
        } finally {
            try (var st = Files.walk(dir)) {
                List<Path> all = new ArrayList<>(st.toList());
                Collections.reverse(all);
                for (Path q : all) { try { Files.deleteIfExists(q); } catch (Exception ignored) {} }
            }
        }
    }

    public static void main(String[] a) throws Exception {
        unsafe();
        files();
        System.out.println("DONE UnsafeFilesSweep");
    }
}
