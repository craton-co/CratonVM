import java.lang.reflect.Method;

import jdk.internal.vm.Continuation;
import jdk.internal.vm.ContinuationScope;

/**
 * W7-86 — static natives whose Rust body indexes {@code args} as if the method
 * had a receiver.
 *
 * <p><b>Section A establishes the calling convention empirically</b>, which is
 * the thing this probe exists for. The convention is not read off the
 * dispatcher: it is read off natives the JDK provides <i>no bytecode for</i>, so
 * a right answer can only have come from the Rust body and cannot have come
 * from a fallback. {@code System.identityHashCode(Object)} is
 * {@code public static native}: its Rust body takes {@code args[0]} as the
 * object, and if a static were handed a receiver slot the object would be at
 * {@code args[1]} and the answer would be 0. {@code Object.wait(long)} is an
 * instance native whose body takes {@code args[0]} as the receiver and
 * {@code args[1]} as the millis. Both answering correctly in the SAME run is
 * the measurement: <b>{@code args} carries exactly the descriptor's
 * parameters, preceded by a receiver only for an instance method.</b>
 *
 * <p>Section B is the RED. {@code Continuation.pin()} / {@code unpin()} are
 * {@code public static native void} on JDK 25 (verified with
 * {@code javap -p --module java.base jdk.internal.vm.Continuation} against
 * Adoptium 25.0.3.9), so a call site is an {@code invokestatic} with zero
 * operands and the native receives an empty {@code args}. Both registrations
 * opened {@code obj_arg(args, 0)?}, which answers
 * {@code NullPointerException("null object argument")} on an empty slice.
 *
 * <p><b>The observable is the exception, not the absence of one.</b> "It did
 * not throw" is not a test of this: a native that read the wrong index and
 * happened not to fault also does not throw. What is printed is the exception
 * identity beside HotSpot's, so the two lines differ before the fix and agree
 * after it.
 *
 * <p>Section C is {@code ClassLoader.findBootstrapClass(String)}, which JDK 25
 * declares {@code private static native} while its Rust body read
 * {@code args[1]} — the shape its INSTANCE neighbour {@code findLoadedClass0}
 * legitimately has. Reading one past the end of a one-element {@code args} made
 * it answer null for every name. It is reached reflectively because it is
 * private; that needs {@code --add-opens java.base/java.lang=ALL-UNNAMED}, and
 * the probe says so rather than silently reporting the failure to open as the
 * defect.
 *
 * <p>Run (both spellings of {@code --add-exports} work on CratonVM):
 * <pre>
 *   javac --add-exports java.base/jdk.internal.vm=ALL-UNNAMED -d out probes/StaticNativeArityProbe.java
 *   java  --add-exports java.base/jdk.internal.vm=ALL-UNNAMED \
 *         --add-opens    java.base/java.lang=ALL-UNNAMED -cp out StaticNativeArityProbe
 * </pre>
 *
 * <p>Expected values are in {@code StaticNativeArityProbe.expected.txt},
 * measured on HotSpot, with the pre-fix CratonVM column beside them.
 */
public final class StaticNativeArityProbe {

    static void p(String k, Object v) {
        System.out.println(k + "=" + v);
    }

    /** Prints the outcome, naming the exception when there is one. */
    static String outcome(Call c) {
        try {
            Object v = c.call();
            return v == null ? "returned:null" : "returned:" + v;
        } catch (Throwable e) {
            String m = e.getMessage();
            return "THREW:" + e.getClass().getName() + (m == null ? "" : ":" + m);
        }
    }

    interface Call {
        Object call() throws Throwable;
    }

    public static void main(String[] args) {
        // ---- A. the calling convention, on natives with no bytecode behind ----
        Object o = new Object();
        p("A1 static-native identityHashCode(o)==o.hashCode()",
                outcome(() -> System.identityHashCode(o) == o.hashCode()));
        p("A2 static-native identityHashCode(null)", outcome(() -> System.identityHashCode(null)));
        p("A3 static-native StrictMath.sqrt(16.0)", outcome(() -> StrictMath.sqrt(16.0)));
        p("A4 instance-native o.getClass()", outcome(() -> o.getClass().getName()));
        p("A5 instance-native o.wait(1) millis-at-args1", outcome(() -> {
            long t0 = System.nanoTime();
            synchronized (o) {
                o.wait(1);
            }
            // A body that read the millis from the wrong slot would see no
            // timeout and block until interrupted; returning at all is the
            // evidence, and the elapsed bound keeps "returned" honest.
            return (System.nanoTime() - t0) < 10_000_000_000L;
        }));
        p("A6 static-native System.arraycopy 5-params-at-0..4", outcome(() -> {
            int[] src = {1, 2, 3, 4};
            int[] dst = new int[4];
            System.arraycopy(src, 0, dst, 0, 4);
            return dst[0] + "," + dst[3];
        }));

        // ---- B. the RED: two zero-parameter statics ----
        p("B1 Continuation.pin()", outcome(() -> {
            Continuation.pin();
            return "ok";
        }));
        p("B2 Continuation.unpin()", outcome(() -> {
            Continuation.unpin();
            return "ok";
        }));
        // Same pair from inside a mounted continuation, which is the only place
        // HotSpot's pin count means anything. On a VM that cannot construct a
        // real Continuation this line reports THAT, and does not pretend to
        // have measured pin/unpin.
        p("B3 pin/unpin inside a running Continuation", outcome(() -> {
            ContinuationScope scope = new ContinuationScope("w7-86");
            final String[] inner = {"body-never-ran"};
            Continuation c = new Continuation(scope, () -> {
                try {
                    Continuation.pin();
                    Continuation.unpin();
                    inner[0] = "ok";
                } catch (Throwable e) {
                    inner[0] = "THREW:" + e.getClass().getName();
                }
            });
            c.run();
            return inner[0];
        }));

        // ---- C. a static native that read args[1] ----
        p("C1 ClassLoader.findBootstrapClass(\"java.lang.String\")", outcome(() -> {
            Method m = ClassLoader.class.getDeclaredMethod("findBootstrapClass", String.class);
            try {
                m.setAccessible(true);
            } catch (RuntimeException e) {
                return "NEEDS --add-opens java.base/java.lang=ALL-UNNAMED";
            }
            Object r = m.invoke(null, "java.lang.String");
            return r == null ? "null" : ((Class<?>) r).getName();
        }));
        p("C2 ClassLoader.findBootstrapClass(\"no.such.Class\")", outcome(() -> {
            Method m = ClassLoader.class.getDeclaredMethod("findBootstrapClass", String.class);
            try {
                m.setAccessible(true);
            } catch (RuntimeException e) {
                return "NEEDS --add-opens java.base/java.lang=ALL-UNNAMED";
            }
            Object r = m.invoke(null, "no.such.Class");
            return r == null ? "null" : ((Class<?>) r).getName();
        }));
    }
}
