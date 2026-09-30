// difftest: strict
//
// Exception identity & ordering (design §4.5). Catches a handful of common
// throwables and prints their exact class name + message, plus the JEP-358
// "helpful NPE" message for a null array/field/invoke deref — the channel that
// must be byte-identical to HotSpot. Everything is caught, so the program
// exits 0 with a deterministic transcript on both VMs.
public class ExceptionId {
    static int[] arr = null;

    public static void main(String[] args) {
        try {
            int x = 1 / 0;
            System.out.println(x);
        } catch (ArithmeticException e) {
            System.out.println("AE: " + e.getMessage());
        }

        try {
            Object o = "hello";
            Integer i = (Integer) o;
            System.out.println(i);
        } catch (ClassCastException e) {
            System.out.println("CCE: " + e.getMessage());
        }

        try {
            int v = arr[3];
            System.out.println(v);
        } catch (NullPointerException e) {
            // JEP-358 helpful message: "Cannot load from int array because ..."
            System.out.println("NPE: " + e.getMessage());
        }

        try {
            String s = "abc";
            char c = s.charAt(9);
            System.out.println(c);
        } catch (StringIndexOutOfBoundsException e) {
            System.out.println("SIOOBE: " + e.getMessage());
        }

        System.out.println("done");
    }
}
