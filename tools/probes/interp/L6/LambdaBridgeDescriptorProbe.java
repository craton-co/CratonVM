/*
 * Interpreter round i1 wave 6, lane L6: a lambda called through a BRIDGE
 * descriptor, i.e. not its exact SAM descriptor.
 *
 * Two ways a bridge exists:
 *
 *   1. In the functional interface itself. When `StrSupplier extends
 *      Supplier<String>` narrows `get()`, javac emits a synthetic default
 *      bridge `Object get()` in StrSupplier that calls `String get()`. A call
 *      through `Supplier.get:()Ljava/lang/Object;` resolves to that default,
 *      which then calls the SAM. CratonVM relies on this (its lambda dispatch
 *      only enters the body for the EXACT SAM descriptor, commit a857ee34c).
 *   2. Only in the spun lambda class, listed through altMetafactory's
 *      FLAG_BRIDGES. An intersection target `(ObjM & StrM)` whose two
 *      interfaces are unrelated has no interface that could hold the bridge,
 *      so HotSpot's spun class implements `Object m()` itself. Before wave 7
 *      CratonVM recorded no FLAG_BRIDGES descriptors, so a call through
 *      `ObjM.m:()Ljava/lang/Object;` had no body to reach (see
 *      docs/internal/fixed-bugs/interpreter-L6-lambda-flag-bridges-descriptors-are-dropped-FIXED-20260924.md);
 *      wave 7 records them and admits them through `lambda_accepts_descriptor`.
 *
 * Stdout is deterministic. HotSpot 25 prints exactly:
 *
 *   sam=x
 *   bridgeSupplier=x
 *   bridgeFunction=6
 *   bridgeFunctionCce=java.lang.ClassCastException
 *   bridgeNamed=n
 *   intersectionStr=s
 *   intersectionObj=s
 *
 * Run with and without --nojit. The last line is the one wave 7 fixed.
 */
import java.util.function.Function;
import java.util.function.Supplier;

public class LambdaBridgeDescriptorProbe {
    interface StrSupplier extends Supplier<String> {
        @Override
        String get();
    }

    interface IntOp extends Function<Integer, Integer> {
        @Override
        Integer apply(Integer x);
    }

    interface Named<T> {
        T name();
    }

    interface StrNamed extends Named<String> {
        @Override
        String name();
    }

    interface ObjM {
        Object m();
    }

    interface StrM {
        String m();
    }

    static Object viaSupplier(Supplier<?> s) {
        return s.get();
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object viaRawFunction(Function f, Object arg) {
        return f.apply(arg);
    }

    static Object viaNamed(Named<?> n) {
        return n.name();
    }

    static String viaStrM(Object o) {
        return ((StrM) o).m();
    }

    static Object viaObjM(Object o) {
        return ((ObjM) o).m();
    }

    public static void main(String[] args) {
        StrSupplier s = () -> "x";
        System.out.println("sam=" + s.get());
        System.out.println("bridgeSupplier=" + viaSupplier(s));

        IntOp inc = x -> x + 1;
        System.out.println("bridgeFunction=" + viaRawFunction(inc, 5));
        try {
            viaRawFunction(inc, "not an Integer");
            System.out.println("bridgeFunctionCce=none");
        } catch (RuntimeException e) {
            System.out.println("bridgeFunctionCce=" + e.getClass().getName());
        }

        StrNamed n = () -> "n";
        System.out.println("bridgeNamed=" + viaNamed(n));

        Object both = (ObjM & StrM) () -> "s";
        System.out.println("intersectionStr=" + viaStrM(both));
        String objResult;
        try {
            objResult = String.valueOf(viaObjM(both));
        } catch (Throwable t) {
            objResult = t.getClass().getName();
        }
        System.out.println("intersectionObj=" + objResult);
    }
}
