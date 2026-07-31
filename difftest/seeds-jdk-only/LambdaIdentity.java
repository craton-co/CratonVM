// difftest: strict
//
// JDK-only boundary vector: lambdas / invokedynamic
// (docs/feature-designs/jdk-only-mode.md §1.6 — "lambdas ... are allowed and
// carry their own distinct origin ... they are not compatibility stubs").
//
// The `Function.identity()` path is the named case: CratonVM has historically
// stood a fabricated `Function$Identity` class in for it, which is exactly the
// `ClassOrigin::CompatibilityStub` that `--jdk-only` must refuse. Under
// `--jdk-only` the correct outcome is a *real* LambdaMetafactory-spun hidden
// class with origin `generated-lambda` and identical observable behaviour.
//
// Everything printed is a boolean or a stable name — never a generated class
// name or an identity hash — so the transcript is deterministic. Generated-class
// assignability and loader identity are printed explicitly, because the
// harness's oracle can only compare what the program puts on stdout.
import java.util.function.Function;
import java.util.function.IntBinaryOperator;
import java.util.function.Supplier;

public class LambdaIdentity {

    /** A user-declared SAM, so the vector covers a non-JDK functional interface too. */
    interface Doubler {
        int twice(int x);
    }

    public static void main(String[] args) {
        // --- Function.identity(): the named compatibility-stub magnet -------
        Function<String, String> id = Function.identity();
        System.out.println("identity-apply: " + id.apply("abc"));
        System.out.println("identity-null: " + id.apply(null));
        // Non-capturing lambdas come from a ConstantCallSite, so the JDK hands
        // back the same instance on every call to identity().
        System.out.println("identity-cached: " + (Function.identity() == Function.identity()));
        System.out.println("identity-compose: " + id.andThen(id).apply("xyz"));

        Class<?> idClass = id.getClass();
        System.out.println("identity-assignable: " + Function.class.isAssignableFrom(idClass));
        System.out.println("identity-instanceof: " + (id instanceof Function));
        System.out.println("identity-super: " + idClass.getSuperclass().getName());
        System.out.println("identity-ifaces: " + idClass.getInterfaces().length);
        System.out.println("identity-iface0: " + idClass.getInterfaces()[0].getName());
        System.out.println("identity-hidden: " + idClass.isHidden());
        System.out.println("identity-synthetic: " + idClass.isSynthetic());
        System.out.println("identity-is-array: " + idClass.isArray());
        System.out.println("identity-is-iface: " + idClass.isInterface());
        // java.base's identity lambda is defined to the boot loader.
        System.out.println("identity-loader: " + String.valueOf(idClass.getClassLoader()));

        // --- a lambda defined in THIS class: app-loader identity ------------
        ClassLoader app = LambdaIdentity.class.getClassLoader();
        Doubler d = x -> x + x;
        Class<?> dClass = d.getClass();
        System.out.println("doubler-apply: " + d.twice(21));
        System.out.println("doubler-assignable: " + Doubler.class.isAssignableFrom(dClass));
        System.out.println("doubler-iface0: " + dClass.getInterfaces()[0].getName());
        System.out.println("doubler-hidden: " + dClass.isHidden());
        System.out.println("doubler-loader-is-app: " + (dClass.getClassLoader() == app));
        System.out.println("doubler-loader-class: " + app.getClass().getName());
        System.out.println("doubler-name-has-lambda: " + dClass.getName().contains("$$Lambda"));
        // A lambda proxy is NOT assignable to an unrelated SAM.
        System.out.println("doubler-not-a-Function: " + Function.class.isAssignableFrom(dClass));

        // --- capturing lambda, method reference, and a bound receiver -------
        int captured = 10;
        IntBinaryOperator add = (a, b) -> a + b + captured;
        System.out.println("capturing: " + add.applyAsInt(1, 2));
        // Each evaluation of a *capturing* lambda expression yields a fresh
        // instance, but the same generated class.
        IntBinaryOperator add2 = (a, b) -> a + b + captured;
        System.out.println("capturing-distinct-instance: " + (add != add2));
        System.out.println("capturing-2: " + add2.applyAsInt(3, 4));

        Function<Object, String> toStr = String::valueOf;
        System.out.println("methodref: " + toStr.apply(42));
        Supplier<String> bound = "bound"::toUpperCase;
        System.out.println("boundref: " + bound.get());

        // --- the SAM must still typecheck at runtime (checkcast on the SAM) -
        Object erased = d;
        Doubler back = (Doubler) erased;
        System.out.println("sam-checkcast: " + back.twice(5));
        try {
            Function<?, ?> wrong = (Function<?, ?>) erased;
            System.out.println("no-cce: " + wrong);
        } catch (ClassCastException e) {
            System.out.println("CCE-on-wrong-sam: true");
        }

        System.out.println("done");
    }
}
