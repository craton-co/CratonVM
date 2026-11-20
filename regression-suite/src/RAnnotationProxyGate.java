import java.lang.annotation.*;
import java.lang.reflect.*;
import java.util.*;

/**
 * Drives the annotation-proxy dispatch gate in `execute_invokevirtual_cached`.
 *
 * That gate decides, per virtual invoke, whether the receiver is the VM's
 * synthetic `java/lang/annotation/AnnotationProxy` and must therefore bypass
 * the monomorphic inline cache and reach `execute_invoke`'s interception layer.
 * It used to answer by taking the class-manager read lock and comparing the
 * receiver class's NAME against a literal; since 2026-08-24 it answers from a
 * memoized `ClassId` whose negative half is keyed on the class-definition
 * epoch.
 *
 * Three things have to stay true, and each is a different way that memo could
 * be wrong:
 *
 *  1. A proxy method called through a WARM call site keeps returning the
 *     annotation's value -- an inline-cache entry must never serve a proxy.
 *  2. `equals`/`hashCode`/`toString`, which the proxy inherits from `Object`
 *     and has no bytecode for, keep the `Annotation` contract when warm.
 *  3. ONE call site alternating between a proxy receiver and an ordinary
 *     implementation of the same interface answers correctly for both -- the
 *     case a per-call-site memo would get wrong, and the reason the memo is
 *     keyed on the receiver's class rather than on the resolved target.
 *
 * Nothing in the suite drove a proxy through a warm site before this, so a memo
 * that answered `false` one query too early would have gone unnoticed.
 */
public class RAnnotationProxyGate {

    @Retention(RetentionPolicy.RUNTIME)
    @Target({ElementType.TYPE, ElementType.METHOD})
    public @interface Tag {
        String name();
        int order() default 7;
    }

    @Tag(name = "alpha", order = 1)
    static class Alpha {
        @Tag(name = "m1")
        void m1() {}
    }

    @Tag(name = "beta", order = 2)
    static class Beta {}

    /** A hand-written `Tag` so one call site can alternate proxy / non-proxy. */
    static final class PlainTag implements Tag {
        public String name() { return "plain"; }
        public int order() { return 99; }
        public Class<? extends Annotation> annotationType() { return Tag.class; }
    }

    /** The shared call site: one `invokeinterface` for both receiver kinds. */
    static String callName(Tag t) { return t.name(); }
    static int callOrder(Tag t) { return t.order(); }

    private static int checks = 0;

    private static void ck(String key, Object value) {
        checks++;
        System.out.println("CK RAnnotationProxyGate " + key + "=" + value);
    }

    public static void main(String[] args) throws Exception {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;

        Tag a = Alpha.class.getAnnotation(Tag.class);
        Tag b = Beta.class.getAnnotation(Tag.class);
        Tag m = Alpha.class.getDeclaredMethod("m1").getAnnotation(Tag.class);
        Tag p = new PlainTag();

        ck("annotationType", a.annotationType().getName());
        ck("aName", a.name());
        ck("aOrder", a.order());
        ck("bName", b.name());
        ck("bOrder", b.order());
        ck("mName", m.name());
        ck("mDefaultedOrder", m.order());

        // 1 -- hammer ONE call site with a proxy receiver until it is warm.
        long warm = 0;
        for (int i = 0; i < reps; i++) {
            warm += callName(a).length() + callOrder(a);
        }
        ck("warmProxyOnlySum", warm);

        // 3 -- the same site alternating proxy / non-proxy / other proxy.
        long mixed = 0;
        StringBuilder shape = new StringBuilder();
        for (int i = 0; i < reps; i++) {
            Tag t = switch (i % 3) {
                case 0 -> a;
                case 1 -> p;
                default -> b;
            };
            mixed += callName(t).length() + callOrder(t);
            if (i < 6) {
                shape.append(callName(t)).append('/').append(callOrder(t)).append(' ');
            }
        }
        ck("mixedSum", mixed);
        ck("mixedShape", shape.toString().trim());

        // 2 -- the Object-inherited surface, cold and then warm.
        ck("aEqualsSameProxy", a.equals(Alpha.class.getAnnotation(Tag.class)));
        ck("aEqualsOtherProxy", a.equals(b));
        ck("aEqualsPlainImpl", a.equals(p));
        ck("hashStableAcrossLookups",
                a.hashCode() == Alpha.class.getAnnotation(Tag.class).hashCode());
        String s = a.toString();
        ck("toStringCarriesMember", s.contains("alpha"));
        ck("toStringCarriesType", s.contains("Tag") || s.contains("tag"));

        long collisions = 0;
        for (int i = 0; i < reps; i++) {
            collisions += a.hashCode() == b.hashCode() ? 1 : 0;
        }
        ck("warmHashesStayDistinct", collisions == 0);

        // A HashSet puts the proxy through equals/hashCode from a warm site,
        // and is where a proxy served by a cached `Object` target shows up as a
        // set that grows without bound.
        Set<Tag> set = new HashSet<>();
        for (int i = 0; i < reps; i++) {
            set.add(Alpha.class.getAnnotation(Tag.class));
        }
        set.add(b);
        set.add(p);
        ck("setSize", set.size());

        // The array / `instanceof` surface the gate sits beside.
        List<String> names = new ArrayList<>();
        for (Annotation an : Alpha.class.getAnnotations()) {
            names.add(an.annotationType().getSimpleName() + ":" + (an instanceof Tag));
        }
        Collections.sort(names);
        ck("classAnnotations", names);

        System.out.println("PASS RAnnotationProxyGate (" + checks + " checks)");
    }
}
