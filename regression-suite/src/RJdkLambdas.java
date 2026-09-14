import java.lang.invoke.CallSite;
import java.lang.invoke.LambdaMetafactory;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.Comparator;
import java.util.List;
import java.util.function.BiFunction;
import java.util.function.Function;
import java.util.function.IntBinaryOperator;
import java.util.function.Supplier;
import java.util.function.UnaryOperator;

/**
 * JDK-only corpus: lambdas and method references -- {@code invokedynamic},
 * {@code LambdaMetafactory}, bridge methods, captured values.
 *
 * {@code Function.identity()} is asserted explicitly: it is a named P0 blocker
 * ("Function.identity()" row of docs/jdk-only-runtime-services.md). CratonVM
 * carries a fabricated {@code java/util/function/Function$Identity} stand-in
 * because the real {@code Function.identity()} is itself a lambda -- so this is
 * a hole in the metafactory path. Under {@code --jdk-only} the object returned
 * here must be a real generated lambda class, not a compatibility stub.
 *
 * Determinism: identity hash codes and generated lambda class NAMES are never
 * printed (they contain an address-derived suffix). Only shape predicates are.
 */
public class RJdkLambdas {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    // ---- Function.identity(): the named P0 blocker -------------------------

    static void functionIdentity() {
        Function<String, String> id = Function.identity();
        check(id != null, "Function.identity() returned null");
        check(id.apply("abc").equals("abc"), "identity.apply must be the argument");
        String sample = "same-instance";
        check(id.apply(sample) == sample, "identity must return the SAME reference");
        check(id.apply(null) == null, "identity(null)");

        // identity() is documented to be usable in composition chains.
        Function<String, Integer> len = String::length;
        check(id.andThen(len).apply("abcd") == 4, "identity.andThen");
        check(len.compose(id).apply("abcde") == 5, "compose(identity)");
        check(id.compose(id).andThen(id).apply("x").equals("x"), "identity chain");

        // The value is a real generated lambda, so: it implements Function,
        // it is not a JDK named class, and it has a lambda-shaped class name.
        Class<?> k = id.getClass();
        check(Function.class.isAssignableFrom(k), "identity is not a Function");
        check(k.isSynthetic(), "identity implementation class must be synthetic (generated)");
        check(!k.getName().equals("java.util.function.Function$Identity"),
                "identity must not be a fabricated Function$Identity compatibility class");
        // Generated lambda classes are hidden classes since JDK 15; both the
        // hidden form and the classic form carry a '$$Lambda' marker.
        check(k.getName().contains("$$Lambda"),
                "identity class name is not lambda-shaped: " + k.getName());
        // A generic identity over a different type argument must behave the same.
        Function<List<String>, List<String>> lid = Function.identity();
        List<String> arg = new ArrayList<>();
        check(lid.apply(arg) == arg, "generic identity");
        check(UnaryOperator.identity().apply("u").equals("u"), "UnaryOperator.identity");
        System.out.println("CK RJdkLambdas identity synthetic=" + k.isSynthetic()
                + " lambdaShaped=" + k.getName().contains("$$Lambda")
                + " sameRef=" + (id.apply(sample) == sample));
    }

    // ---- capture, method references, bridges -------------------------------

    interface Adder {
        int add(int a, int b);
    }

    /** A generic SAM: javac emits a bridge method on the lambda class. */
    interface Mapper<T, R> {
        R map(T t);
    }

    /** A sub-interface narrowing the return type forces a covariant bridge. */
    interface StringMapper extends Mapper<String, String> {
        @Override
        String map(String t);
    }

    static int applyAdder(Adder a) {
        return a.add(3, 4);
    }

    static void capture() {
        int captured = 10;
        String captured2 = "cap";
        Adder plusCaptured = (a, b) -> a + b + captured;
        check(applyAdder(plusCaptured) == 17, "captured int");
        Supplier<String> s = () -> captured2 + captured;
        check(s.get().equals("cap10"), "captured String + int");

        // Capture inside a loop: each lambda must see its own copy.
        List<Supplier<Integer>> sups = new ArrayList<>();
        for (int i = 0; i < 5; i++) {
            final int v = i * i;
            sups.add(() -> v);
        }
        int sum = 0;
        for (Supplier<Integer> sup : sups) {
            sum += sup.get();
        }
        check(sum == 30, "per-iteration capture: " + sum);

        // A non-capturing lambda: the JDK is permitted to cache the instance,
        // so we assert behaviour, not identity.
        Adder pure = (a, b) -> a * b;
        check(applyAdder(pure) == 12, "non-capturing lambda");

        // `this` capture from an instance method.
        check(new RJdkLambdas().instanceCapture() == 42, "this capture");
        System.out.println("CK RJdkLambdas capture sum=" + sum);
    }

    int field = 42;

    int instanceCapture() {
        Supplier<Integer> s = () -> this.field;
        return s.get();
    }

