import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

/** Residual check for docs/known-issues/jdk-only/primitive-class-had-a-loader-and-two-deeper-gaps-20260827.md.
 *
 *  Three rows, each a re-measurement of one section of that doc:
 *
 *    1. int.class.getClassLoader() -- the FIXED primitive-loader defect.
 *       Re-run as a regression check, not a fresh finding.
 *    2. java.base module's getPackages().size() -- doc section 3, OPEN as of
 *       2026-08-27: HotSpot > 100, CratonVM was short.
 *    3. invokeExact on a mismatched-but-convertible call site -- doc section 4,
 *       OPEN as of 2026-08-27: HotSpot throws WrongMethodTypeException,
 *       CratonVM silently coerced.
 *
 *  No identity hashes, no thread/timing-dependent values -- every row is a
 *  plain value or an exception class name, matching the doc's own recorded
 *  harness-artefact lessons (section 5).
 */
public class PrimClassResidualSweep {
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
            v = e.getClass().getName();
        }
        p(tag, v);
    }

    public static void main(String[] a) throws Throwable {
        // 1. FIXED regression check.
        t("int.class.getClassLoader()", () -> int.class.getClassLoader());

        // 2. Module.getPackages() for java.base.
        t("java.base.getPackages().size()>100", () -> {
            int size = Object.class.getModule().getPackages().size();
            return size > 100;
        });
        t("java.base.getPackages().size()", () -> Object.class.getModule().getPackages().size());
        t("java.base.getPackages().contains(jdk.internal.loader)", () ->
                Object.class.getModule().getPackages().contains("jdk.internal.loader"));

        // 3. invokeExact must enforce its exact signature.
        t("invokeExact(wrong sig) throws WrongMethodTypeException", () -> {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            MethodHandle max = lookup.findStatic(Math.class, "max",
                    MethodType.methodType(int.class, int.class, int.class));
            try {
                max.invokeExact(1L, 2L);
                return "ACCEPTED";
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
    }
}
