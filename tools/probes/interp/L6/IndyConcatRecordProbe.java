/*
 * Interpreter round i1, lane L6: how `invokedynamic` string concatenation
 * (StringConcatFactory) renders its operands, and how record `toString`
 * (ObjectMethods) renders a record. Stdout is deterministic; compare it with
 * HotSpot 25 line by line.
 *
 * HotSpot 25 prints exactly:
 *
 *   counter=C5
 *   holder=H[true]
 *   nullToString=xnully
 *   throwing=caught IllegalStateException boom
 *   charLen=3 mid=d800
 *   boxedCharLen=1 unit=dc00
 *   wrappers=5 true A 1.0 7 2.5
 *   record=P[x=1.0, f=2.5, c=Z, s=hi, o=null, big=1.0E10]
 *   local=Local[a=1]
 *   recordIdentity=false
 *   recordSurrogate=8 d800
 *   toStringSurrogate=3 d800
 *   hot=v=41999 v=null caught IllegalStateException
 *
 * (`toStringSurrogate` added in wave 2: before it, a `toString()` result was
 * read through a Rust `String`, and CratonVM printed `toStringSurrogate=3 fffd`.)
 *
 * Before the i1-L6 fixes CratonVM printed, respectively: `counter=5` (any
 * object with a single primitive field was rendered from that field, and its
 * toString() was never called); `holder=true` (the class name contains
 * "Boolean"); `nullToString=x...NullString@<hash>y`; `throwing=NOT THROWN ...`
 * (the exception was swallowed); `charLen=8` (a lone-surrogate char became the
 * six characters `\ud800`); `record=P[x=1, f=2.5, ..., big=10000000000]`
 * (Rust `{}` float formatting); `local=1Local[a=1]`; `recordIdentity=true`
 * (record toString results were interned). The `hot=` line runs the same
 * concat through a method hot enough to be compiled, so it exercises the JIT
 * concat bridge; its throwing case was flattened into a null String.
 */
public class IndyConcatRecordProbe {
    static final class Counter {
        final int n;

        Counter(int n) {
            this.n = n;
        }

        @Override
        public String toString() {
            return "C" + n;
        }
    }

    static final class BooleanHolder {
        final boolean b;

        BooleanHolder(boolean b) {
            this.b = b;
        }

        @Override
        public String toString() {
            return "H[" + b + "]";
        }
    }

    static final class NullString {
        @Override
        public String toString() {
            return null;
        }
    }

    static final class Thrower {
        @Override
        public String toString() {
            throw new IllegalStateException("boom");
        }
    }

    static final class LoneSurrogate {
        @Override
        public String toString() {
            return "x\uD800y";
        }
    }

    record P(double x, float f, char c, String s, Object o, double big) {}

    record S(String s) {}

    static String cat(Object o) {
        return "v=" + o;
    }

    public static void main(String[] args) {
        System.out.println("counter=" + new Counter(5));
        System.out.println("holder=" + new BooleanHolder(true));
        System.out.println("nullToString=x" + new NullString() + "y");
        try {
            String s = "t" + new Thrower();
            System.out.println("throwing=NOT THROWN " + s);
        } catch (IllegalStateException e) {
            System.out.println(
                    "throwing=caught " + e.getClass().getSimpleName() + " " + e.getMessage());
        }

        char hi = (char) 0xD800;
        String sc = "a" + hi + "b";
        System.out.println("charLen=" + sc.length() + " mid=" + Integer.toHexString(sc.charAt(1)));

        Character lo = Character.valueOf((char) 0xDC00);
        String bc = "" + lo;
        System.out.println(
                "boxedCharLen=" + bc.length() + " unit=" + Integer.toHexString(bc.charAt(0)));

        Integer i5 = 5;
        Boolean bt = true;
        Character ca = 'A';
        Double d1 = 1.0;
        Long l7 = 7L;
        Float f25 = 2.5f;
        System.out.println("wrappers=" + i5 + " " + bt + " " + ca + " " + d1 + " " + l7 + " " + f25);

        P p = new P(1.0, 2.5f, 'Z', "hi", null, 1e10);
        System.out.println("record=" + p);

        record Local(int a) {}
        System.out.println("local=" + new Local(1));

        System.out.println("recordIdentity=" + (p.toString() == p.toString()));

        String rs = new S("x\uD800y").toString();
        System.out.println(
                "recordSurrogate=" + rs.length() + " " + Integer.toHexString(rs.charAt(5)));

        String ts = "" + new LoneSurrogate();
        System.out.println(
                "toStringSurrogate=" + ts.length() + " " + Integer.toHexString(ts.charAt(1)));

        String last = null;
        for (int k = 0; k < 42_000; k++) {
            last = cat(k);
        }
        String n = cat(new NullString());
        String t;
        try {
            t = cat(new Thrower());
        } catch (IllegalStateException e) {
            t = "caught " + e.getClass().getSimpleName();
        }
        System.out.println("hot=" + last + " " + n + " " + t);
    }
}
