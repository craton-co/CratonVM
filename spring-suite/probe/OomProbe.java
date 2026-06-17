public class OomProbe {
    public static void main(String[] a) {
        // (1) ArrayList(Integer.MAX_VALUE) -> new Object[2147483647] internally
        try {
            Object o = new java.util.ArrayList<>(Integer.MAX_VALUE);
            System.out.println("ArrayList: alloc OK " + o);
        } catch (Throwable t) {
            System.out.println("ArrayList: CAUGHT " + t.getClass().getName() + ": " + t.getMessage());
        }
        // (2) int[Integer.MAX_VALUE] = ~8 GiB
        try {
            int[] x = new int[Integer.MAX_VALUE];
            System.out.println("int[]: alloc OK len=" + x.length);
        } catch (Throwable t) {
            System.out.println("int[]: CAUGHT " + t.getClass().getName() + ": " + t.getMessage());
        }
        // (3) Object[Integer.MAX_VALUE]
        try {
            Object[] y = new Object[Integer.MAX_VALUE];
            System.out.println("Object[]: alloc OK len=" + y.length);
        } catch (Throwable t) {
            System.out.println("Object[]: CAUGHT " + t.getClass().getName() + ": " + t.getMessage());
        }
        System.out.println("DONE-PROBE");
    }
}
