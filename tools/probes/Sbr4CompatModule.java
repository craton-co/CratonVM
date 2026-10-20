import java.lang.module.*;
import java.util.*;

/**
 * SBR round 4, lane compat: the boot module graph with a {@code --module-path} module, HotSpot-diffed in
 * {@code --jdk-only} and {@code --compatible}. Needs the module {@code m3} (package {@code p}, class
 * {@code p.X}); run with {@code --module-path <dir>/m3 --add-modules m3}. Prints one line per question.
 */
public class Sbr4CompatModule {
    static String norm(Object v) {
        return String.valueOf(v).replaceAll("0x[0-9a-fA-F]+", "0xA").replaceAll("@[0-9a-f]+", "@X");
    }

    static void row(String n, Object v) {
        System.out.println(n + " |" + norm(v) + "|");
    }

    interface T {
        Object run() throws Throwable;
    }

    static void t(String n, T r) {
        try {
            row(n, r.run());
        } catch (Throwable e) {
            row(n, "THREW " + e.getClass().getName() + ": " + norm(e.getMessage()));
        }
    }

    public static void main(String[] a) throws Throwable {
        ModuleLayer boot = ModuleLayer.boot();
        t("boot.same", () -> boot == ModuleLayer.boot());
        t("boot.parents", () -> boot.parents().size());
        t("boot.cf.same", () -> boot.configuration() == boot.configuration());
        t("boot.cf.m3", () -> boot.configuration().findModule("m3").isPresent());
        t("boot.cf.base", () -> boot.configuration().findModule("java.base").isPresent());
        t("boot.cf.none", () -> boot.configuration().findModule("no.such").isPresent());
        t("boot.cf.modules.m3", () -> boot.configuration().modules().stream()
                .anyMatch(r -> r.name().equals("m3")));
        t("boot.cf.modules.many", () -> boot.configuration().modules().size() > 20);
        t("boot.cf.parents", () -> boot.configuration().parents().size());
        t("boot.cf.m3.reads", () -> boot.configuration().findModule("m3").get().reads().stream()
                .map(ResolvedModule::name).sorted().toList());
        t("boot.cf.m3.cfg", () -> boot.configuration().findModule("m3").get().configuration() == boot.configuration());
        t("boot.cf.m3.ref", () -> boot.configuration().findModule("m3").get().reference().descriptor().name());
        t("boot.find.m3", () -> boot.findModule("m3").isPresent());
        t("boot.find.m3.layer", () -> boot.findModule("m3").get().getLayer() == boot);
        t("boot.modules.m3", () -> boot.modules().stream().anyMatch(m -> m.getName().equals("m3")));
        t("boot.modules.many", () -> boot.modules().size() > 20);
        t("m3.class.module", () -> Class.forName("p.X").getModule().getName());
        t("m3.class.layer", () -> Class.forName("p.X").getModule().getLayer() == boot);
        t("m3.exports", () -> Class.forName("p.X").getModule().getDescriptor().exports().toString());
        t("m3.isExported", () -> Class.forName("p.X").getModule().isExported("p"));
        t("m3.canRead.base", () -> Class.forName("p.X").getModule().canRead(Object.class.getModule()));
        t("m3.packages", () -> Class.forName("p.X").getModule().getPackages());
        t("m3.loader", () -> Class.forName("p.X").getClassLoader() == ClassLoader.getSystemClassLoader());
        t("base.layer", () -> Object.class.getModule().getLayer() == boot);
        t("base.cf.module", () -> boot.configuration().findModule("java.base").get().reference().descriptor().name());
        t("sys.find.base", () -> ModuleFinder.ofSystem().find("java.base").isPresent());
        t("sys.find.m3", () -> ModuleFinder.ofSystem().find("m3").isPresent());
        t("sys.findAll.many", () -> ModuleFinder.ofSystem().findAll().size() > 20);
        t("services", () -> boot.findModule("m3").get().getDescriptor().provides().size());
        t("empty.parent", () -> boot.parents().get(0) == ModuleLayer.empty());
    }
}
