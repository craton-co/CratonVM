// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 2 (lane SPREVIEW): page
// docs/feature-designs/jit-ir-optimizations.md
//
// A single-pass caller splices a tiny static body whose only throwing opcode
// is a `checkcast` (`CRATONVM_JIT_SP_SPLICE_TYPECHECK`, default ON). Before the
// fix the helper call inside the splice filed no inline-frame row, so the
// ClassCastException's trace lost the callee frame. Each fact compares the
// warm (compiled) answer with HotSpot's, or with this run's own cold
// (interpreted) answer, so no line number is baked in.
//
// Run: single-pass only
//   CRATONVM_TIER_C2_THRESHOLD=1000000000 CRATONVM_JIT_OSR_OPTIMIZING=0 cratonvm --real-jdk -cp out J27SpliceCastTrace
// and once with defaults. Expected (HotSpot 25, both runs):
//   fact cast-exact ok
//   fact cast-null null
//   fact cast-subclass 7
//   fact instanceof 1,0,0,1
//   fact cce-message class java.lang.Integer cannot be cast to class java.lang.String (java.lang.Integer and java.lang.String are in module java.base of loader 'bootstrap')
//   fact cce-frames asStr,viaStr
//   fact cce-trace-matches-cold true
//   fact cce-sub-message class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
//   fact cce-sub-frames asNum,viaNum
//   fact cce-array-message class [Ljava.lang.Object; cannot be cast to class [Ljava.lang.String; ([Ljava.lang.Object; and [Ljava.lang.String; are in module java.base of loader 'bootstrap')
//   fact cce-array-frames asStrArr,viaStrArr
// Timing lines only with -Dj27.time=true.
public class J27SpliceCastTrace {
    static String asStr(Object o) { return (String) o; }
    static String viaStr(Object o) { return asStr(o); }

    static Number asNum(Object o) { return (Number) o; }
    static Number viaNum(Object o) { return asNum(o); }

    static String[] asStrArr(Object o) { return (String[]) o; }
    static String[] viaStrArr(Object o) { return asStrArr(o); }

    static boolean isStr(Object o) { return o instanceof String; }
    static boolean viaIsStr(Object o) { return isStr(o); }

    static final int WARM = 50_000;

    /** The first two frame names of `t`, and the full first-two-frame text. */
    static String frames(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        return st.length < 2 ? "short" : st[0].getMethodName() + "," + st[1].getMethodName();
    }

    static String frameText(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        return st.length < 2 ? "short" : st[0] + "|" + st[1];
    }

    static ClassCastException strCce(Object o) {
        try {
            viaStr(o);
            return null;
        } catch (ClassCastException e) {
            return e;
        }
    }

    public static void main(String[] args) {
        boolean time = Boolean.getBoolean("j27.time");
        // Cold: interpreted, the reference trace.
        ClassCastException cold = strCce(Integer.valueOf(1));

        long sink = 0;
        Object s = "seven!!";
        Object[] nums = {Integer.valueOf(3), Long.valueOf(4)};
        String[] sa = {"a"};
        Object[] mixed = {"x", Integer.valueOf(2), null, "y"};
        for (int i = 0; i < WARM; i++) {
            sink += viaStr(s).length();
            sink += viaNum(nums[i & 1]).intValue();
            sink += viaStrArr(sa).length;
            sink += viaIsStr(mixed[i & 3]) ? 1 : 0;
        }
        if (sink == 42) System.out.println("unlikely");

        System.out.println("fact cast-exact " + ("seven!!".equals(viaStr(s)) ? "ok" : "bad"));
        System.out.println("fact cast-null " + viaStr(null));
        System.out.println("fact cast-subclass " + (viaNum(nums[0]).intValue() + viaNum(nums[1]).intValue()));
        StringBuilder io = new StringBuilder();
        for (int k = 0; k < mixed.length; k++) {
            if (k > 0) io.append(',');
            io.append(viaIsStr(mixed[k]) ? 1 : 0);
        }
        System.out.println("fact instanceof " + io);

        ClassCastException warm = strCce(Integer.valueOf(1));
        System.out.println("fact cce-message " + (warm == null ? "none" : warm.getMessage()));
        System.out.println("fact cce-frames " + (warm == null ? "none" : frames(warm)));
        System.out.println("fact cce-trace-matches-cold "
                + (warm != null && cold != null && frameText(warm).equals(frameText(cold))));

        try {
            viaNum("str");
            System.out.println("fact cce-sub-message none");
        } catch (ClassCastException e) {
            System.out.println("fact cce-sub-message " + e.getMessage());
            System.out.println("fact cce-sub-frames " + frames(e));
        }
        try {
            viaStrArr(new Object[1]);
            System.out.println("fact cce-array-message none");
        } catch (ClassCastException e) {
            System.out.println("fact cce-array-message " + e.getMessage());
            System.out.println("fact cce-array-frames " + frames(e));
        }

        if (time) {
            int reps = 5_000_000;
            long t0 = System.nanoTime();
            for (int i = 0; i < reps; i++) sink += viaNum(nums[i & 1]).intValue();
            System.out.println("time cast-subclass " + (System.nanoTime() - t0) * 1000 / reps + " ps/op");
            if (sink == 42) System.out.println("unlikely");
        }
    }
}
