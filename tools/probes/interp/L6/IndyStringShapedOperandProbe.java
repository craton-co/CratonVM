/*
 * Interpreter round i1 wave 16, lane L1: an object whose first fields LOOK
 * like a String's (`byte[] value; byte coder; ...`) must be rendered by its
 * own `toString()`, not decoded as a String.
 *
 * `read_java_string_units` (vm/src/vm/vm_object.rs) recognises a String by
 * shape. `AbstractStringBuilder` declares `byte[] value; byte coder; boolean
 * maybeLatin1; int count`, which passes every one of its guards, so the
 * record `toString` indy (ObjectMethods.bootstrap) rendered a StringBuilder
 * component as its WHOLE buffer, unused capacity included (NUL characters),
 * and a user class with the same layout as its byte contents. The concat
 * indy had the same fast path for operands a pre-JDK-19 javac passes as
 * themselves. Fixed by `vm::is_java_lang_string` (a class test) in front of
 * each renderer's String fast path.
 *
 * Stdout is deterministic. HotSpot 25 prints exactly:
 *
 *   recordToString=Holder[sb=ab, o=xyz]
 *   recordToStringLength=20
 *   lookalike=Wrap[l=Lookalike!]
 *   concat=[ab]
 *   builderUnchanged=ab
 *
 * CratonVM before wave 16 printed a `recordToStringLength` well above 20 and
 * `lookalike=Wrap[l=Hi]`. Must match with and without --nojit.
 */
public class IndyStringShapedOperandProbe {
    record Holder(StringBuilder sb, Object o) {}

    /** The field layout of a JDK 9+ `java.lang.String`. */
    static final class Lookalike {
        byte[] value = {72, 105};
        byte coder = 0;
        int hash;
        boolean hashIsZero;

        @Override
        public String toString() {
            return "Lookalike!";
        }
    }

    record Wrap(Lookalike l) {}

    public static void main(String[] args) {
        StringBuilder sb = new StringBuilder("ab");
        Holder h = new Holder(sb, new StringBuilder("xyz"));
        String s = h.toString();
        System.out.println("recordToString=" + s);
        System.out.println("recordToStringLength=" + s.length());
        System.out.println("lookalike=" + new Wrap(new Lookalike()));
        Object o = sb;
        System.out.println("concat=" + ("[" + o + "]"));
        System.out.println("builderUnchanged=" + sb);
    }
}
