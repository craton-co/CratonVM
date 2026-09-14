import java.lang.annotation.*;
import java.lang.reflect.*;

/** Array.set into an interface-typed array with a Proxy / subclass element. */
public class ArraySetProxyRepro {
    @Retention(RetentionPolicy.RUNTIME)
    public @interface Ann { String value() default "v"; }

    public interface Plain { String name(); }

    static String t(String w, java.util.concurrent.Callable<Object> c) {
        try { return w + "=" + String.valueOf(c.call()); }
        catch (Throwable e) { return w + "=THREW " + e.getClass().getName() + ": " + e.getMessage(); }
    }

    public static void main(String[] a) throws Exception {
        ClassLoader cl = ArraySetProxyRepro.class.getClassLoader();
        InvocationHandler h = (p, m, args) -> {
            if (m.getName().equals("value")) return "v";
            if (m.getName().equals("name")) return "n";
            if (m.getName().equals("annotationType")) return Ann.class;
            if (m.getName().equals("toString")) return "@Ann(v)";
            if (m.getName().equals("hashCode")) return 1;
            if (m.getName().equals("equals")) return p == args[0];
            return null;
        };
        Object annProxy = Proxy.newProxyInstance(cl, new Class<?>[]{Ann.class}, h);
        Object plainProxy = Proxy.newProxyInstance(cl, new Class<?>[]{Plain.class}, h);

        System.out.println("annProxy class=" + annProxy.getClass().getName()
                + " isAnn=" + (annProxy instanceof Ann)
                + " assignable=" + Ann.class.isAssignableFrom(annProxy.getClass())
                + " ifaces=" + java.util.Arrays.toString(annProxy.getClass().getInterfaces()));

        Object annArr = Array.newInstance(Ann.class, 1);
        System.out.println("annArr class=" + annArr.getClass().getName()
                + " comp=" + annArr.getClass().getComponentType().getName());
        System.out.println(t("Array.set(Ann[],proxy)", () -> { Array.set(annArr, 0, annProxy); return "ok"; }));
        System.out.println(t("Array.get(Ann[],0)", () -> Array.get(annArr, 0)));

        Object plainArr = Array.newInstance(Plain.class, 1);
        System.out.println(t("Array.set(Plain[],proxy)", () -> { Array.set(plainArr, 0, plainProxy); return "ok"; }));

        // Plain aastore (bytecode) for comparison.
        Ann[] direct = new Ann[1];
        System.out.println(t("aastore Ann[]", () -> { direct[0] = (Ann) annProxy; return "ok"; }));

        // Interface array with an ordinary implementing class.
        Object plainArr2 = Array.newInstance(Plain.class, 1);
        Plain impl = () -> "impl";
        System.out.println(t("Array.set(Plain[],lambda)", () -> { Array.set(plainArr2, 0, impl); return "ok"; }));

        // Superclass-typed array with a subclass element.
        Object objArr = Array.newInstance(CharSequence.class, 1);
        System.out.println(t("Array.set(CharSequence[],String)", () -> { Array.set(objArr, 0, "s"); return "ok"; }));
    }
}
