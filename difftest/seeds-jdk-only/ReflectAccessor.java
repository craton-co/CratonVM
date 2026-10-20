// difftest: strict
//
// JDK-only boundary vector: reflection accessors
// (docs/feature-designs/jdk-only-mode.md §1.6 — a reflection accessor is an
// *allowed* generated class with its own origin, not a compatibility stub).
//
// Under `--jdk-only` the accessor a `Method`/`Field`/`Constructor` spins up must
// come from real JDK machinery, and every native it needs must resolve to a
// reviewed `NativeKind::Bridge` or `Intrinsic` — an absent one is a structured
// `MissingNative`, never a silent stub returning a plausible-looking value. The
// loop below runs the same accessor well past HotSpot's inflation threshold so
// both the first (direct) and the spun-up accessor paths are exercised.
//
// Everything printed is a value or a stable name; no identity hashes.
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;

public class ReflectAccessor {

    static class Box {
        private int value;
        private static String tag = "T";

        Box(int v) {
            this.value = v;
        }

        private int twice() {
            return value * 2;
        }

        static String describe(String prefix, int n) {
            return prefix + ":" + n;
        }

        @Override
        public String toString() {
            return "Box(" + value + ")";
        }
    }

    private static void boom() {
        throw new IllegalStateException("boom");
    }

    public static void main(String[] args) throws Exception {
        // --- Constructor accessor ------------------------------------------
        Constructor<Box> ctor = Box.class.getDeclaredConstructor(int.class);
        ctor.setAccessible(true);
        Box b = ctor.newInstance(21);
        System.out.println("ctor: " + b);
        System.out.println("ctor-params: " + ctor.getParameterCount());
        System.out.println("ctor-declaring: " + ctor.getDeclaringClass().getName());

        // --- Method accessor ------------------------------------------------
        Method twice = Box.class.getDeclaredMethod("twice");
        twice.setAccessible(true);
        System.out.println("invoke: " + twice.invoke(b));
        System.out.println("method-declaring: " + twice.getDeclaringClass().getName());
        System.out.println("method-modifiers: " + Modifier.toString(twice.getModifiers()));
        System.out.println("method-return: " + twice.getReturnType().getName());
        System.out.println("method-params: " + twice.getParameterCount());

        Method describe = Box.class.getDeclaredMethod("describe", String.class, int.class);
        describe.setAccessible(true);
        System.out.println("static-invoke: " + describe.invoke(null, "n", 3));
        System.out.println("static-modifiers: " + Modifier.toString(describe.getModifiers()));

        // --- Field accessor ---------------------------------------------------
        Field value = Box.class.getDeclaredField("value");
        value.setAccessible(true);
        System.out.println("field-get: " + value.getInt(b));
        System.out.println("field-type: " + value.getType().getName());
        value.setInt(b, 5);
        System.out.println("field-set: " + b);
        System.out.println("field-boxed-get: " + value.get(b));

        Field tag = Box.class.getDeclaredField("tag");
        tag.setAccessible(true);
        System.out.println("static-field: " + tag.get(null));
        tag.set(null, "U");
        System.out.println("static-field-set: " + tag.get(null));

        // --- past the inflation threshold: the *spun* accessor path ---------
        long sum = 0;
        for (int i = 0; i < 64; i++) {
            sum += (Integer) twice.invoke(b);
        }
        System.out.println("hot-sum: " + sum);

        long fieldSum = 0;
        for (int i = 0; i < 64; i++) {
            fieldSum += value.getInt(b);
        }
        System.out.println("hot-field-sum: " + fieldSum);

        // --- error surface: identity of the thrown/unwrapped exceptions -----
        try {
            twice.invoke(null);
            System.out.println("no-NPE");
        } catch (NullPointerException e) {
            System.out.println("NPE-on-null-receiver: true");
        }
        try {
            twice.invoke("wrong receiver type");
            System.out.println("no-IAE");
        } catch (IllegalArgumentException e) {
            System.out.println("IAE-on-wrong-receiver: true");
        }
        try {
            describe.invoke(null, "n");
            System.out.println("no-IAE-2");
        } catch (IllegalArgumentException e) {
            System.out.println("IAE-on-arg-count: true");
        }
        Method bad = ReflectAccessor.class.getDeclaredMethod("boom");
        bad.setAccessible(true);
        try {
            bad.invoke(null);
            System.out.println("no-ITE");
        } catch (InvocationTargetException e) {
            System.out.println("ITE-cause: " + e.getCause().getClass().getName()
                    + ": " + e.getCause().getMessage());
        }
        try {
            Box.class.getDeclaredMethod("nosuch");
            System.out.println("no-NSME");
        } catch (NoSuchMethodException e) {
            System.out.println("NSME: true");
        }
        try {
            Box.class.getDeclaredField("nosuch");
            System.out.println("no-NSFE");
        } catch (NoSuchFieldException e) {
            System.out.println("NSFE: true");
        }

        // --- accessor-adjacent Class metadata -------------------------------
        System.out.println("box-loader-is-app: "
                + (Box.class.getClassLoader() == ReflectAccessor.class.getClassLoader()));
        System.out.println("box-name: " + Box.class.getName());
        System.out.println("box-simple: " + Box.class.getSimpleName());
        System.out.println("box-is-member: " + Box.class.isMemberClass());
        System.out.println("box-enclosing: " + Box.class.getEnclosingClass().getName());

        System.out.println("done");
    }
}
