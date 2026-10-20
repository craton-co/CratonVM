import java.lang.module.ModuleDescriptor;
import java.util.*;

/**
 * SBR round 5, lane compat2: java.lang.Module access rows against HotSpot. addExports / addOpens /
 * addUses / addReads called from an unnamed module on a named module, the implAdd* family through
 * the JavaLangAccess, isOpen / isExported across the boot layer, and a Module of a user loader.
 * Every line must be byte-identical under HotSpot, --jdk-only and --compatible.
 */
public class Sbr5Compat2Module {
    static String norm(Object v) {
        return String.valueOf(v).replaceAll("@[0-9a-f]+", "@X");
    }

    static void row(String name, Object v) {
        System.out.println(name + " |" + norm(v) + "|");
    }

    interface T {
        Object run() throws Throwable;
    }

    static void t(String name, T r) {
        try {
            row(name, r.run());
        } catch (Throwable e) {
            row(name, "THREW " + e.getClass().getName() + ": " + norm(e.getMessage()));
        }
    }

    public static void main(String[] args) throws Throwable {
        Module self = Sbr5Compat2Module.class.getModule();
        Module base = Object.class.getModule();
        Module logging = java.util.logging.Logger.class.getModule();
        Module sql = ModuleLayer.boot().findModule("java.sql").orElse(null);
        Module unnamedOfLoader = new java.net.URLClassLoader(new java.net.URL[0]).getUnnamedModule();

        t("self.named", () -> self.isNamed());
        t("self.getName", () -> self.getName());
        t("self.layer", () -> self.getLayer());
        t("self.descriptor", () -> self.getDescriptor());
        t("base.name", () -> base.getName());
        t("base.layer.isBoot", () -> base.getLayer() == ModuleLayer.boot());
        t("base.descriptor.name", () -> base.getDescriptor().name());
        t("base.packages.has", () -> base.getPackages().contains("java.lang"));
        t("logging.getLayer.isBoot", () -> logging.getLayer() == ModuleLayer.boot());

        // The caller is an unnamed module: HotSpot refuses every mutation of a named module.
        t("base.addExports", () -> base.addExports("jdk.internal.misc", self));
        t("base.addOpens", () -> base.addOpens("java.lang", self));
        t("base.addUses", () -> base.addUses(Runnable.class));
        t("base.addReads", () -> base.addReads(self));
        t("logging.addExports", () -> logging.addExports("java.util.logging", self));
        t("logging.addOpens", () -> logging.addOpens("java.util.logging", self));
        t("logging.addUses", () -> logging.addUses(Runnable.class));
        t("logging.addReads", () -> logging.addReads(self));
        t("sql.addExports", () -> sql.addExports("java.sql", self));
        t("sql.addOpens", () -> sql.addOpens("java.sql", self));
        t("sql.addUses", () -> sql.addUses(Runnable.class));
        // Null checks come first.
        t("base.addExports.nullpkg", () -> base.addExports(null, self));
        t("base.addExports.nullmod", () -> base.addExports("java.lang", null));
        t("base.addOpens.nullpkg", () -> base.addOpens(null, self));
        t("base.addUses.null", () -> base.addUses(null));
        t("base.addReads.null", () -> base.addReads(null));
        // The unnamed module accepts everything and answers itself.
        t("self.addExports", () -> self.addExports("x", base) == self);
        t("self.addOpens", () -> self.addOpens("x", base) == self);
        t("self.addReads", () -> self.addReads(base) == self);
        t("self.addUses", () -> self.addUses(Runnable.class) == self);
        t("unnamed.addExports", () -> unnamedOfLoader.addExports("x", base) == unnamedOfLoader);
        t("unnamed.addUses", () -> unnamedOfLoader.addUses(Runnable.class) == unnamedOfLoader);
        t("unnamed.addReads", () -> unnamedOfLoader.addReads(self) == unnamedOfLoader);

        // Access questions.
        for (String pkg : new String[] {"java.lang", "java.util", "jdk.internal.misc", "sun.nio.ch", "no.such.pkg"}) {
            t("base.isExported." + pkg, () -> base.isExported(pkg));
            t("base.isOpen." + pkg, () -> base.isOpen(pkg));
            t("base.isExported." + pkg + ".self", () -> base.isExported(pkg, self));
            t("base.isOpen." + pkg + ".self", () -> base.isOpen(pkg, self));
        }
        for (String pkg : new String[] {"java.util.logging", "sun.util.logging.internal", "no.such.pkg"}) {
            t("logging.isExported." + pkg, () -> logging.isExported(pkg));
            t("logging.isOpen." + pkg, () -> logging.isOpen(pkg));
            t("logging.isExported." + pkg + ".self", () -> logging.isExported(pkg, self));
            t("logging.isOpen." + pkg + ".self", () -> logging.isOpen(pkg, self));
            t("logging.isOpen." + pkg + ".base", () -> logging.isOpen(pkg, base));
            t("logging.isOpen." + pkg + ".logging", () -> logging.isOpen(pkg, logging));
        }
        t("self.isOpen.any", () -> self.isOpen("anything"));
        t("self.isExported.any", () -> self.isExported("anything"));
        t("self.isExported.any.base", () -> self.isExported("anything", base));
        t("self.isOpen.any.base", () -> self.isOpen("anything", base));
        t("unnamed.isOpen.any", () -> unnamedOfLoader.isOpen("anything"));
        t("base.isExported.null", () -> base.isExported(null));
        t("base.isOpen.null", () -> base.isOpen(null));
        t("base.isExported.null.self", () -> base.isExported(null, self));
        t("base.isExported.pkg.null", () -> base.isExported("java.lang", null));
        t("base.isOpen.pkg.null", () -> base.isOpen("java.lang", null));
        t("self.canRead.base", () -> self.canRead(base));
        t("self.canRead.logging", () -> self.canRead(logging));
        t("base.canRead.self", () -> base.canRead(self));
        t("base.canRead.logging", () -> base.canRead(logging));
        t("logging.canRead.base", () -> logging.canRead(base));
        t("self.canUse.runnable", () -> self.canUse(Runnable.class));
        t("base.canUse.runnable", () -> base.canUse(Runnable.class));
        t("base.canUse.null", () -> base.canUse(null));
        t("self.canRead.null", () -> self.canRead(null));
        // After addExports/addOpens on the unnamed side the answers do not move.
        t("self.after.addExports.isExported", () -> {
            self.addExports("anything", base);
            return self.isExported("anything", base) + " " + self.isExported("anything");
        });
        t("self.annotation.null", () -> base.getAnnotation(Deprecated.class) != null);
        t("self.getAnnotations.len", () -> base.getAnnotations().length);
        t("self.getDeclaredAnnotations.len", () -> base.getDeclaredAnnotations().length);
        t("base.toString", () -> base.toString());
        t("self.toString", () -> self.toString());
        t("logging.toString", () -> logging.toString());
        t("self.getClassLoader", () -> self.getClassLoader() == ClassLoader.getSystemClassLoader());
        t("base.getClassLoader", () -> base.getClassLoader());
        t("logging.getClassLoader", () -> logging.getClassLoader() == null);
        t("sql.getClassLoader.name", () -> sql.getClassLoader().getName());
        t("base.getResourceAsStream.java.lang.Object", () -> base.getResourceAsStream("java/lang/Object.class") != null);
        t("self.getResourceAsStream.probe", () -> self.getResourceAsStream("Sbr5Compat2Module.class") != null);
        t("base.getResourceAsStream.null", () -> base.getResourceAsStream(null));
        t("self.getDescriptor.null", () -> self.getDescriptor());
        t("sql.reads.logging", () -> sql.canRead(logging));
        t("sql.descriptor.requires.has.logging", () -> sql.getDescriptor().requires().stream()
                .map(ModuleDescriptor.Requires::name).sorted().toList().contains("java.logging"));
        t("layer.findModule.self", () -> ModuleLayer.boot().findModule("java.logging").get() == logging);
    }
}
