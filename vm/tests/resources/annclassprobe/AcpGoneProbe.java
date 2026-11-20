import java.lang.annotation.Annotation;
import java.lang.reflect.Method;

// `AcpGoneTarget` carries two `@AcpGone` repeats, so javac emitted an
// `@AcpGones({@AcpGone("x"), @AcpGone("y")})` container. `AcpGone.class` was
// moved to `compileonly/` after compilation and is NOT on the runtime
// classpath — the `org.apiguardian.api.API` shape, where an annotation's jar is
// compile-scoped only.
//
// HotSpot throws NoClassDefFoundError straight out of getDeclaredAnnotations().
// CratonVM may instead surface the container and defer the failure to member
// ACCESS. What NEITHER may do is hand back a live annotation whose
// annotationType() is null: JUnit's AnnotationUtils.findRepeatableAnnotations
// walks exactly this array and calls `candidateAnnotationType.equals(...)` on
// every entry.
//
// Prints `GONE-SUMMARY: nullTypes=<n>`; anything but 0 is the defect.
public class AcpGoneProbe {

    private static int nullTypes = 0;
    private static int nonAnnotations = 0;

    private static void check(String where, Object o) {
        if (o == null) {
            System.out.println("  " + where + ": null entry");
            return;
        }
        if (!(o instanceof Annotation)) {
            // Counted, not merely printed. This branch existed and stayed quiet
            // while `org.infinispan.query.remote.client.impl.QueryRequest` was
            // unmockable for exactly this reason: `getDeclaredAnnotations()`
            // handed back a `Proxy` built over a fabricated stand-in for an
            // annotation type that is on no classpath, and a proxy over a
            // non-interface with no superinterfaces is not an `Annotation`.
            // Byte Buddy's `AnnotationList$ForLoadedAnnotations` casts every
            // element to `Annotation`, so this is a defect, not an observation.
            System.out.println("  " + where + ": non-annotation " + o.getClass().getName()
                    + "  [DEFECT]");
            nonAnnotations++;
            return;
        }
        Class<? extends Annotation> t = ((Annotation) o).annotationType();
        if (t == null) {
            System.out.println("  " + where + ": annotationType() = NULL  [DEFECT]");
            nullTypes++;
        } else {
            System.out.println("  " + where + ": annotationType() = " + t.getName());
        }
    }

    public static void main(String[] args) {
        container();
        solo();
        System.out.println("GONE-SUMMARY: nullTypes=" + nullTypes
                + " nonAnnotations=" + nonAnnotations);
    }

    /// `AcpGoneTarget`: a LOADABLE `@Repeatable` container whose entries have an
    /// unloadable type.
    private static void container() {
        Annotation[] anns;
        try {
            anns = Class.forName("AcpGoneTarget").getDeclaredAnnotations();
        } catch (Throwable t) {
            // HotSpot's answer: NoClassDefFoundError before anything is built.
            // Nothing with a null type ever reached the caller. This used to
            // `return` from main, which skipped every later case AND printed
            // the summary itself — so on HotSpot the solo case below never ran
            // and the run still looked complete.
            System.out.println("getDeclaredAnnotations() threw " + t.getClass().getName()
                    + ": " + t.getMessage() + "  [no null-typed annotation escaped]");
            return;
        }
        System.out.println("getDeclaredAnnotations() count=" + anns.length);
        for (Annotation a : anns) {
            check("declared", a);
            if (a == null) {
                continue;
            }
            Class<? extends Annotation> t = a.annotationType();
            if (t == null || !t.getName().equals("AcpGones")) {
                continue;
            }
            Method value;
            try {
                value = t.getMethod("value");
            } catch (Throwable e) {
                System.out.println("  getMethod(value) threw " + e.getClass().getName()
                        + "  [acceptable — the member type is unresolvable]");
                continue;
            }
            try {
                Object result = value.invoke(a);
                if (result == null) {
                    System.out.println("  value() = null  [acceptable — no live entry escaped]");
                    continue;
                }
                Object[] entries = (Object[]) result;
                System.out.println("  value().length = " + entries.length);
                for (Object e : entries) {
                    check("entry", e);
                }
            } catch (Throwable e) {
                Throwable c = (e instanceof java.lang.reflect.InvocationTargetException)
                        ? e.getCause() : e;
                System.out.println("  value() threw " + c.getClass().getName()
                        + "  [acceptable — deferred, nothing escaped]");
            }
        }
    }

    /// `AcpGoneSolo`: the shape that escaped. A DIRECTLY APPLIED annotation
    /// whose own type is unresolvable, in an ENTERPRISE-PREFIXED package
    /// (`org/jboss/`) — the prefix is load-bearing, because that is what makes
    /// this VM fabricate a synthetic stand-in instead of simply not finding the
    /// class. The same fixture in the default package passes on a broken
    /// binary and proves nothing.
    ///
    /// `getDeclaredAnnotations()` may report it as absent (HotSpot's
    /// `AnnotationParser` drops an annotation whose type will not resolve) or
    /// throw; what it may not do is return an element that is not an
    /// `Annotation`.
    private static void solo() {
        try {
            Object[] solo = Class.forName("AcpGoneSolo").getDeclaredAnnotations();
            System.out.println("AcpGoneSolo.getDeclaredAnnotations() count=" + solo.length);
            for (Object a : solo) {
                check("solo", a);
            }
        } catch (Throwable t) {
            System.out.println("AcpGoneSolo.getDeclaredAnnotations() threw "
                    + t.getClass().getName() + "  [no non-annotation escaped]");
        }
    }
}
