// Tiny fixture used by jit-cuda lowering tests: the `frem` opcode
// (0x72) and the `drem` opcode (0x73) must be rejected because PTX has
// no `rem.f32`/`rem.f64` mnemonic. Each method consists of two loads,
// the `*rem` opcode, and the matching `*return` — small enough that the
// canonical-loop recogniser classifies it as `StraightLine`.
public class FloatRemainder {
    public static float fremScalar(float a, float b) {
        return a % b;
    }

    public static double dremScalar(double a, double b) {
        return a % b;
    }
}
