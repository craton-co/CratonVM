import java.io.*;
import java.nio.file.*;
import java.util.*;

/**
 * SBR2 W7 TAIL: the System property store rows (getProperties/setProperty/clearProperty/setProperties)
 * seen from BOTH sides -- Java reads and readers that are Rust natives in this VM (tmpdir, timezone,
 * library path, proxy) -- and System$1.findBootstrapClassOrNull through Class.forName(Module, name).
 */
public class Sbr2W7TailSys {
    static void row(String n, Object v) { System.out.println(n + " = " + v); }
    static String ex(Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }
    interface T { Object run() throws Throwable; }
    static void t(String n, T r) { try { row(n, r.run()); } catch (Throwable e) { row(n, "EX " + ex(e)); } }

    public static void main(String[] a) throws Throwable {
        // timezone is a lazily-read Rust-side reader on this VM: set it before anything asks
        System.setProperty("user.timezone", "Asia/Tokyo");
        t("tz.after set", () -> TimeZone.getDefault().getID());
        t("get basic", () -> System.getProperty("java.specification.version"));
        t("set returns old", () -> System.setProperty("sbr2.tail.k", "1") + "," + System.setProperty("sbr2.tail.k", "2") + "," + System.getProperty("sbr2.tail.k"));
        t("clear returns old", () -> System.clearProperty("sbr2.tail.k") + "," + System.clearProperty("sbr2.tail.k") + "," + System.getProperty("sbr2.tail.k"));
        t("set null key", () -> System.setProperty(null, "x"));
        t("set empty key", () -> System.setProperty("", "x"));
        t("set null value", () -> System.setProperty("sbr2.tail.nv", null));
        t("clear null key", () -> System.clearProperty(null));
        t("clear empty key", () -> System.clearProperty(""));
        t("get null key", () -> System.getProperty(null));
        t("get empty key", () -> System.getProperty(""));
        t("get default", () -> System.getProperty("sbr2.tail.none", "dflt"));
        t("props identity", () -> System.getProperties() == System.getProperties());
        t("props via obj", () -> { System.getProperties().setProperty("sbr2.tail.viaobj", "v"); return System.getProperty("sbr2.tail.viaobj"); });
        t("props via obj clear", () -> { System.getProperties().remove("sbr2.tail.viaobj"); return System.getProperty("sbr2.tail.viaobj"); });
        t("set visible in props", () -> { System.setProperty("sbr2.tail.vis", "z"); return System.getProperties().getProperty("sbr2.tail.vis") + "," + System.getProperties().containsKey("sbr2.tail.vis"); });
        t("clear visible in props", () -> { System.clearProperty("sbr2.tail.vis"); return System.getProperties().getProperty("sbr2.tail.vis") + "," + System.getProperties().containsKey("sbr2.tail.vis"); });
        t("non-string key", () -> { System.getProperties().put(42, "n"); Object v = System.getProperties().get(42); System.getProperties().remove(42); return v; });
        t("tmpdir", () -> {
            Path d = Files.createTempDirectory("sbr2tailbase");
            String old = System.getProperty("java.io.tmpdir");
            System.setProperty("java.io.tmpdir", d.toString());
            try { Path f = Files.createTempFile("sbr2", ".t"); boolean in = f.getParent().equals(d); Files.delete(f); return in; }
            finally { System.setProperty("java.io.tmpdir", old); Files.delete(d); }
        });
        t("tmpdir via props obj", () -> {
            Path d = Files.createTempDirectory("sbr2tailbase2");
            String old = System.getProperty("java.io.tmpdir");
            System.getProperties().setProperty("java.io.tmpdir", d.toString());
            try { Path f = Files.createTempFile("sbr2", ".t"); boolean in = f.getParent().equals(d); Files.delete(f); return in; }
            finally { System.getProperties().setProperty("java.io.tmpdir", old); Files.delete(d); }
        });
        t("library path msg", () -> {
            System.setProperty("java.library.path", "/nonexistent/sbr2");
            try { System.loadLibrary("sbr2nonesuch"); return "loaded"; } catch (UnsatisfiedLinkError e) { return e.getMessage().replaceAll("\\(.*\\)", "(..)"); }
        });
        t("setProperties custom", () -> {
            Properties saved = System.getProperties();
            Properties p = new Properties(); p.setProperty("sbr2.tail.only", "1");
            System.setProperties(p);
            try { return System.getProperty("sbr2.tail.only") + "," + System.getProperty("java.version") + "," + (System.getProperties() == p) + "," + System.getProperty("os.name"); }
            finally { System.setProperties(saved); }
        });
        t("setProperties then set", () -> {
            Properties saved = System.getProperties();
            Properties p = new Properties();
            System.setProperties(p);
            try { System.setProperty("sbr2.tail.b", "2"); return p.getProperty("sbr2.tail.b") + "," + System.getProperty("sbr2.tail.b"); }
            finally { System.setProperties(saved); }
        });
        t("setProperties null", () -> {
            Properties saved = System.getProperties();
            System.setProperties(null);
            try { return System.getProperty("java.specification.version") + "," + (System.getProperties() != saved) + "," + System.getProperty("os.name"); }
            finally { System.setProperties(saved); }
        });
        t("restored after swap", () -> System.getProperty("sbr2.tail.only") + "," + System.getProperty("java.version"));
        t("stringPropertyNames has", () -> System.getProperties().stringPropertyNames().contains("java.version"));
        t("props size sane", () -> System.getProperties().size() > 20);
        t("lineSeparator", () -> System.lineSeparator().length() + "," + System.getProperty("line.separator").length());
        t("file.separator", () -> System.getProperty("file.separator") + System.getProperty("path.separator"));
        t("user.dir", () -> System.getProperty("user.dir").equals(new File("").getAbsolutePath()));
        t("jboss.home.dir", () -> System.getProperty("jboss.home.dir"));
        t("ioWorkerCount", () -> System.getProperty("reactor.netty.ioWorkerCount"));
        t("getenv", () -> System.getenv("PATH") != null);
        t("prop from thread", () -> { var r = new String[1]; var th = new Thread(() -> { System.setProperty("sbr2.tail.thr", "q"); r[0] = System.getProperty("sbr2.tail.thr"); }); th.start(); th.join(); return r[0] + "," + System.getProperty("sbr2.tail.thr"); });
        t("Boolean.getBoolean", () -> { System.setProperty("sbr2.tail.bool", "true"); return Boolean.getBoolean("sbr2.tail.bool") + "," + Integer.getInteger("sbr2.tail.none", 7) + "," + Long.getLong("java.version", 1L); });
        t("Integer.getInteger set", () -> { System.setProperty("sbr2.tail.int", "0x10"); return Integer.getInteger("sbr2.tail.int"); });
        // System$1.findBootstrapClassOrNull through Class.forName(Module, String) and loaders
        t("forName module base", () -> Class.forName(Object.class.getModule(), "java.util.Objects"));
        t("forName module missing", () -> Class.forName(Object.class.getModule(), "java.util.NoSuchThing"));
        t("forName module app class", () -> Class.forName(Object.class.getModule(), "Sbr2W7TailSys"));
        t("forName null loader jdk", () -> Class.forName("java.util.ArrayList", false, null));
        t("forName null loader app", () -> Class.forName("Sbr2W7TailSys", false, null));
        t("forName null loader missing", () -> Class.forName("no.such.Thing", false, null));
        t("platform loadClass", () -> ClassLoader.getPlatformClassLoader().loadClass("java.sql.Date").getClassLoader());
        t("platform loadClass app", () -> ClassLoader.getPlatformClassLoader().loadClass("Sbr2W7TailSys"));
        t("system loadClass jdk", () -> ClassLoader.getSystemClassLoader().loadClass("java.util.HashMap").getClassLoader());
        t("findBoot reflect", () -> {
            var m = ClassLoader.class.getDeclaredMethod("findBootstrapClassOrNull", String.class);
            m.setAccessible(true);
            return m.invoke(null, "java.lang.Thread") + "," + m.invoke(null, "Sbr2W7TailSys") + "," + m.invoke(null, "bad name!") + "," + m.invoke(null, "java.util.concurrent.locks.LockSupport");
        });
        t("Package boot", () -> Object.class.getPackage().getName() + "," + Package.getPackage("java.util"));
        t("service loader", () -> { int n = 0; for (var p : ServiceLoader.load(java.nio.file.spi.FileSystemProvider.class)) n++; return n >= 0; });
    }
}
