// difftest: strict
//
// invokedynamic family (design §4.1): every `+` on a non-constant String in
// modern javac lowers to an `invokedynamic` against
// java.lang.invoke.StringConcatFactory. Mixing primitive widths, null, and a
// boxed value through the indy bootstrap is a dense source of divergence.
// Output is deterministic.
public class StringConcatIndy {
    public static void main(String[] args) {
        int i = 42;
        long l = 9_000_000_000L;
        double d = 3.5;
        boolean b = true;
        char c = 'Z';
        String s = null;
        Object o = Integer.valueOf(7);

        // One big concat exercising every primitive recipe slot + null + box.
        System.out.println("mix: " + i + "/" + l + "/" + d + "/" + b + "/" + c + "/" + s + "/" + o);

        // Concat in a loop (each iteration a fresh indy call site result).
        StringBuilder expected = new StringBuilder();
        for (int k = 0; k < 5; k++) {
            String line = "k=" + k + " sq=" + (k * k);
            expected.append(line).append('|');
        }
        System.out.println(expected.toString());

        // Float vs double formatting through concat.
        float f = 1.0f / 3.0f;
        System.out.println("f=" + f + " d=" + (1.0 / 3.0));
    }
}
