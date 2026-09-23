import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

/**
 * Differential probe for MethodHandles.Lookup.find* failure modes.
 *
 * The contract that matters: a missing member must raise a checked
 * NoSuchMethodException / NoSuchFieldException (both extend Exception), not a
 * NoSuchMethodError (an Error). Library code version-probes with
 *
 *   try   { mh = lookup.findVirtual(C.class, "m", type); }
 *   catch (Exception e) { mh = lookup.findGetter(C.class, "m", long.class); }
 *
 * and an Error sails straight through that catch. H2's FullTextLucene does
 * exactly this to support both Lucene 9 (field TotalHits.value) and Lucene 10
 * (accessor TotalHits.value()).
 */
public class MhLookupProbe {
    public static class Target {
        public final long value = 42L;
        public long realMethod() {
            return 7L;
        }
        public static long staticMethod() {
            return 9L;
        }
    }

    static void p(String label, Object v) {
        System.out.println(label + "=" + v);
        System.out.flush();
    }

    interface T {
        Object get() throws Throwable;
    }

    static void check(String label, T t) {
        try {
            p(label, t.get());
        } catch (Throwable e) {
            p(label, "THREW " + e.getClass().getName());
        }
    }

    public static void main(String[] a) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodType longType = MethodType.methodType(long.class);

        // --- present members resolve ---
        check("findVirtual.present", () ->
            (long) lk.findVirtual(Target.class, "realMethod", longType).invoke(new Target()));
        check("findStatic.present", () ->
            (long) lk.findStatic(Target.class, "staticMethod", longType).invoke());
        check("findGetter.present", () ->
            (long) lk.findGetter(Target.class, "value", long.class).invoke(new Target()));

        // --- missing members raise the CHECKED exception ---
        check("findVirtual.missing", () -> lk.findVirtual(Target.class, "nope", longType));
        check("findStatic.missing", () -> lk.findStatic(Target.class, "nope", longType));
        check("findSpecial.missing", () ->
            lk.findSpecial(Target.class, "nope", longType, MhLookupProbe.class));
        check("findGetter.missingField", () -> lk.findGetter(Target.class, "nope", long.class));
        check("findSetter.missingField", () -> lk.findSetter(Target.class, "nope", long.class));

        // --- is it catchable as Exception? that is the whole point ---
        check("findVirtual.missing.caughtAsException", () -> {
            try {
                lk.findVirtual(Target.class, "nope", longType);
                return "NO THROW";
            } catch (Exception e) {
                return "caught " + e.getClass().getSimpleName();
            }
        });

        // --- H2's exact version-probe shape, against our own Target ---
        check("h2Pattern.fallsBackToField", () -> {
            MethodHandle mh;
            try {
                mh = lk.findVirtual(Target.class, "value", longType);
            } catch (Exception e) {
                mh = lk.findGetter(Target.class, "value", long.class);
            }
            return (long) mh.invoke(new Target());
        });

        // --- and against the real Lucene class H2 probes ---
        check("h2Pattern.luceneTotalHits", () -> {
            Class<?> th = Class.forName("org.apache.lucene.search.TotalHits");
            MethodHandle mh;
            try {
                mh = lk.findVirtual(th, "value", longType);
            } catch (Exception e) {
                mh = lk.findGetter(th, "value", long.class);
            }
            Class<?> rel = Class.forName("org.apache.lucene.search.TotalHits$Relation");
            Object eq = rel.getEnumConstants()[0];
            Object inst = th.getConstructor(long.class, rel).newInstance(123L, eq);
            return (long) mh.invoke(inst);
        });

        p("DONE", "ok");
    }
}
