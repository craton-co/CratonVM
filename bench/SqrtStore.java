/**
 * Minimal test: sqrt + dastore. If sqrt dispatch corrupts state,
 * arr[0] won't change.
 */
public class SqrtStore {
    static double[] arr;

    static void step() {
        // arr[0] = Math.sqrt(arr[0]) + 1.0
        arr[0] = Math.sqrt(arr[0]) + 1.0;
    }

    public static void main(String[] args) {
        arr = new double[1];
        arr[0] = 4.0;
        System.out.println("before: " + arr[0]);
        for (int i = 0; i < 5000; i++) step();
        System.out.println("after 5K: " + arr[0]);
        for (int i = 0; i < 5000; i++) step();
        System.out.println("after 10K: " + arr[0]);
    }
}
