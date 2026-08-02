import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

/**
 * Run one no-arg static method of a corpus fixture and report what it did.
 *
 * `vm/tests/interpreter_tests.rs` reports a failure as
 * `Err(ExceptionThrown(ObjectRef { .. }))` — an address, with no class, no
 * message and no stack. That is unusable for grouping 251 failures by cause,
 * so this runs the same call through reflection and prints the exception
 * itself.
 *
 *   cratonvm -cp <classes-dir> CorpusMethodProbe cratonvm/MemoryModelTest testSynchronizedCounter
 */
public final class CorpusMethodProbe {

    private CorpusMethodProbe() {
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 2) {
            throw new IllegalArgumentException("usage: CorpusMethodProbe <class> <method>");
        }
        String className = args[0].replace('/', '.');
        Method method;
        try {
            Class<?> type = Class.forName(className);
            method = type.getDeclaredMethod(args[1]);
        } catch (Throwable resolution) {
            System.out.println("RESOLVE-FAILED " + resolution);
            return;
        }
        method.setAccessible(true);
        try {
            Object result = method.invoke(null);
            System.out.println("RETURNED " + result);
        } catch (InvocationTargetException wrapped) {
            Throwable cause = wrapped.getCause();
            System.out.println("THREW " + (cause == null ? "<null cause>" : cause.getClass().getName()
                    + ": " + cause.getMessage()));
            if (cause != null) {
                StackTraceElement[] frames = cause.getStackTrace();
                for (int i = 0; i < Math.min(frames.length, 8); i++) {
                    System.out.println("    at " + frames[i]);
                }
            }
        } catch (Throwable other) {
            System.out.println("INVOKE-FAILED " + other);
        }
    }
}
