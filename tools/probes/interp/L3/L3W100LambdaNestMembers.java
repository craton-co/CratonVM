import java.util.*;
import java.util.function.*;

/**
 * Interpreter round i5 wave 100, lane L3: a lambda's class is a hidden
 * NESTMATE of the class that defines the lambda. HotSpot's
 * JVM_GetNestMembers answers that host's nest, host first: [host, members...]
 * (the hidden class itself is never listed). CratonVM answered [lambda class]
 * for its lambda proxies while getNestHost() already named the host.
 *
 * Run: java -cp . L3W100LambdaNestMembers
 *
 * Expected HotSpot 25:
 *   runnable nest host: L3W100LambdaNestMembers
 *   runnable nest members: [L3W100LambdaNestMembers, L3W100LambdaNestMembers$Inner]
 *   runnable nestmate of host: true
 *   runnable nestmate of Inner: true
 *   capturing nest members: [L3W100LambdaNestMembers, L3W100LambdaNestMembers$Inner]
 *   inner-defined nest host: L3W100LambdaNestMembers
 *   inner-defined nest members: [L3W100LambdaNestMembers, L3W100LambdaNestMembers$Inner]
 *   method-ref nest members: [L3W100LambdaNestMembers, L3W100LambdaNestMembers$Inner]
 *   host nest members: [L3W100LambdaNestMembers, L3W100LambdaNestMembers$Inner]
 */
public class L3W100LambdaNestMembers {
    static class Inner {
        static Supplier<String> make() { return () -> "inner"; }
    }

    static List<String> names(Class<?>[] cs) {
        List<String> out = new ArrayList<>();
        for (Class<?> c : cs) out.add(c.getName());
        // The host is first on HotSpot; the members after it are in
        // NestMembers attribute order, which javac writes sorted. Keep the
        // first, sort the rest so the row does not depend on javac.
        List<String> rest = new ArrayList<>(out.subList(1, out.size()));
        Collections.sort(rest);
        List<String> r = new ArrayList<>();
        r.add(out.get(0));
        r.addAll(rest);
        return r;
    }

    public static void main(String[] args) {
        Runnable r = () -> {};
        Class<?> rc = r.getClass();
        System.out.println("runnable nest host: " + rc.getNestHost().getName());
        System.out.println("runnable nest members: " + names(rc.getNestMembers()));
        System.out.println("runnable nestmate of host: " + rc.isNestmateOf(L3W100LambdaNestMembers.class));
        System.out.println("runnable nestmate of Inner: " + rc.isNestmateOf(Inner.class));
        int k = args.length;
        IntSupplier cap = () -> k + 1;
        System.out.println("capturing nest members: " + names(cap.getClass().getNestMembers()));
        Class<?> ic = Inner.make().getClass();
        System.out.println("inner-defined nest host: " + ic.getNestHost().getName());
        System.out.println("inner-defined nest members: " + names(ic.getNestMembers()));
        Function<Object, String> mr = String::valueOf;
        System.out.println("method-ref nest members: " + names(mr.getClass().getNestMembers()));
        System.out.println("host nest members: " + names(L3W100LambdaNestMembers.class.getNestMembers()));
    }
}
