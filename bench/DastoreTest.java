/**
 * Minimal test for dup2 + daload + dadd + dastore pattern.
 * If JIT-compiled dastore works, a[0] changes from 1.0 to 2.0.
 */
public class DastoreTest {
    static double[] arr;

    static void addOne() {
        // bytecode: getstatic arr, iconst_0, dup2, daload, dconst_1, dadd, dastore
        arr[0] += 1.0;
    }

    public static void main(String[] args) {
        arr = new double[1];
        arr[0] = 1.0;
        System.out.println("before: " + arr[0]);
        // Call enough times to trigger JIT
        for (int i = 0; i < 10000; i++) {
            addOne();
        }
        System.out.println("after 10000: " + arr[0]);
        // Expected: 10001.0
    }
}
