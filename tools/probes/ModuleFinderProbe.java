import java.lang.module.ModuleFinder;
import java.lang.module.ModuleReference;
import java.lang.module.ModuleDescriptor;
import java.util.Set;
public class ModuleFinderProbe {
    interface Body { Object run() throws Throwable; }
    static void t(String tag, Body b) {
        String v;
        try { v = String.valueOf(b.run()); }
        catch (Throwable e) { v = "throws " + e.getClass().getName() + ": " + e.getMessage(); }
        System.out.println(tag + " = " + v);
    }
    public static void main(String[] a) {
        t("ofSystem.notNull", () -> ModuleFinder.ofSystem() != null);
        t("findAll.notNull", () -> ModuleFinder.ofSystem().findAll() != null);
        t("findAll.size>10", () -> ModuleFinder.ofSystem().findAll().size() > 10);
        t("findAll.forEach", () -> {
            int[] n = new int[1];
            ModuleFinder.ofSystem().findAll().forEach(r -> n[0]++);
            return "counted:" + (n[0] > 0);
        });
        t("findAll.stream.map.forEach", () -> {
            int[] n = new int[1];
            ModuleFinder.ofSystem().findAll().stream()
                .map(ModuleReference::descriptor).forEach(d -> n[0]++);
            return "counted:" + (n[0] > 0);
        });
        t("find.javaBase.present", () -> ModuleFinder.ofSystem().find("java.base").isPresent());
        t("find.javaBase.descriptorName", () -> ModuleFinder.ofSystem()
            .find("java.base").map(r -> r.descriptor().name()).orElse("ABSENT"));
        t("descriptor.packages.notNull", () -> ModuleFinder.ofSystem()
            .find("java.base").map(r -> r.descriptor().packages() != null).orElse(false));
        t("descriptor.exports.notNull", () -> ModuleFinder.ofSystem()
            .find("java.base").map(r -> r.descriptor().exports() != null).orElse(false));
        t("descriptor.opens.notNull", () -> ModuleFinder.ofSystem()
            .find("java.base").map(r -> r.descriptor().opens() != null).orElse(false));
        System.out.println("DONE");
    }
}
