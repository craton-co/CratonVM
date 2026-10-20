import java.lang.reflect.*;
import java.util.*;
import java.util.function.*;

/**
 * Interpreter round i5 wave 104, lane L3 (hunt): Class's enclosing-member
 * answers for classes declared in initializers and lambda bodies, and for the
 * lambda proxy classes themselves, as HotSpot 25 gives them
 * (JVM_GetEnclosingMethodInfo: an EnclosingMethod attribute with method index
 * 0 -- a class declared in a static or instance initializer, or a field
 * initializer -- answers [class, null, null], so getEnclosingMethod and
 * getEnclosingConstructor are null but getEnclosingClass is the outer class;
 * a class declared inside a lambda body names the synthetic lambda$... method).
 *
 * Rows: "<shape>: enclosingMethod=<name or null> enclosingCtor=<..>
 * enclosingClass=<simple> declaringClass=<..> anonymous=<b> local=<b>
 * member=<b> simpleName='<..>' canonical=<..> hidden=<b> synthetic=<b>
 * nestHost=<simple>". Hidden-class addresses and lambda indices folded.
 *
 * Run: java -cp . L3W104EnclosingInInitializers, both modes, JIT and --nojit.
 * Expected: HotSpot 25's output.
 */
public class L3W104EnclosingInInitializers {
    static final Map<String, Class<?>> SHAPES = new LinkedHashMap<>();

    static Object staticFieldAnon = new Object() { };
    Object instanceFieldAnon = new Object() { };

    static {
        SHAPES.put("static-init anonymous", new Object() { }.getClass());
        class StaticInitLocal { }
        SHAPES.put("static-init local", StaticInitLocal.class);
        Runnable r = () -> {
            class InLambdaLocal { }
            SHAPES.put("static-init lambda-body local", InLambdaLocal.class);
            SHAPES.put("static-init lambda-body anonymous", new Object() { }.getClass());
        };
        r.run();
        SHAPES.put("static-init lambda proxy", r.getClass());
        Supplier<Object> s = Object::new;
        SHAPES.put("static-init method-ref proxy", s.getClass());
    }

    {
        SHAPES.put("instance-init anonymous", new Object() { }.getClass());
        class InstanceInitLocal { }
        SHAPES.put("instance-init local", InstanceInitLocal.class);
    }

    L3W104EnclosingInInitializers() {
        SHAPES.put("constructor anonymous", new Object() { }.getClass());
        Runnable r = () -> SHAPES.put("constructor lambda-body anonymous", new Object() { }.getClass());
        r.run();
    }

    static class Member {
        static {
            SHAPES.put("member static-init anonymous", new Object() { }.getClass());
        }
    }

    static String fold(String s) {
        if (s == null) return "null";
        return s.replaceAll("([/.])0x[0-9a-f]+", "$1ADDR")
                .replaceAll("lambda\\$(\\w+)\\$\\d+", "lambda\\$$1\\$N");
    }

    static String simple(Class<?> c) {
        if (c == null) return "null";
        String n = c.getName();
        int d = n.indexOf('$');
        return fold(d < 0 ? n : n.substring(d));
    }

    static String describe(Class<?> c) {
        Method em = c.getEnclosingMethod();
        Constructor<?> ec = c.getEnclosingConstructor();
        return "enclosingMethod=" + (em == null ? "null" : fold(em.getName()) + "/" + Modifier.toString(em.getModifiers())
                        + (em.isSynthetic() ? " synthetic" : ""))
                + " enclosingCtor=" + (ec == null ? "null" : ec.getParameterCount() + "-arg")
                + " enclosingClass=" + simple(c.getEnclosingClass())
                + " declaringClass=" + simple(c.getDeclaringClass())
                + " anonymous=" + c.isAnonymousClass()
                + " local=" + c.isLocalClass()
                + " member=" + c.isMemberClass()
                + " simpleName='" + fold(c.getSimpleName()) + "'"
                + " canonical=" + fold(c.getCanonicalName())
                + " hidden=" + c.isHidden()
                + " synthetic=" + c.isSynthetic()
                + " nestHost=" + simple(c.getNestHost())
                + " name=" + simple(c);
    }

    public static void main(String[] args) {
        new L3W104EnclosingInInitializers();
        new Member();
        SHAPES.put("static-field anonymous", staticFieldAnon.getClass());
        SHAPES.put("instance-field anonymous", new L3W104EnclosingInInitializers().instanceFieldAnon.getClass());
        for (Map.Entry<String, Class<?>> e : SHAPES.entrySet()) {
            System.out.println(e.getKey() + ": " + describe(e.getValue()));
        }
    }
}
