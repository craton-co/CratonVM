/**
 * Regression probe for native String-producing methods.  Dynamic results must
 * not be retained by the VM's literal/intern pool: this fits in a small heap
 * while producing two million distinct decimal strings.
 */
public final class StringNativeAllocationChurn {
    public static void main(String[] args) {
        String first = String.valueOf(123456789);
        String second = String.valueOf(123456789);
        if (first == second) {
            throw new AssertionError("String.valueOf(int) result was interned");
        }

        long checksum = 0;
        for (int i = 0; i < 2_000_000; i++) {
            String value = String.valueOf(i);
            checksum += value.length();
            if ((i & 0x3fff) == 0) {
                System.gc();
            }
        }
        if (checksum != 12888890L) {
            throw new AssertionError("bad checksum: " + checksum);
        }
        System.out.println("STRING_NATIVE_ALLOCATION_CHURN_OK " + checksum);
    }
}
