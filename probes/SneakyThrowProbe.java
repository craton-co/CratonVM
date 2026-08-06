// Minimal, Hibernate-free repro for the "compiled `checkcast erased-T; athrow`
// fails to throw" defect documented in
// fixed-suite-bugs/hibernate/offsetdatetimetest-zoneddatetimetest-athrow-ir-sneaky-throw-swallowed-20260804-FIXED.md
//
// `throwAs` is the JUnit Platform `ExceptionUtils` sneaky-throw shape: a
// `void`-returning method whose entire body is `checkcast <erased-to-Throwable>`
// followed by `athrow`. If the compiled body ever returns normally instead of
// throwing, `throwAsUnchecked` falls through to `return null`, and the caller's
// `throw null` becomes a helpful-NPE that has swallowed the real exception.
//
// HotSpot control: npes=0, every run.
public class SneakyThrowProbe {
    static RuntimeException throwAsUnchecked(Throwable t) {
        SneakyThrowProbe.<RuntimeException>throwAs(t);
        return null; // unreachable if throwAs really throws
    }

    @SuppressWarnings("unchecked")
    private static <T extends Throwable> void throwAs(Throwable t) throws T {
        throw (T) t;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 300000;
        int npes = 0;
        for (int i = 0; i < n; i++) {
            try {
                throw throwAsUnchecked(new RuntimeException("probe" + i));
            } catch (NullPointerException npe) {
                npes++;
                System.out.println("NPE (sneaky throw failed to throw) at i=" + i + ": " + npe);
            } catch (RuntimeException e) {
                /* expected */
            }
        }
        System.out.println("done npes=" + npes);
    }
}
