// difftest: strict
//
// `ldc` answered from the recorded-resolution store
// (`CRATONVM_JIT_NO_LDC_CONST_CACHE=1` opts out).
//
// Every `ldc` tag whose resolution is now RECORDED is exercised here more than
// once from the same site, because the second and later executions take a
// different code path from the first: they answer from the store, before the
// class_manager lock, without re-reading the constant pool. A cache that
// returns the wrong entry is silent — an `ldc` pushes a plausible constant of
// the right shape — so every line prints an exact value.
//
//   * String literals, including the identity JVMS §5.1 requires. Two `ldc`s
//     of one CONSTANT_String must be `==`, and must equal `.intern()`.
//   * Lone-surrogate literals. These took a constructor that pooled NOTHING
//     and allocated a fresh String per execution, so `==` answered false where
//     HotSpot answers true. They are the reason this seed exists in `strict`.
//   * Class literals: the mirror is per-ClassId, so identity must hold across
//     sites and across executions, and `getName()` must not drift.
//   * MethodType / MethodHandle, which already recorded but only AFTER taking
//     the lock — the probe moved ahead of it, so their arms changed too.
//   * The primitives, which deliberately do NOT record: they are the control
//     that must be unaffected either way.
//   * A failed resolution, which must NOT be recorded — the error has to be
//     raised again on the next execution rather than answered from a hole.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class LdcConstCache {

    static final String PLAIN = "ldc-plain-literal";
    static final String LONE_HIGH = "\uD800";
    static final String LONE_LOW = "\uDC00";
    static final String PAIR = "\uD83D\uDE00";
    static final String EMPTY = "";

    static String plain() { return "ldc-plain-literal"; }
    static String loneHigh() { return "\uD800"; }
    static Class<?> stringClass() { return String.class; }

    static int ints() { return 1234567 + 1234567; }
    static float floats() { return 3.5f + 3.5f; }
    static long longs() { return 0x0123_4567_89AB_CDEFL; }
    static double doubles() { return 2.718281828459045d; }

    public static void main(String[] args) throws Throwable {
        // 1. Repeated execution of one site. Run each loop enough times that
        //    the first (resolving) execution is a small minority.
        for (int i = 0; i < 3; i++) {
            System.out.println("plain[" + i + "]: " + plain() + " len=" + plain().length());
            System.out.println("lone[" + i + "]: len=" + loneHigh().length()
                    + " cp=" + (int) loneHigh().charAt(0));
            System.out.println("int[" + i + "]: " + ints() + " float=" + floats());
            System.out.println("long[" + i + "]: " + longs() + " double=" + doubles());
        }

        // 2. Literal identity — the JVMS §5.1 contract.
        //
        // Every row that can be written as a comparison of two compile-time
        // constants is routed through a NON-FINAL local or a method call
        // instead. javac folds `"a" == "a"` — and equally `STATIC_FINAL == "a"`
        // — into a literal `true`/`false` and emits one ldc of the result, so
        // the row would print the right answer on a VM that never interned
        // anything and never executed the opcode. Six of the ten rows here were
        // written that way first and tested nothing; the ones that caught the
        // real defect were the ones going through `loneHigh()` and `.intern()`.
        String sp1 = "ldc-plain-literal", sp2 = "ldc-plain-literal";
        String sl1 = "\uD800", sl2 = "\uD800";
        String slo = "\uDC00", spr = "😀", sem = "";
        String vHigh = LONE_HIGH, vLow = LONE_LOW, vPair = PAIR, vEmpty = EMPTY;
        System.out.println("id same-site plain: " + (sp1 == sp2));
        System.out.println("id cross-site plain: " + (PLAIN == plain()));
        System.out.println("id intern plain: " + (PLAIN == PLAIN.intern()));
        System.out.println("id same-site lone: " + (sl1 == sl2));
        System.out.println("id cross-site lone: " + (LONE_HIGH == loneHigh()));
        System.out.println("id intern lone: " + (LONE_HIGH == LONE_HIGH.intern()));
        System.out.println("id lone-low: " + (vLow == slo));
        System.out.println("id pair: " + (vPair == spr));
        System.out.println("id empty: " + (vEmpty == sem));
        System.out.println("distinct lone: " + (vHigh == vLow));

        // 3. Content must survive the identity change.
        System.out.println("content lone-high: " + (int) LONE_HIGH.charAt(0)
                + " len=" + LONE_HIGH.length()
                + " eq=" + LONE_HIGH.equals(loneHigh())
                + " hash=" + LONE_HIGH.hashCode());
        System.out.println("content pair: " + (int) PAIR.charAt(0) + "," + (int) PAIR.charAt(1)
                + " len=" + PAIR.length() + " hash=" + PAIR.hashCode());

        // 4. Class literals: identity and name, repeated.
        for (int i = 0; i < 3; i++) {
            System.out.println("class[" + i + "]: " + String.class.getName()
                    + " id=" + (String.class == stringClass())
                    + " arr=" + int[].class.getName()
                    + " objarr=" + Object[].class.getName()
                    + " prim=" + int.class.getName());
        }
        System.out.println("class distinct: " + ((Object) String.class == (Object) Integer.class));

        // 5. MethodType / MethodHandle — recorded before, but the probe moved.
        for (int i = 0; i < 3; i++) {
            MethodType mt = MethodType.methodType(String.class, int.class);
            System.out.println("mt[" + i + "]: " + mt);
        }
        MethodHandle mh = MethodHandles.lookup()
                .findStatic(LdcConstCache.class, "plain", MethodType.methodType(String.class));
        System.out.println("mh: " + (String) mh.invokeExact());

        // 6. A resolution that FAILS must fail every time, not once.
        for (int i = 0; i < 3; i++) {
            try {
                Class<?> c = Class.forName("no.such.Class$Missing");
                System.out.println("unreachable " + c);
            } catch (ClassNotFoundException e) {
                System.out.println("cnfe[" + i + "]: " + e.getClass().getName());
            }
        }
    }
}
