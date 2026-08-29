import java.math.BigInteger;
import java.util.ArrayList;
import java.util.List;

/** Does this VM produce HELPFUL NullPointerException messages at all?
 *
 *  `BigIntegerSweep` turned up four rows where HotSpot names the field and the
 *  variable — `Cannot read field "signum" because "val" is null` — and CratonVM
 *  answers a fixed `null object argument`. That could be four BigInteger rows
 *  or it could be a VM-wide capability, and the difference decides whether it
 *  is a fix or a scoped known issue. So: ask every SHAPE of null dereference
 *  the JDK's `NullPointerException::extendedMessage` knows how to describe,
 *  including ones no native is involved in at all.
 *
 *  JEP 358 is on by default from JDK 15 (`ShowCodeDetailsInExceptionMessages`),
 *  and the messages are computed from the bytecode at the throwing BCI plus the
 *  local-variable table, so they are a property of the VM raising the throw and
 *  not of the library.
 */
public class HelpfulNpeProbe {

    static int rows = 0;

    interface F {
        Object get() throws Throwable;
    }

    static void p(String tag, F f) {
        rows++;
        String v;
        try {
            v = String.valueOf(f.get());
        } catch (Throwable e) {
            v = "THREW " + e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println(tag + " |" + v + "|");
    }

    static class Node {
        Node next;
        int value;
        int[] arr;
        String name;
    }

    static String nullString() {
        return null;
    }

    static int[] nullArray() {
        return null;
    }

    public static void main(String[] args) {
        // 1. invoke on a null local
        p("invoke on a null local", () -> {
            String s = null;
            return s.length();
        });
        // 2. invoke on a null method return
        p("invoke on a null return", () -> nullString().length());
        // 3. read a field of a null
        p("read a field of a null", () -> {
            Node n = null;
            return n.value;
        });
        // 4. write a field of a null
        p("write a field of a null", () -> {
            Node n = null;
            n.value = 1;
            return "no throw";
        });
        // 5. a chained field read
        p("chained field read", () -> {
            Node n = new Node();
            return n.next.value;
        });
        // 6. array length of a null
        p("array length of a null", () -> {
            int[] a = null;
            return a.length;
        });
        // 7. array load from a null
        p("array load from a null", () -> {
            int[] a = null;
            return a[0];
        });
        // 8. array store into a null
        p("array store into a null", () -> {
            int[] a = null;
            a[0] = 1;
            return "no throw";
        });
        // 9. array load from a null field
        p("array load from a null field", () -> {
            Node n = new Node();
            return n.arr[0];
        });
        // 10. unboxing a null
        p("unboxing a null", () -> {
            Integer i = null;
            return i + 1;
        });
        // 11. a null argument to a JDK method
        p("null argument to String.concat", () -> "a".concat(null));
        // 12. throwing a null
        p("athrow of a null", () -> {
            RuntimeException e = null;
            throw e;
        });
        // 13. monitorenter on a null
        p("synchronized on a null", () -> {
            Object o = null;
            synchronized (o) {
                return "no throw";
            }
        });
        // 14. a null receiver in an interface call
        p("interface call on a null", () -> {
            List<String> l = null;
            return l.size();
        });
        // 15. a null element out of a collection
        p("invoke on a null collection element", () -> {
            List<String> l = new ArrayList<>();
            l.add(null);
            return l.get(0).length();
        });
        // 16. the BigInteger rows this probe came from
        p("BigInteger.add(null)", () -> BigInteger.ONE.add(null).toString());
        p("BigInteger.and(null)", () -> BigInteger.ONE.and(null).toString());
        // 17. an explicit NPE keeps its own message
        p("explicit NPE with a message", () -> {
            throw new NullPointerException("mine");
        });
        p("explicit NPE without a message", () -> {
            throw new NullPointerException();
        });
        p("Objects.requireNonNull", () -> java.util.Objects.requireNonNull(null, "because"));
        p("Objects.requireNonNull no message", () -> java.util.Objects.requireNonNull(null));
        System.out.println("rows " + rows);
        System.out.println("DONE HelpfulNpeProbe");
    }
}
