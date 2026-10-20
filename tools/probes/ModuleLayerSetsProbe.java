import java.lang.ModuleLayer;
public class ModuleLayerSetsProbe {
    interface Body { Object run() throws Throwable; }
    static void t(String tag, Body b) {
        String v;
        try { v = String.valueOf(b.run()); }
        catch (Throwable e) { v = "throws " + e.getClass().getName(); }
        System.out.println(tag + " = " + v);
    }
    public static void main(String[] a) {
        t("boot.notNull", () -> ModuleLayer.boot() != null);
        t("boot.modules.notNull", () -> ModuleLayer.boot().modules() != null);
        t("boot.modules.isEmpty", () -> ModuleLayer.boot().modules().isEmpty());
        t("boot.modules.forEach", () -> {
            int[] n = new int[1];
            ModuleLayer.boot().modules().forEach(m -> n[0]++);
            return "ok:" + (n[0] > 0);
        });
        t("boot.cfg.notNull", () -> ModuleLayer.boot().configuration() != null);
        t("boot.cfg.modules.notNull", () -> ModuleLayer.boot().configuration().modules() != null);
        t("boot.cfg.modules.forEach", () -> {
            int[] n = new int[1];
            ModuleLayer.boot().configuration().modules().forEach(m -> n[0]++);
            return "ok:" + (n[0] > 0);
        });
        t("boot.parents.notNull", () -> ModuleLayer.boot().parents() != null);
        t("base.pkgs.forEach", () -> {
            int[] n = new int[1];
            Object.class.getModule().getPackages().forEach(p -> n[0]++);
            return "ok:" + (n[0] > 0);
        });
        System.out.println("DONE");
    }
}
