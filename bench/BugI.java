// BUG-I reproducer: JIT null-check elimination wrongly treated a putfield's
// stored VALUE (and an invoke argument) as proven non-null, eliding a later
// `x == null` test. Mirrors Tomcat MessageBytes.setString:
//   strValue = s;  if (s == null) type = T_NULL; else type = T_STR;
// A null `s` wrongly took the else arm under JIT, so isNull() returned false
// (TestMessageBytesConversion.testConversionNull: 432/864 failed, JIT only).
public class BugI {
    static final int T_NULL = 0, T_STR = 1;

    static final class MB {
        Object strValue;
        int type = -9;
        // putfield-value form: `strValue = s` must NOT prove `s` non-null
        void setString(String s) {
            strValue = s;
            if (s == null) { type = T_NULL; } else { type = T_STR; }
        }
    }

    static final class Sink { void take(Object o) {} }
    static final Sink SINK = new Sink();
    // invoke-argument form: `SINK.take(arg)` must NOT prove `arg` non-null
    static int classify(Object arg) {
        SINK.take(arg);
        return (arg == null) ? 0 : 1;
    }

    static int putfieldCase() { MB mb = new MB(); mb.setString(null); return mb.type; }

    public static void main(String[] args) {
        int badPut = 0, badInvoke = 0;
        long pf = -1, ifail = -1;
        for (int i = 0; i < 400000; i++) {
            if (putfieldCase() != T_NULL) { badPut++; if (pf < 0) pf = i; }
            if (classify(null) != 0)      { badInvoke++; if (ifail < 0) ifail = i; }
        }
        System.out.println("putfield-value null-check elided count=" + badPut + " firstFail@" + pf);
        System.out.println("invoke-arg    null-check elided count=" + badInvoke + " firstFail@" + ifail);
        boolean ok = (badPut == 0) && (badInvoke == 0);
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) System.exit(1);
    }
}
