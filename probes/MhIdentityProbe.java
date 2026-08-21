import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;

/**
 * `MethodHandle` adapters are PURE: `asFixedArity()`, `asVarargsCollector()`,
 * `asType()` and `bindTo()` each return a NEW handle and leave the receiver
 * exactly as it was. Code that asks a handle a question after someone else
 * adapted it must get the same answer as before.
 *
 * CratonVM models a handle as one mutable object and applies varargs
 * semantics at dispatch, so `asFixedArity()` cleared the marking on the
 * RECEIVER. Spring's `FunctionReference.executeFunctionViaMethodHandle` reads
 * `methodHandle.isVarargsCollector()` on every evaluation of the same
 * registered handle, so one adapter call anywhere changes how every later
 * evaluation is routed.
 */
public class MhIdentityProbe {

    public static String vf(String... strings) {
        return Arrays.toString(strings);
    }

    public static String fixed(String[] a) {
        return "fixed:" + Arrays.toString(a);
    }

    interface Thrower { Object get() throws Throwable; }

    static void row(String name, Thrower t) {
        String v;
        try {
            v = String.valueOf(t.get());
        }
        catch (Throwable ex) {
            v = "THREW " + ex.getClass().getName() + ": " + ex.getMessage();
        }
        System.out.println(name + " = " + v);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup L = MethodHandles.lookup();
        MethodType vfType = MethodType.methodType(String.class, String[].class);

        MethodHandle a = L.findStatic(MhIdentityProbe.class, "vf", vfType);
        row("I01 a.isVarargsCollector", () -> a.isVarargsCollector());
        MethodHandle b = a.asFixedArity();
        row("I02 b.isVarargsCollector", () -> b.isVarargsCollector());
        row("I03 a.isVarargsCollector AFTER asFixedArity", () -> a.isVarargsCollector());
        row("I04 a==b", () -> a == b);
        row("I05 a.iwa(x,y) AFTER asFixedArity", () -> a.invokeWithArguments("x", "y"));
        row("I06 b.iwa(x,y)", () -> b.invokeWithArguments("x", "y"));

        MethodHandle c = L.findStatic(MhIdentityProbe.class, "fixed", vfType);
        row("I07 c.isVarargsCollector", () -> c.isVarargsCollector());
        MethodHandle d = c.asVarargsCollector(String[].class);
        row("I08 d.isVarargsCollector", () -> d.isVarargsCollector());
        row("I09 c.isVarargsCollector AFTER asVarargsCollector", () -> c.isVarargsCollector());
        row("I10 c==d", () -> c == d);

        MethodHandle e = L.findStatic(MhIdentityProbe.class, "vf", vfType);
        row("I11 e.type", () -> e.type());
        MethodHandle f = e.asType(MethodType.methodType(Object.class, Object.class));
        row("I12 f.type", () -> f.type());
        row("I13 e.type AFTER asType", () -> e.type());
        row("I14 e==f", () -> e == f);
        row("I15 e.isVarargsCollector AFTER asType", () -> e.isVarargsCollector());
        row("I16 e.iwa(x,y) AFTER asType", () -> e.invokeWithArguments("x", "y"));

        MethodHandle g = L.findStatic(MhIdentityProbe.class, "vf", vfType);
        MethodHandle h = g.asCollector(String[].class, 2);
        row("I17 h.type", () -> h.type());
        row("I18 g.type AFTER asCollector", () -> g.type());
        row("I19 g.isVarargsCollector AFTER asCollector", () -> g.isVarargsCollector());
        row("I20 g.iwa(x,y) AFTER asCollector", () -> g.invokeWithArguments("x", "y"));

        MethodHandle i = L.findStatic(MhIdentityProbe.class, "vf", vfType);
        MethodHandle j = i.asSpreader(String[].class, 1);
        row("I21 j.type", () -> j.type());
        row("I22 i.type AFTER asSpreader", () -> i.type());
        row("I23 i.iwa(x,y) AFTER asSpreader", () -> i.invokeWithArguments("x", "y"));

        MethodHandle k = L.findStatic(MhIdentityProbe.class, "vf", vfType);
        MethodHandle m = MethodHandles.dropArguments(k, 0, int.class);
        row("I24 m.type", () -> m.type());
        row("I25 k.type AFTER dropArguments", () -> k.type());
        row("I26 k.iwa(x,y) AFTER dropArguments", () -> k.invokeWithArguments("x", "y"));

        // The Spring shape: register once, evaluate several times, with an
        // unrelated adapter call in between.
        MethodHandle reg = L.findStatic(MhIdentityProbe.class, "vf", vfType);
        row("I27 eval#1", () -> reg.invokeWithArguments(new Object[] {null}));
        MethodHandle ignored = reg.asFixedArity();
        row("I28 eval#2 after someone else adapted", () -> reg.invokeWithArguments(new Object[] {null}));
        row("I29 ignored.isVarargsCollector", () -> ignored.isVarargsCollector());
    }
}
