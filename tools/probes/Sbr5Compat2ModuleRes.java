import java.io.InputStream;

/**
 * SBR round 5, lane compat2: the ONE Module row that still differs from HotSpot in both modes.
 * {@code Module.getResourceAsStream("module-info.class")} of a boot module answers a stream on
 * HotSpot; {@code --jdk-only} and {@code --compatible} (with the jdk-module-access row) answer
 * null although {@code ModuleFinder.ofSystem().find("java.base").get().open().open("module-info.class")}
 * works and {@code java/lang/Object.class} through the same call works. See
 * docs/internal/jdk-only/SBR5-COMPAT2-module-info-resource-RETIRED-20261006.md.
 */
public class Sbr5Compat2ModuleRes {
    public static void main(String[] args) throws Throwable {
        Module base = Object.class.getModule();
        for (String n : new String[] {"module-info.class", "/module-info.class", "java/lang/Object.class"}) {
            try (InputStream in = base.getResourceAsStream(n)) {
                System.out.println(n + " |" + (in != null) + "|");
            }
        }
    }
}
