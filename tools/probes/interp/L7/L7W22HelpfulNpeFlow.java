/*
 * Interpreter round i1, wave 22, lane L7: JEP 358 helpful NullPointerException
 * messages whose "because ..." clause depends on the operand-stack analysis
 * reaching the trapping bytecode (HotSpot's `ExceptionMessageBuilder`, a
 * whole-method dataflow over every bytecode).
 *
 * Compile WITHOUT -g (plain `javac`, which emits no LocalVariableTable), so
 * locals print as <localN> / <parameterN>; run on CratonVM with `--nojit` and
 * without, both modes (helpful messages are on by default on JDK 25):
 *
 *   iastore   an array store earlier in the same basic block
 *   concat    a string concatenation (invokedynamic) earlier in the block
 *   dupx1     a postfix increment of a field used as a value (dup_x1)
 *   dup2      a postfix increment of a long static used as a value (dup2)
 *   pop2      a discarded long result (pop2)
 *   sync      the dereference inside a synchronized block (monitorenter)
 *   merge     the receiver pushed BEFORE a conditional argument, so the
 *             invoke sits after a control-flow merge whose predecessors
 *             agree on where the receiver came from
 *   param     a parameter dereferenced before the method's only store to it
 *             (HotSpot names it <parameterN> until a write reaches the site)
 *   param2    the same parameter after the store (now <localN>)
 *   ternary   the null value itself chosen by a conditional (no clause)
 *   multi     a multianewarray earlier in the block
 *   array     an array chosen by a conditional, then indexed (HotSpot names
 *             the undescribable array <array> and keeps the rest)
 *   index     an int array element used as an index (iaload is described)
 *   caught    a parameter reassigned in a try, dereferenced in its catch (a
 *             handler starts with no written locals)
 *   loop      a parameter dereferenced in a loop before the loop's store
 *   plain     control: nothing unusual before the dereference
 *
 * HotSpot 25 prints exactly:
 *
 *   iastore: Cannot invoke "String.length()" because "<local1>" is null
 *   concat: Cannot invoke "String.length()" because "<local1>" is null
 *   dupx1: Cannot read field "next" because "<local2>" is null
 *   dup2: Cannot invoke "String.length()" because "<local2>" is null
 *   pop2: Cannot invoke "String.length()" because "<local0>" is null
 *   sync: Cannot invoke "String.length()" because "<local1>" is null
 *   merge: Cannot invoke "L7W22HelpfulNpeFlow$Node.take(int)" because "<parameter1>" is null
 *   param: Cannot invoke "String.trim()" because "<parameter1>" is null
 *   param2: Cannot invoke "String.length()" because "<local0>" is null
 *   ternary: Cannot invoke "String.length()"
 *   multi: Cannot invoke "String.length()" because "<local0>" is null
 *   plain: Cannot invoke "String.length()" because "<local0>" is null
 *   array: Cannot read field "count" because "<array>[0]" is null
 *   index: Cannot read field "count" because "<parameter1>[<parameter2>[0]]" is null
 *   caught: Cannot invoke "String.length()" because "<parameter1>" is null
 *   loop: Cannot invoke "String.length()" because "<parameter1>" is null
 *
 * CratonVM before wave 22 (read from `helpful_npe::simulate_to`, which walked
 * only the trapping bci's basic block and gave up on the first opcode it did
 * not model, and `local_slot_written`, which asked whether ANY store to the
 * slot exists anywhere in the method): every row but `param`, `ternary`,
 * `index`, `caught`, `loop` and `plain` lost its `because` clause;
 * `param`, `caught` and `loop` said <local0>; `index` said
 * `<parameter1>[...]` (an `iaload` index was not described).
 */
public class L7W22HelpfulNpeFlow {
    static final class Node {
        Node next;
        int count;
        Node[] kids = new Node[1];

        int take(int v) {
            return v;
        }
    }

    static long counter;
    static boolean flip = true;

    static String none() {
        return null;
    }

    static long longValue() {
        return 7L;
    }

    static void iastore() {
        int[] a = new int[2];
        a[0] = 1;
        String s = none();
        s.length();
    }

    static void concat(int n) {
        String s = none();
        String t = "n=" + n;
        s.length();
        t.length();
    }

    static void dupx1(Node n) {
        int before = n.count++;
        Node m = n.next;
        int after = m.next.count + before;
    }

    static void dup2() {
        long was = counter++;
        String s = none();
        s.length();
    }

    static void pop2() {
        String s = none();
        longValue();
        s.length();
    }

    static void sync(Object lock) {
        String s = none();
        synchronized (lock) {
            s.length();
        }
    }

    static void merge(Node n, boolean b) {
        n.take(b ? 1 : 2);
    }

    static String param(String s) {
        s = s.trim();
        return s;
    }

    static int param2(String s) {
        s = s + "";
        s = none();
        return s.length();
    }

    static void ternary(boolean b) {
        String a = "x";
        (b ? none() : a).length();
    }

    static void multi() {
        String s = none();
        int[][] grid = new int[2][3];
        s.length();
    }

    static void plain() {
        String s = none();
        s.length();
    }

    static int array(Node n) {
        return (flip ? n.kids : n.kids)[0].count;
    }

    static int index(Node[] k, int[] idx) {
        return k[idx[0]].count;
    }

    static int caught(String s) {
        try {
            s = s.trim();
        } catch (RuntimeException e) {
            return s.length();
        }
        return 0;
    }

    static void loop(String s) {
        for (int i = 0; i < 2; i++) {
            s.length();
            s = "x";
        }
    }

    interface Row {
        void run() throws Exception;
    }

    static void row(String label, Row r) {
        try {
            r.run();
            System.out.println(label + ": no exception");
        } catch (NullPointerException e) {
            System.out.println(label + ": " + e.getMessage());
        } catch (Exception e) {
            System.out.println(label + ": " + e);
        }
    }

    public static void main(String[] args) {
        Node dangling = new Node();
        row("iastore", L7W22HelpfulNpeFlow::iastore);
        row("concat", () -> concat(3));
        row("dupx1", () -> dupx1(dangling));
        row("dup2", L7W22HelpfulNpeFlow::dup2);
        row("pop2", L7W22HelpfulNpeFlow::pop2);
        row("sync", () -> sync(new Object()));
        row("merge", () -> merge(null, flip));
        row("param", () -> param(null));
        row("param2", () -> param2("y"));
        row("ternary", () -> ternary(flip));
        row("multi", L7W22HelpfulNpeFlow::multi);
        row("plain", L7W22HelpfulNpeFlow::plain);
        row("array", () -> array(dangling));
        row("index", () -> index(new Node[2], new int[] {1}));
        row("caught", () -> caught(null));
        row("loop", () -> loop(null));
    }
}
