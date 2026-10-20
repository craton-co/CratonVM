// Interpreter round i1 wave 15, lane L1 — a superclass's <clinit> that stores
// into its subclass's statics (JVMS §5.5: the subclass's initialization is in
// progress on this thread, so the `putstatic` is a recursive request and
// completes normally, and the value must survive).
//
// Until wave 15 CratonVM prepared a class (allocated its statics block) only
// AFTER its superclass's <clinit> had run, so these stores landed in a block
// that preparation then replaced: `int`, `long`, `ref` and `x` printed 0 /
// null (the last line matched: Sub2's own <clinit> stores `z`). HotSpot prepares
// a class while linking it, before any initialization, and keeps them.
//
// Run with `--compatible`, with and without `--nojit`, and diff stdout against
// HotSpot 25, which prints:
//
//   int 42
//   long 7000000000
//   ref set by Super
//   withClinit x=42 y=7
//   overwritten 5
public class L1Wave15RecursiveInitStore {
    static class Super {
        static {
            Sub.i = 42;
            Sub.l = 7_000_000_000L;
            Sub.s = "set by Super";
        }
    }

    static class Sub extends Super {
        static int i;
        static long l;
        static String s;
    }

    static class Super2 {
        static {
            Sub2.x = 42;
            Sub2.z = 99;
        }
    }

    // `y` and `z` are assigned by Sub2's own <clinit>, which runs after
    // Super2's: `x` keeps Super2's store, `z` is overwritten.
    static class Sub2 extends Super2 {
        static int x;
        static int y = 7;
        static int z = 5;
    }

    public static void main(String[] args) {
        System.out.println("int " + Sub.i);
        System.out.println("long " + Sub.l);
        System.out.println("ref " + Sub.s);
        System.out.println("withClinit x=" + Sub2.x + " y=" + Sub2.y);
        System.out.println("overwritten " + Sub2.z);
    }
}
