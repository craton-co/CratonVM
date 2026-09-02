import java.util.*;
import java.util.function.*;

/** L3 — is a lambda's runtime class the FUNCTIONAL INTERFACE's name?
 *
 *  The dead-registration census (`/tmp/deadcensus.py`) calls a registration
 *  unreachable when its class is abstract-or-interface in the image and nothing
 *  in the Rust tree mints that name. It finds one row each on eighteen
 *  `java.util.function.*` interfaces and calls them dead.
 *
 *  That is only true if a lambda implementing `Function` does NOT have
 *  `java.util.function.Function` as its runtime class. On HotSpot it does not --
 *  it is a hidden class with a `$$Lambda` name. If this VM names them after the
 *  interface instead, those eighteen rows are LIVE and the census would have
 *  deleted working registrations.
 *
 *  The census's own exception is the reason to ask: CratonVM already mints
 *  `java.util.stream.Stream` and `java.util.Spliterator`, both interfaces, as
 *  concrete carriers. This asks whether the lambda machinery does the same.
 */
public class LambdaClassProbe {
    static int rows = 0;

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + tag + " |" + String.valueOf(v) + "|");
    }

    /** The interface name, or a marker -- never the raw class name, which on
     *  HotSpot carries an unstable hash and would diff against itself. */
    static String shape(Object o, Class<?> iface) {
        String n = o.getClass().getName();
        if (n.equals(iface.getName())) return "IS-THE-INTERFACE";
        if (n.contains("$$Lambda")) return "lambda-hidden-class";
        if (n.startsWith("cratonvm")) return "cratonvm-synthetic";
        return "other:" + n.replaceAll("[0-9a-fA-F]{4,}", "#").replaceAll("@.*", "");
    }

    public static void main(String[] args) {
        Function<Integer, Integer> f = x -> x + 1;
        p("Function lambda", shape(f, Function.class));
        p("Function is instance", f instanceof Function);
        p("Function applies", f.apply(1));

        Supplier<String> s = () -> "v";
        p("Supplier lambda", shape(s, Supplier.class));
        IntFunction<String> ifn = i -> "i" + i;
        p("IntFunction lambda", shape(ifn, IntFunction.class));
        ToIntFunction<String> tif = String::length;
        p("ToIntFunction methodref", shape(tif, ToIntFunction.class));
        UnaryOperator<String> uo = t -> t;
        p("UnaryOperator lambda", shape(uo, UnaryOperator.class));
        ObjIntConsumer<String> oic = (t, i) -> { };
        p("ObjIntConsumer lambda", shape(oic, ObjIntConsumer.class));
        IntBinaryOperator ibo = (x, y) -> x + y;
        p("IntBinaryOperator lambda", shape(ibo, IntBinaryOperator.class));
        LongToIntFunction lti = v -> (int) v;
        p("LongToIntFunction lambda", shape(lti, LongToIntFunction.class));

        // An ANONYMOUS CLASS implementing the same interface, which is the other
        // way a receiver could arrive.
        Function<Integer, Integer> anon = new Function<>() {
            public Integer apply(Integer x) { return x + 1; }
        };
        p("Function anonymous class", shape(anon, Function.class));

        // And a named class, the third way.
        p("Function named class", shape(new Named(), Function.class));

        // The stream carriers the census reports as MINTED, for contrast: these
        // really are named after their interface on this VM.
        p("Stream carrier", List.of("a").stream().getClass().getName()
                .equals("java.util.stream.Stream") ? "IS-THE-INTERFACE" : "not-the-interface");
        p("Spliterator carrier", new HashSet<>(List.of("a")).spliterator().getClass().getName()
                .equals("java.util.Spliterator") ? "IS-THE-INTERFACE" : "not-the-interface");

        System.out.println("DONE LambdaClassProbe");
    }

    static final class Named implements Function<Integer, Integer> {
        public Integer apply(Integer x) { return x + 1; }
    }
}
