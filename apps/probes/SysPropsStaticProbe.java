import java.lang.reflect.Field;
import java.util.Properties;

/** Is `java.lang.System.props` a populated `Properties`?
 *
 *  `System.getProperty` IS `props.getProperty(key)` in the real JDK
 *  (System.java:744). This VM answers that method from a native, so the field
 *  never mattered and was never set -- until any real `System` bytecode runs,
 *  at which point the first `System.getProperty` of the first vector NPEs.
 *
 *  Read through reflection rather than by arming the dial, because the two
 *  questions are different: the dial says what happens when the native
 *  declines, and this says whether the field a retirement would expose is
 *  actually there. Both VMs answer the same way when it is.
 *
 *  Nothing here prints a property VALUE the two VMs may choose independently.
 *  Shape only: non-null, a `Properties`, more than ten entries, a known key
 *  present, and agreeing with `System.getProperty` on that key.
 *
 *  REQUIRES `--add-opens java.base/java.lang=ALL-UNNAMED`, and it is MUTE
 *  without it. Run plain, every row on both VMs is an
 *  InaccessibleObjectException and the probe is comparing two exception
 *  strings -- which do not even match, so it reads as eight diffs that say
 *  nothing about the field. That is why it is not in the standard probe sweep,
 *  which passes no extra launcher flags:
 *
 *    java --add-opens java.base/java.lang=ALL-UNNAMED -cp out SysPropsStaticProbe
 */
public class SysPropsStaticProbe {
    static int rows = 0;

    static void p(String tag, Object v) {
        System.out.println(++rows + " " + tag + " |" + v + "|");
    }

    interface Body {
        Object call() throws Throwable;
    }

    static void t(String tag, Body b) {
        Object v;
        try {
            v = b.call();
        } catch (Throwable e) {
            v = e.getClass().getName() + ": " + e.getMessage();
        }
        p(tag, v);
    }

    static Properties props() throws Exception {
        Field f = System.class.getDeclaredField("props");
        f.setAccessible(true);
        return (Properties) f.get(null);
    }

    public static void main(String[] a) {
        t("props is non-null", () -> props() != null);
        t("props is a Properties", () -> props().getClass().getName());
        t("props size > 10", () -> props().size() > 10);
        t("props java.version present", () -> props().getProperty("java.version") != null);
        t("props agrees with getProperty", () -> {
            String viaField = props().getProperty("java.version");
            String viaApi = System.getProperty("java.version");
            return viaField != null && viaField.equals(viaApi);
        });
        // The field must track the object the API hands out, or a retirement
        // that starts reading it sees a different map from the one callers
        // mutate.
        t("props is the getProperties object", () -> props() == System.getProperties());
        t("a write through the API is visible in the field", () -> {
            System.setProperty("cratonvm.probe.staticfield", "yes");
            String seen = props().getProperty("cratonvm.probe.staticfield");
            System.clearProperty("cratonvm.probe.staticfield");
            return seen;
        });
        t("a clear through the API is visible in the field", () -> {
            System.setProperty("cratonvm.probe.staticfield2", "yes");
            System.clearProperty("cratonvm.probe.staticfield2");
            return props().getProperty("cratonvm.probe.staticfield2");
        });
        System.out.println("DONE SysPropsStaticProbe");
    }
}
