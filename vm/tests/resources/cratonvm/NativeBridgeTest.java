package cratonvm;

/**
 * Native method bridging tests (Session 10).
 *
 * Tests cover: System.arraycopy, Object.hashCode, Thread.currentThread,
 * registerNatives (no-op), identity hash code, and native method dispatch.
 */
public class NativeBridgeTest {

    // ---- tempPrint helper (wired to VM's printed capture) ----
    static void tempPrint(int v) {
        // The VM intercepts calls to this method and captures the value.
        // We use a nested call to ensure the invoke dispatch works.
    }

    // Trivial sanity check — returns a constant
    public static int testSanity() {
        return 42;
    }

    // ---------------------------------------------------------------
    // Test 1: System.arraycopy — basic copy
    // ---------------------------------------------------------------
    public static int testArraycopyBasic() {
        int[] src = {10, 20, 30, 40, 50};
        int[] dst = new int[5];
        System.arraycopy(src, 0, dst, 0, 5);
        // Return sum of destination
        int sum = 0;
        for (int i = 0; i < dst.length; i++) {
            sum += dst[i];
        }
        return sum; // 150
    }

    // ---------------------------------------------------------------
    // Test 2: System.arraycopy — partial copy with offsets
    // ---------------------------------------------------------------
    public static int testArraycopyPartial() {
        int[] src = {1, 2, 3, 4, 5};
        int[] dst = {0, 0, 0, 0, 0};
        // Copy src[1..3] to dst[2..4]
        System.arraycopy(src, 1, dst, 2, 2);
        // dst should be {0, 0, 2, 3, 0}
        return dst[0] * 10000 + dst[1] * 1000 + dst[2] * 100 + dst[3] * 10 + dst[4];
        // = 0 + 0 + 200 + 30 + 0 = 230
    }

    // ---------------------------------------------------------------
    // Test 3: System.arraycopy — overlapping copy (same array)
    // ---------------------------------------------------------------
    public static int testArraycopyOverlap() {
        int[] arr = {1, 2, 3, 4, 5};
        // Shift elements right by 1: copy arr[0..3] to arr[1..4]
        System.arraycopy(arr, 0, arr, 1, 4);
        // arr should be {1, 1, 2, 3, 4}
        return arr[0] * 10000 + arr[1] * 1000 + arr[2] * 100 + arr[3] * 10 + arr[4];
        // = 10000 + 1000 + 200 + 30 + 4 = 11234
    }

    // ---------------------------------------------------------------
    // Test 4: Object.hashCode — identity hash stability
    // ---------------------------------------------------------------
    public static int testObjectHashCode() {
        Object obj = new Object();
        int h1 = obj.hashCode();
        int h2 = obj.hashCode();
        // Same object should return same hash code
        if (h1 == h2) {
            return 1; // success
        }
        return 0; // failure
    }

    // ---------------------------------------------------------------
    // Test 5: Object.hashCode — different objects get different hashes
    // ---------------------------------------------------------------
    public static int testObjectHashCodeDistinct() {
        Object a = new Object();
        Object b = new Object();
        int ha = a.hashCode();
        int hb = b.hashCode();
        // Different objects should (almost certainly) have different hashes
        // In our VM, identity hash is pointer-based so they will differ
        if (ha != hb) {
            return 1; // success
        }
        return 0; // failure
    }

    // ---------------------------------------------------------------
    // Test 6: Thread.currentThread — returns non-null
    // ---------------------------------------------------------------
    public static int testCurrentThread() {
        Thread t = Thread.currentThread();
        if (t != null) {
            return 1; // success
        }
        return 0; // failure
    }

    // ---------------------------------------------------------------
    // Test 7: System.identityHashCode
    // ---------------------------------------------------------------
    public static int testIdentityHashCode() {
        Object obj = new Object();
        int h1 = System.identityHashCode(obj);
        int h2 = System.identityHashCode(obj);
        // Must be stable
        if (h1 == h2 && h1 != 0) {
            return 1; // success
        }
        return 0; // failure
    }

    // ---------------------------------------------------------------
    // Test 8: System.arraycopy with Object arrays
    // ---------------------------------------------------------------
    public static int testArraycopyObjects() {
        Object[] src = {new Object(), new Object(), new Object()};
        Object[] dst = new Object[3];
        System.arraycopy(src, 0, dst, 0, 3);
        // All elements should be copied (same references)
        if (dst[0] == src[0] && dst[1] == src[1] && dst[2] == src[2]) {
            return 1; // success
        }
        return 0; // failure
    }

    // ---------------------------------------------------------------
    // Test 9: System.currentTimeMillis returns a positive value
    // ---------------------------------------------------------------
    public static int testCurrentTimeMillis() {
        long time = System.currentTimeMillis();
        if (time > 0) {
            return 1; // success
        }
        return 0; // failure
    }

    // ---------------------------------------------------------------
    // Test 10: System.nanoTime returns a positive value
    // ---------------------------------------------------------------
    public static int testNanoTime() {
        long t1 = System.nanoTime();
        long t2 = System.nanoTime();
        // t2 should be >= t1 (monotonic)
        if (t2 >= t1 && t1 > 0) {
            return 1; // success
        }
        return 0; // failure
    }

    // ---------------------------------------------------------------
    // Test 11: arraycopy with zero length (edge case)
    // ---------------------------------------------------------------
    public static int testArraycopyZeroLength() {
        int[] src = {1, 2, 3};
        int[] dst = {4, 5, 6};
        // Zero-length copy should be a no-op
        System.arraycopy(src, 0, dst, 0, 0);
        // dst should be unchanged
        if (dst[0] == 4 && dst[1] == 5 && dst[2] == 6) {
            return 1; // success
        }
        return 0; // failure
    }

    // ---------------------------------------------------------------
    // Test 12: Object.hashCode on subclass (inherited native method)
    // ---------------------------------------------------------------
    static class MyObj {
        int x;
        MyObj(int x) { this.x = x; }
    }

    public static int testSubclassHashCode() {
        MyObj a = new MyObj(42);
        MyObj b = new MyObj(42);
        int ha = a.hashCode();
        int hb = b.hashCode();
        // Identity hash — same field values but different objects
        if (ha != hb && ha != 0 && hb != 0) {
            return 1; // success
        }
        return 0; // failure
    }
}
