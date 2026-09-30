// Interpreter round i1, wave 13, lane L2 — a direct-call owner the JIT
// SUBSTITUTED for the constant-pool class is bound by the id the substitution
// found, not access-checked against the caller as if the constant pool named
// it (docs/internal/fixed-bugs/interpreter-L2-substituted-direct-call-owner-is-access-checked-against-the-caller-FIXED-20260925.md).
//
// `StringBuilder` is `final`, so `sb.length()`, `sb.charAt(int)` and
// `sb.setLength(int)` are statically bound `invokevirtual` sites whose body is
// declared by the package-private `java.lang.AbstractStringBuilder`. A
// class-path caller may not access that class, but it never names it: JVMS
// §5.4.4 checks `StringBuilder`, the class the constant pool names.
//
// Compare stdout against HotSpot 25 (`java L2SubstitutedOwner`). Run CratonVM
// with `--compatible`, with and without `--nojit`. Expected output, all modes:
//   round 0 lengths=20199999 grow=999990
//   round 1 lengths=20199999 grow=999990
//   round 2 lengths=20199999 grow=999990
//   round 3 lengths=20199999 grow=999990
//   round 4 lengths=20199999 grow=999990
//
// To measure the bind (not compared): `CRATONVM_DBG_JITC=1` should show a
// `BOUND` line for `java/lang/AbstractStringBuilder.length()I` (or
// `charAt(I)C` / `setLength(I)V`) unless a registered native or an intrinsic
// takes the site first (`final-devirt REFUSED` / `devirt YIELDS` name those).
// Before the fix the bind was refused silently and the site kept
// `jit_invoke_dispatch`. stderr carries the last round's ns/iteration.
public class L2SubstitutedOwner {
    static int lengths(StringBuilder sb, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += sb.length();
            s += sb.charAt(i % sb.length());
        }
        return s;
    }

    static int grow(int n) {
        StringBuilder sb = new StringBuilder();
        int s = 0;
        for (int i = 0; i < n; i++) {
            sb.append('a');
            if (sb.length() > 7) {
                sb.setLength(3);
            }
            s += sb.length();
        }
        return s;
    }

    public static void main(String[] args) {
        StringBuilder sb = new StringBuilder("abc");
        final int n = 200000;
        long ns = 0;
        for (int round = 0; round < 5; round++) {
            long t0 = System.nanoTime();
            int l = lengths(sb, n);
            int g = grow(n);
            ns = System.nanoTime() - t0;
            System.out.println("round " + round + " lengths=" + l + " grow=" + g);
        }
        System.err.println("last round: " + (ns / (2L * n)) + " ns/iteration");
    }
}
