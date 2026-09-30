// Exercises the `i2c` bytecode (0x92, int -> char) in the GPU lowerer.
//
// `char` is an UNSIGNED 16-bit type (JVMS §6.5 `i2c`: "zero extend").
// For an input with bit 15 set — e.g. 0xFFFF — the JVM yields 65535,
// not -1. The lowered PTX must zero-extend (mask to 0xFFFF), NOT
// sign-extend like `i2b`/`i2s`.
//
// The `(char) a[i]` cast emits `i2c`; the result (an int on the JVM
// stack) is stored back into an `int[]`, so no char-array support is
// needed in the analyzer.
public class EligibleI2cConvert {
    public static void toChar(int[] a, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = (char) a[i];
        }
    }
}
