// SBR2 wave 9, lane MODLOAD: cost of the rows the lane retires (real JDK bytecode vs the natives).
// One line per phase: "<phase> <ms>". Run it interleaved against the control binary, medians of >= 5.
//   java|cratonvm [--jdk-only] -cp out Sbr2W9ModloadBench
import java.lang.module.ModuleFinder;

public class Sbr2W9ModloadBench {
    static long sink;

    static void phase(String name, int n, Runnable r) {
        for (int i = 0; i < Math.min(n, 2000); i++) r.run(); // warm
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) r.run();
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println(name + " " + ms);
    }

    public static void main(String[] a) throws Exception {
        Module base = Object.class.getModule();
        Module sql = java.sql.Connection.class.getModule();
        ClassLoader sys = ClassLoader.getSystemClassLoader();
        ClassLoader plat = ClassLoader.getPlatformClassLoader();
        ModuleLayer boot = ModuleLayer.boot();
        ModuleFinder sf = ModuleFinder.ofSystem();
        Class<?> self = Sbr2W9ModloadBench.class;
        phase("forName(Module,String) boot x40k", 40_000, () -> {
            try { sink += Class.forName(base, "java.lang.String").hashCode(); } catch (Throwable t) { sink++; }
        });
        phase("forName(Module,String) platform x40k", 40_000, () -> {
            try { sink += Class.forName(sql, "java.sql.Connection").hashCode(); } catch (Throwable t) { sink++; }
        });
        phase("sys.loadClass(app) x40k", 40_000, () -> {
            try { sink += sys.loadClass("Sbr2W9ModloadBench").hashCode(); } catch (Throwable t) { sink++; }
        });
        phase("sys.loadClass(String) x40k", 40_000, () -> {
            try { sink += sys.loadClass("java.lang.String").hashCode(); } catch (Throwable t) { sink++; }
        });
        phase("plat.loadClass(sql) x40k", 40_000, () -> {
            try { sink += plat.loadClass("java.sql.Connection").hashCode(); } catch (Throwable t) { sink++; }
        });
        phase("getPackage app x40k", 40_000, () -> sink += self.getPackage().hashCode());
        phase("getPackage boot x40k", 40_000, () -> sink += String.class.getPackage().hashCode());
        phase("getPackage platform x40k", 40_000, () -> sink += java.sql.Connection.class.getPackage().hashCode());
        phase("ModuleLayer.boot x200k", 200_000, () -> sink += System.identityHashCode(ModuleLayer.boot()));
        phase("boot.configuration x200k", 200_000, () -> sink += System.identityHashCode(boot.configuration()));
        phase("ofSystem().find x40k", 40_000, () -> sink += sf.find("java.base").isPresent() ? 1 : 0);
        phase("ModuleFinder.ofSystem x40k", 40_000, () -> sink += System.identityHashCode(ModuleFinder.ofSystem()));
        phase("Class.getResource boot x10k", 10_000, () -> sink += String.class.getResource("String.class") != null ? 1 : 0);
        phase("Class.getResource app x10k", 10_000, () -> sink += self.getResource("Sbr2W9ModloadBench.class") != null ? 1 : 0);
        phase("Class.getResource missing x10k", 10_000, () -> sink += self.getResource("/nope.txt") != null ? 1 : 0);
        System.out.println("sink " + (sink != 0));
    }
}