    static void methodReferences() {
        // static ref
        Function<String, Integer> parse = Integer::parseInt;
        check(parse.apply("123") == 123, "static method ref");
        // unbound instance ref
        Function<String, Integer> len = String::length;
        check(len.apply("abcd") == 4, "unbound instance method ref");
        // bound instance ref
        String bound = "hello";
        Supplier<Integer> boundLen = bound::length;
        check(boundLen.get() == 5, "bound instance method ref");
        // constructor ref
        Supplier<ArrayList<String>> ctor = ArrayList::new;
        check(ctor.get().isEmpty(), "constructor ref");
        // `isEmpty()` is true of one memoised instance handed back forever, so
        // it cannot see a dispatcher that caches the constructed object -- a
        // shape this VM has two independent lambda dispatchers to get wrong.
        // A constructor reference must ALLOCATE on every call.
        check(ctor.get() != ctor.get(), "constructor ref must allocate a new instance per call");
        Function<Integer, ArrayList<String>> ctor1 = ArrayList::new;
        check(ctor1.apply(8).isEmpty(), "constructor ref with arg");
        ArrayList<String> sized = ctor1.apply(8);
        sized.add("only");
        // ...and the argument must reach the capacity constructor rather than
        // being dropped into an (int-element) collection constructor.
        check(sized.size() == 1 && sized.get(0).equals("only"),
                "constructor ref with arg produced " + sized);
        check(ctor1.apply(8) != ctor1.apply(8),
                "constructor ref with arg must allocate a new instance per call");
        // array constructor ref
        java.util.function.IntFunction<String[]> arr = String[]::new;
        check(arr.apply(3).length == 3, "array constructor ref");
        // primitive-specialised SAM (no boxing in the descriptor)
        IntBinaryOperator ibo = Math::max;
        check(ibo.applyAsInt(3, 9) == 9, "IntBinaryOperator method ref");
        // superclass method ref through a bi-function
        BiFunction<String, String, Boolean> eq = String::equalsIgnoreCase;
        check(eq.apply("AB", "ab"), "BiFunction method ref");
        System.out.println("CK RJdkLambdas methodrefs ok");
    }

    static void bridges() {
        // A lambda implementing a generic SAM gets an erased bridge; calling
        // through the raw/erased view must reach the same body.
        Mapper<String, String> upper = t -> t.toUpperCase(java.util.Locale.ROOT);
        check(upper.map("ab").equals("AB"), "generic SAM");
        @SuppressWarnings("rawtypes")
        Mapper raw = upper;
        check("AB".equals(raw.map("ab")), "erased bridge dispatch");

        StringMapper sm = t -> t + "!";
        Mapper<String, String> asSuper = sm;
        check(asSuper.map("x").equals("x!"), "covariant bridge dispatch");

        // javac must have emitted a bridge on the interface hierarchy.
        int bridgeCount = 0;
        for (java.lang.reflect.Method m : StringMapper.class.getMethods()) {
            if (m.isBridge()) {
                bridgeCount++;
            }
        }
        check(bridgeCount >= 1, "expected a bridge method on StringMapper, found " + bridgeCount);

        // Comparator's default methods are themselves lambda-heavy.
        List<String> l = new ArrayList<>(Arrays.asList("bb", "a", "ccc", "dd"));
        Collections.sort(l, Comparator.comparingInt(String::length).thenComparing(Function.identity()));
        check(l.equals(Arrays.asList("a", "bb", "dd", "ccc")), "comparator chain: " + l);
        System.out.println("CK RJdkLambdas bridges=" + bridgeCount + " sorted=" + l);
    }

    /** Drive LambdaMetafactory by hand, the way the JDK's indy bootstrap does. */
    static void explicitMetafactory() throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle impl = lookup.findStatic(RJdkLambdas.class, "sum2",
                MethodType.methodType(int.class, int.class, int.class));
        CallSite cs = LambdaMetafactory.metafactory(
                lookup,
                "add",
                MethodType.methodType(Adder.class),
                MethodType.methodType(int.class, int.class, int.class),
                impl,
                MethodType.methodType(int.class, int.class, int.class));
        Adder a = (Adder) cs.getTarget().invokeExact();
        check(a.add(20, 22) == 42, "explicit LambdaMetafactory call site");
        check(Adder.class.isInstance(a), "metafactory product implements the SAM");

        // altMetafactory with the SERIALIZABLE flag off but MARKERS on.
        CallSite cs2 = LambdaMetafactory.altMetafactory(
                lookup,
                "add",
                MethodType.methodType(Adder.class),
                new Object[] {
                    MethodType.methodType(int.class, int.class, int.class),
                    impl,
                    MethodType.methodType(int.class, int.class, int.class),
                    LambdaMetafactory.FLAG_MARKERS,
                    1,
                    Cloneable.class
                });
        Adder a2 = (Adder) cs2.getTarget().invokeExact();
        check(a2.add(1, 2) == 3, "altMetafactory call site");
        check(a2 instanceof Cloneable, "altMetafactory marker interface not applied");
        System.out.println("CK RJdkLambdas metafactory=" + a.add(20, 22)
                + " marker=" + (a2 instanceof Cloneable));
    }

    static int sum2(int a, int b) {
        return a + b;
    }

    public static void main(String[] args) throws Throwable {
        functionIdentity();
        capture();
        methodReferences();
        bridges();
        explicitMetafactory();
        System.out.println("CK RJdkLambdas checks=" + checks);
        System.out.println("PASS RJdkLambdas (" + checks + " checks)");
    }
}
