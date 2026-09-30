import java.lang.reflect.Field;
import java.lang.reflect.Method;

/** Can `objectFieldOffset1`'s synthetic-offset MINT be reached by the consumers
 *  its own registrar comment names? (L1 residual R1.)
 *
 *  The mint fires only when `Unsafe.objectFieldOffset(Class, String)` cannot
 *  find the named field. Its two documented consumers are
 *  `java.lang.Class$Atomic.casReflectionData` and Spring Boot's
 *  `AbstractClassLoaderValue.putIfAbsent`. On JDK 25:
 *
 *    * `Class$Atomic.<clinit>` resolves exactly THREE names on
 *      `java.lang.Class` -- reflectionData, annotationType, annotationData --
 *      via the 2-arg `objectFieldOffset`, once per VM (verified with javap);
 *    * `jdk.internal.loader.AbstractClassLoaderValue` references `Unsafe`
 *      ZERO times, so that half of the citation is stale.
 *
 *  This drives the first hard (reflection that forces the reflectionData cache
 *  and the annotation caches) and prints a POSITIVE CONTROL first, because a
 *  zero from an instrument that cannot fire is not a zero -- this lane has
 *  already been caught by that three times.
 *
 *  The measurement is on STDERR: `objectFieldOffset1: field ... not found ...
 *  minting synthetic offset`. stdout only says what was exercised.
 */
public class MintReachProbe {

    @Deprecated
    static class Annotated {
        int a; long b; Object c;
        void m1() {}
        void m2(int x) {}
    }

    public static void main(String[] args) throws Exception {
        Field f = Class.forName("sun.misc.Unsafe").getDeclaredField("theInternalUnsafe");
        f.setAccessible(true);
        jdk.internal.misc.Unsafe v = (jdk.internal.misc.Unsafe) f.get(null);

        // ---- POSITIVE CONTROL -------------------------------------------
        // A name that certainly does not exist. If the mint is reachable at
        // all, this reaches it, and the warn must appear on stderr. If this is
        // silent the instrument is broken and every zero below is void.
        long bogus = v.objectFieldOffset(Annotated.class, "noSuchFieldAnywhere20260829");
        System.out.println("control: minted offset is non-zero |" + (bogus != 0) + "|");
        long bogus2 = v.objectFieldOffset(String.class, "alsoNotAField20260829");
        System.out.println("control: second mint distinct |" + (bogus2 != bogus) + "|");

        // ---- the three names Class$Atomic actually resolves ---------------
        for (String name : new String[] { "reflectionData", "annotationType", "annotationData" }) {
            long off = v.objectFieldOffset(Class.class, name);
            System.out.println("Class." + name + " resolved non-zero |" + (off != 0) + "|");
        }

        // ---- drive the consumers for real --------------------------------
        // getDeclaredFields/Methods populate and then CAS `reflectionData`;
        // getAnnotations populates `annotationData` / `annotationType`. Two
        // passes so the second hits the cached path the CAS guards.
        int fields = 0, methods = 0, anns = 0;
        for (int pass = 0; pass < 2; pass++) {
            for (Class<?> c : new Class<?>[] { Annotated.class, String.class, Integer.class,
                                               java.util.HashMap.class, Deprecated.class,
                                               MintReachProbe.class, Thread.class }) {
                fields += c.getDeclaredFields().length;
                methods += c.getDeclaredMethods().length;
                anns += c.getAnnotations().length;
                for (Method m : c.getDeclaredMethods()) {
                    anns += m.getAnnotations().length;
                }
            }
        }
        System.out.println("reflection exercised |fields=" + (fields > 0)
                           + " methods=" + (methods > 0) + " anns>=0=" + (anns >= 0) + "|");
        System.out.println("DONE MintReachProbe");
    }
}
