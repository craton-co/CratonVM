// Interpreter round i1 wave 17, lane L2 — linking the supertypes first
// (HotSpot `link_class_impl`) must not change what initialization runs.
//
// Wave 17 links (verifies and prepares) a class's superclass and every
// superinterface before the class itself, and links the class of a
// VM-raised exception. Linking runs no Java: a superinterface without a
// default method is linked but still not initialized (JVMS 5.5 step 7),
// reflection on a class does not initialize it, and the VM-raised exceptions
// keep their HotSpot messages.
//
// Run with `--compatible` and `--jdk-only`, with and without `--nojit`, and
// diff stdout against HotSpot 25, which prints:
//
//   init: Base WithDefault Impl
//   methods 1 fields 1 ctors 1
//   after reflection: []
//   1 NoDefault
//   java.lang.ArithmeticException: / by zero
//   java.lang.ClassCastException
//   Index 2 out of bounds for length 1
//   NPE
//
// (the first line ends with a space).
public class L2W17LinkOrder {
    static final StringBuilder LOG = new StringBuilder();

    static int mark(String s) {
        LOG.append(s).append(' ');
        return 1;
    }

    interface NoDefault {
        int C = mark("NoDefault");

        void run();
    }

    interface WithDefault {
        int D = mark("WithDefault");

        default int d() {
            return 3;
        }
    }

    interface Constant {
        int K = 7;
        String S = "k";
    }

    static class Base {
        static {
            mark("Base");
        }
    }

    static class Impl extends Base implements NoDefault, WithDefault, Constant {
        static {
            mark("Impl");
        }

        public void run() {}
    }

    static class Reflected {
        static {
            mark("Reflected");
        }

        int f;

        void m() {}
    }

    public static void main(String[] args) throws Exception {
        new Impl().run();
        System.out.println("init: " + LOG);
        LOG.setLength(0);

        Class<?> c =
                Class.forName("L2W17LinkOrder$Reflected", false, L2W17LinkOrder.class.getClassLoader());
        System.out.println(
                "methods " + c.getDeclaredMethods().length
                        + " fields " + c.getDeclaredFields().length
                        + " ctors " + c.getDeclaredConstructors().length);
        System.out.println("after reflection: [" + LOG + "]");

        int value = NoDefault.C;
        System.out.println(value + " " + LOG.toString().trim());

        try {
            int zero = args.length;
            System.out.println(1 / zero);
        } catch (ArithmeticException e) {
            System.out.println(e.getClass().getName() + ": " + e.getMessage());
        }
        try {
            Object o = "s";
            Integer i = (Integer) o;
            System.out.println(i);
        } catch (ClassCastException e) {
            System.out.println(e.getClass().getName());
        }
        try {
            int[] arr = new int[1];
            arr[args.length + 2] = 1;
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println(e.getMessage());
        }
        try {
            Object o = args.length == 0 ? null : "x";
            System.out.println(o.hashCode());
        } catch (NullPointerException e) {
            System.out.println("NPE");
        }
    }
}
