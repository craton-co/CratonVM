import java.lang.module.*;
import java.nio.file.*;
import java.util.*;

/**
 * SBR3 NIOBUF: ModuleReferenceImpl.open over a --module-path module (boot layer) and over a ModuleFinder.of reference.
 * Needs a module "m3" (package p, class p.X, resource p/r.txt) at <dir>/m3 (exploded) and <dir>/m3j.jar;
 * run with --module-path <dir>/m3 --add-modules m3 and the dir as args[0].
 */
public class Sbr3NioBufMod {
    static String norm(Object v) { return String.valueOf(v).replaceAll("0x[0-9a-fA-F]+", "0xA").replaceAll("@[0-9a-f]+", "@X"); }
    static void row(String n, Object v) { System.out.println(n + " |" + norm(v) + "|"); }
    interface T { Object run() throws Throwable; }
    static void t(String n, T r) { try { row(n, r.run()); } catch (Throwable e) { row(n, "THREW " + e.getClass().getName() + ": " + norm(e.getMessage())); } }
    public static void main(String[] a) throws Throwable {
        String dir = a[0];
        ModuleReference boot = ModuleLayer.boot().configuration().findModule("m3").get().reference();
        t("boot.name", () -> boot.descriptor().name());
        t("boot.open.find.class", () -> { try (var rd = boot.open()) { return rd.find("p/X.class").isPresent(); } });
        t("boot.open.find.res", () -> { try (var rd = boot.open()) { return rd.find("p/r.txt").isPresent(); } });
        t("boot.open.find.missing", () -> { try (var rd = boot.open()) { return rd.find("p/Nope.class").isPresent(); } });
        t("boot.open.read.res", () -> { try (var rd = boot.open(); var in = rd.open("p/r.txt").get()) { return new String(in.readAllBytes()).trim(); } });
        t("boot.open.read.bb", () -> { try (var rd = boot.open()) { var bb = rd.read("p/r.txt").get(); try { return bb.remaining(); } finally { rd.release(bb); } } });
        t("boot.open.list", () -> { try (var rd = boot.open()) { var l = new TreeSet<String>(); rd.list().forEach(l::add); return l; } });
        t("boot.open.closed.find", () -> { var rd = boot.open(); rd.close(); return rd.find("p/X.class").isPresent(); });
        t("boot.open.closed.list", () -> { var rd = boot.open(); rd.close(); return rd.list().count(); });
        t("boot.open.twice", () -> { try (var a1 = boot.open(); var a2 = boot.open()) { return a1 != a2; } });
        t("boot.loc", () -> boot.location().map(u -> u.getScheme()).orElse("none"));
        t("boot.module.res", () -> { try (var in = Class.forName("p.X").getModule().getResourceAsStream("p/r.txt")) { return new String(in.readAllBytes()).trim(); } });
        t("boot.class.res", () -> { try (var in = Class.forName("p.X").getResourceAsStream("r.txt")) { return new String(in.readAllBytes()).trim(); } });
        for (String path : new String[]{dir + "/m3", dir + "/m3j.jar"}) {
            String tag = path.endsWith(".jar") ? "jar" : "dir";
            ModuleFinder f = ModuleFinder.of(Paths.get(path));
            t(tag + ".find", () -> f.find("m3").isPresent());
            t(tag + ".open.find.class", () -> { try (var rd = f.find("m3").get().open()) { return rd.find("p/X.class").isPresent(); } });
            t(tag + ".open.read.res", () -> { try (var rd = f.find("m3").get().open(); var in = rd.open("p/r.txt").get()) { return new String(in.readAllBytes()).trim(); } });
            t(tag + ".open.list", () -> { try (var rd = f.find("m3").get().open()) { var l = new TreeSet<String>(); rd.list().forEach(l::add); return l; } });
            t(tag + ".open.closed.list", () -> { var rd = f.find("m3").get().open(); rd.close(); return rd.list().count(); });
            t(tag + ".findAll", () -> f.findAll().size());
        }
        ModuleFinder sys = ModuleFinder.ofSystem();
        t("sys.open.base", () -> { try (var rd = sys.find("java.base").get().open()) { return rd.find("java/lang/Object.class").isPresent(); } });
        ModuleFinder comp = ModuleFinder.compose(ModuleFinder.of(Paths.get(dir + "/m3")), sys);
        t("compose.open", () -> { try (var rd = comp.find("m3").get().open()) { return rd.find("p/X.class").isPresent(); } });
        t("compose.open.sys", () -> { try (var rd = comp.find("java.sql").get().open()) { return rd.find("module-info.class").isPresent(); } });
    }
}
