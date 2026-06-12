// BUG-H reproducer: JIT->JIT call bypasses callee's local catch for an
// implicit AIOOBE. `get` mirrors Tomcat HexUtils.getDec.
public class BugH {
    static final int[] DEC = new int[128];
    static {
        for (int i = 0; i < DEC.length; i++) DEC[i] = -1;
        for (int i = '0'; i <= '9'; i++) DEC[i] = i - '0';
    }

    // shape of HexUtils.getDec: T[index - '0'] inside catch(AIOOBE) -> -1
    static int get(int index) {
        try {
            return DEC[index];
        } catch (ArrayIndexOutOfBoundsException ex) {
            return -1;
        }
    }

    // caller that itself gets JIT-compiled, so get() is invoked JIT->JIT
    static int caller(int index) {
        return get(index);
    }

    public static void main(String[] args) {
        long warm = 0;
        // warm both caller() and get() hot so BOTH JIT-compile
        for (int i = 0; i < 400000; i++) {
            warm += caller('5');          // in-bounds -> 5
            warm += caller(0);            // DEC[0] -> -1 (valid index, value -1)
        }
        // now the failing case: index that goes out of bounds
        int r1 = caller(-48);             // DEC[-48] -> AIOOBE -> must be -1
        int r2 = caller(200);             // DEC[200] -> AIOOBE -> must be -1
        int r3 = caller(Integer.MIN_VALUE);
        System.out.println("warm=" + warm);
        System.out.println("get(-48)=" + r1 + " (expect -1)");
        System.out.println("get(200)=" + r2 + " (expect -1)");
        System.out.println("get(MIN)=" + r3 + " (expect -1)");
        boolean ok = (r1 == -1) && (r2 == -1) && (r3 == -1);
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) System.exit(1);
    }
}
