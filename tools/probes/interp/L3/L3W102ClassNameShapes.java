import java.io.InputStream;
import java.lang.constant.ClassDesc;
import java.lang.invoke.MethodHandles;
import java.util.*;
import java.util.function.*;

/**
 * Interpreter round i5 wave 102, lane L3 (hunt): the name and shape answers
 * of java.lang.Class for the class kinds whose answers are derived rather
 * than read: anonymous, local, local record, anonymous inside a lambda, lambda
 * and method-reference proxies (this VM spins no class bytes for them),
 * hidden classes defined from a member class's bytes and from a top-level
 * class's bytes, an enum constant with a body, and arrays of each (arrays of
 * hidden classes included).
 *
 * Per class: getName, getSimpleName, getCanonicalName, getTypeName,
 * toGenericString, descriptorString, describeConstable, isHidden,
 * isAnonymousClass, isLocalClass, isMemberClass, isSynthetic, getModifiers,
 * getEnclosingClass, getNestHost, getPackageName. Hidden-class addresses are
 * folded to ADDR; a throw prints its class name.
 *
 * Run: java -cp . L3W102ClassNameShapes (both modes, JIT and --nojit).
 * Expected: whatever HotSpot 25 prints; diff row by row.
 */
public class L3W102ClassNameShapes {
    static class Member<T extends Comparable<? super T>> {}
    interface Iface<E> {}
    @interface Ann {}
    enum WithBody { A { void f() {} }; void f() {} }
    record Rec<X>(X x) {}
    public static final class Victim { public static String hi() { return "hi"; } }
    static class Outer { class Inner {} }

    static String fold(String s) {
        if (s == null) return "null";
        return s.replaceAll("([/.])0x[0-9a-f]+", "$1ADDR")
                .replaceAll("\\$\\$Lambda\\$[0-9]+", "\\$\\$Lambda");
    }

    static String safe(Supplier<String> s) {
        try {
            return fold(s.get());
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static String nameOf(Class<?> c) {
        return c == null ? "null" : c.getName();
    }

    static void row(String label, Class<?> c) {
        StringBuilder sb = new StringBuilder(label).append(':');
        sb.append(" name=").append(safe(c::getName));
        sb.append(" simple=").append(safe(c::getSimpleName));
        sb.append(" canonical=").append(safe(c::getCanonicalName));
        sb.append(" type=").append(safe(c::getTypeName));
        sb.append(" generic=").append(safe(c::toGenericString));
        sb.append(" desc=").append(safe(c::descriptorString));
        sb.append(" constable=").append(safe(() -> c.describeConstable()
                .map(ClassDesc::descriptorString).orElse("empty")));
        sb.append(" hidden=").append(safe(() -> String.valueOf(c.isHidden())));
        sb.append(" anon=").append(safe(() -> String.valueOf(c.isAnonymousClass())));
        sb.append(" local=").append(safe(() -> String.valueOf(c.isLocalClass())));
        sb.append(" member=").append(safe(() -> String.valueOf(c.isMemberClass())));
        sb.append(" synthetic=").append(safe(() -> String.valueOf(c.isSynthetic())));
        sb.append(" mods=").append(safe(() -> Integer.toHexString(c.getModifiers())));
        sb.append(" enclosing=").append(safe(() -> nameOf(c.getEnclosingClass())));
        sb.append(" nesthost=").append(safe(() -> nameOf(c.getNestHost())));
        sb.append(" pkg=").append(safe(c::getPackageName));
        System.out.println(sb);
    }

    static Class<?> hidden(String resource) throws Exception {
        byte[] bytes;
        try (InputStream in = L3W102ClassNameShapes.class.getResourceAsStream(resource)) {
            bytes = in.readAllBytes();
        }
        return MethodHandles.lookup().defineHiddenClass(bytes, true).lookupClass();
    }

    public static void main(String[] args) throws Exception {
        class Local {}
        record LocalRec(int a) {}
        Object anon = new Object() {};
        Supplier<Object> makesAnon = () -> new Object() {};
        Runnable lambda = () -> {};
        Supplier<String> methodRef = Victim::hi;
        Function<String, Integer> generic = Integer::valueOf;

        Map<String, Class<?>> rows = new LinkedHashMap<>();
        rows.put("member-generic", Member.class);
        rows.put("iface-generic", Iface.class);
        rows.put("annotation", Ann.class);
        rows.put("enum-body-constant", WithBody.A.getClass());
        rows.put("record-generic", Rec.class);
        rows.put("inner", Outer.Inner.class);
        rows.put("local", Local.class);
        rows.put("local-record", LocalRec.class);
        rows.put("anonymous", anon.getClass());
        rows.put("anonymous-in-lambda", makesAnon.get().getClass());
        rows.put("lambda", lambda.getClass());
        rows.put("method-ref", methodRef.getClass());
        rows.put("generic-method-ref", generic.getClass());
        rows.put("hidden-member", hidden("L3W102ClassNameShapes$Victim.class"));
        rows.put("hidden-top", hidden("L3W102TopVictim.class"));
        rows.put("int[][]", int[][].class);
        rows.put("member[]", Member[].class);
        rows.put("inner[][]", Outer.Inner[][].class);

        List<Map.Entry<String, Class<?>>> base = new ArrayList<>(rows.entrySet());
        for (Map.Entry<String, Class<?>> e : base) {
            row(e.getKey(), e.getValue());
        }
        // Arrays of the derived kinds (an array of a hidden class is legal).
        for (String k : List.of("local", "anonymous", "lambda", "hidden-member", "hidden-top")) {
            Class<?> c = rows.get(k);
            Class<?> arr;
            try {
                arr = c.arrayType();
            } catch (Throwable t) {
                System.out.println(k + "[]: arrayType threw " + t.getClass().getName());
                continue;
            }
            row(k + "[]", arr);
        }
    }
}

class L3W102TopVictim {
    static String hi() { return "top"; }
}
