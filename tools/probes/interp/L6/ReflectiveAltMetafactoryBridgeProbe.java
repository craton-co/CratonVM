/*
 * Interpreter round i1 wave 7, lane L6: `altMetafactory` FLAG_BRIDGES through
 * the REFLECTIVE path (a hand-written `LambdaMetafactory.altMetafactory`
 * call, the `native-builtins` shim), plus the shape of a lambda class name.
 *
 * The reflective call below is exactly what javac emits for
 * `(ObjM & StrM) () -> "s"`: invoked type `()ObjM`, SAM `()String`, one marker
 * (StrM) and one bridge (`()Object`). A call through `ObjM.m()Object` must
 * reach the body. Wave 7 records the bridge on this path too
 * (`NativeContext::register_lambda_proxy_bridges`).
 *
 * `*Tail` lines print the length of the hex tail after `$$Lambda/0x` of a
 * lambda class name: HotSpot prints the hidden class address as 16
 * zero-padded hex digits; wave 7 gives CratonVM's proxy names the same width.
 *
 * Stdout is deterministic. HotSpot 25 prints exactly:
 *
 *   reflectiveStr=s
 *   reflectiveObj=s
 *   reflectiveMarker=true
 *   reflectiveTail=16
 *   javacTail=16
 *   javacLambdaShaped=true
 *
 * Run with and without --nojit.
 */
import java.lang.invoke.CallSite;
import java.lang.invoke.LambdaMetafactory;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.Supplier;

public class ReflectiveAltMetafactoryBridgeProbe {
    public interface ObjM {
        Object m();
    }

    public interface StrM {
        String m();
    }

    static String impl() {
        return "s";
    }

    static int tail(Object o) {
        String n = o.getClass().getName();
        int i = n.indexOf("$$Lambda/0x");
        if (i < 0) {
            return -1;
        }
        String hex = n.substring(i + "$$Lambda/0x".length());
        for (int k = 0; k < hex.length(); k++) {
            if (Character.digit(hex.charAt(k), 16) < 0) {
                return -2;
            }
        }
        return hex.length();
    }

    static Object viaObjM(Object o) {
        return ((ObjM) o).m();
    }

    static String viaStrM(Object o) {
        return ((StrM) o).m();
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodType sam = MethodType.methodType(String.class);
        MethodHandle impl = lookup.findStatic(ReflectiveAltMetafactoryBridgeProbe.class, "impl", sam);
        CallSite cs = LambdaMetafactory.altMetafactory(
                lookup,
                "m",
                MethodType.methodType(ObjM.class),
                sam,
                impl,
                sam,
                LambdaMetafactory.FLAG_MARKERS | LambdaMetafactory.FLAG_BRIDGES,
                1,
                StrM.class,
                1,
                MethodType.methodType(Object.class));
        Object lambda = cs.getTarget().invoke();

        String str;
        try {
            str = viaStrM(lambda);
        } catch (Throwable t) {
            str = t.getClass().getName();
        }
        System.out.println("reflectiveStr=" + str);
        String obj;
        try {
            obj = String.valueOf(viaObjM(lambda));
        } catch (Throwable t) {
            obj = t.getClass().getName();
        }
        System.out.println("reflectiveObj=" + obj);
        System.out.println("reflectiveMarker=" + (lambda instanceof StrM));
        System.out.println("reflectiveTail=" + tail(lambda));

        Supplier<String> s = () -> "x";
        System.out.println("javacTail=" + tail(s));
        System.out.println("javacLambdaShaped=" + s.getClass().getName().contains("$$Lambda"));
    }
}
