/*
 * Interpreter round i1 wave 9, lane L6: the HOST of a lambda made through a
 * hand-written `LambdaMetafactory.metafactory` call (the `native-builtins`
 * shim, `build_reflective_lambda_callsite`). HotSpot's host is the caller's
 * lookup class: the spun class is named `<lookup class>$$Lambda/0x...` and is
 * its nestmate. Before wave 9 CratonVM recorded no host for this path and
 * named a method reference to a JDK method after the method's owner
 * (`java.lang.String$$Lambda/0x...`). The same bootstrap arguments used from
 * two lookup classes must also give two classes (the reflective CallSite
 * cache is keyed by the lookup class since wave 9).
 *
 * Stdout is deterministic. HotSpot 25 prints exactly:
 *
 *   apply=5
 *   hostName=true
 *   nestHost=ReflectiveLambdaHostProbe
 *   otherApply=3
 *   otherHostName=true
 *   otherNestHost=ReflectiveLambdaHostProbeOther
 *   distinctClasses=true
 *
 * Run with and without --nojit. javac emits two class files (this class and
 * the package-private `ReflectiveLambdaHostProbeOther`); run from the javac
 * output directory.
 */
import java.lang.invoke.CallSite;
import java.lang.invoke.LambdaMetafactory;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.ToIntFunction;

public class ReflectiveLambdaHostProbe {
    static final MethodType FACTORY = MethodType.methodType(ToIntFunction.class);
    static final MethodType SAM = MethodType.methodType(int.class, Object.class);
    static final MethodType INSTANTIATED = MethodType.methodType(int.class, String.class);

    @SuppressWarnings("unchecked")
    static ToIntFunction<String> make(MethodHandles.Lookup lookup, MethodHandle impl)
            throws Throwable {
        CallSite cs = LambdaMetafactory.metafactory(
                lookup, "applyAsInt", FACTORY, SAM, impl, INSTANTIATED);
        return (ToIntFunction<String>) cs.getTarget().invoke();
    }

    static String nestHost(Object o) {
        try {
            return o.getClass().getNestHost().getName();
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle length = lookup.findVirtual(
                String.class, "length", MethodType.methodType(int.class));

        ToIntFunction<String> mine = make(lookup, length);
        System.out.println("apply=" + mine.applyAsInt("hello"));
        System.out.println("hostName="
                + mine.getClass().getName().startsWith("ReflectiveLambdaHostProbe$$Lambda/0x"));
        System.out.println("nestHost=" + nestHost(mine));

        ToIntFunction<String> other = make(ReflectiveLambdaHostProbeOther.lookup(), length);
        System.out.println("otherApply=" + other.applyAsInt("abc"));
        System.out.println("otherHostName=" + other.getClass().getName()
                .startsWith("ReflectiveLambdaHostProbeOther$$Lambda/0x"));
        System.out.println("otherNestHost=" + nestHost(other));
        System.out.println("distinctClasses=" + (mine.getClass() != other.getClass()));
    }
}

class ReflectiveLambdaHostProbeOther {
    static MethodHandles.Lookup lookup() {
        return MethodHandles.lookup();
    }
}
