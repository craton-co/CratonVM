// Straight-line static method — no backward branches, all opcodes the
// analyzer permits, all the lowerer lowers. Used by jit-cuda's
// `straight_line_method_lowers_without_loop` test as a real-bytecode
// substitute for the deleted `vec![0x03, 0xAC]` synthetic fixture.
public class EligibleStraightLine {
    public static int constReturn() {
        return 0;
    }
}
