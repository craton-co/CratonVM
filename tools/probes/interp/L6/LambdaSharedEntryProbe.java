/*
 * Interpreter round i1 wave 5, lane L6 (extended wave 6): two `invokedynamic`
 * instructions that share ONE CONSTANT_InvokeDynamic entry (javac folds the
 * identical method reference `String::length` written twice onto one entry).
 * JVMS 5.4.3.6 links each INSTRUCTION separately, and HotSpot spins one
 * lambda class per linkage.
 *
 * Stdout is deterministic. HotSpot 25 prints exactly:
 *
 *   sharedEntryDistinctInstances=true
 *   sharedEntrySameClass=false
 *   sharedEntryAcrossMethodsSameClass=false
 *   capturingSharedEntrySameClass=false
 *   hotSharedEntryMismatches=0
 *   hotSharedEntryClassMismatches=0
 *   hotCapturingClassMismatches=0
 *   hotSingleEntryMismatches=0
 *
 * CratonVM wave 5 printed `true` for the three `...SameClass` lines -- the
 * proxy CLASS was minted per CP entry, see
 * docs/internal/fixed-bugs/interpreter-L6-lambda-proxy-class-is-per-cp-entry-not-per-instruction-FIXED-20260924.md.
 * Every line must match HotSpot with and without --nojit:
 *
 *   hotSharedEntryMismatches  `pair()` becomes hot. Its compile door sees only
 *                             the CP index, cannot tell the two instructions
 *                             apart, and must leave them interpreted rather
 *                             than hand one of them the other's instance.
 *   hot...ClassMismatches     the per-instruction proxy class must not change
 *                             when the method tiers up.
 *   hotSingleEntryMismatches  `single()` becomes hot and compiles; its compiled
 *                             site must keep the instance the interpreter
 *                             handed out (the wave-5 tier-up identity fix).
 */
import java.util.function.Function;
import java.util.function.Supplier;

public class LambdaSharedEntryProbe {
    @SuppressWarnings("unchecked")
    static Function<String, Integer>[] pair() {
        Function<String, Integer>[] r = new Function[2];
        r[0] = String::length;
        r[1] = String::length;
        return r;
    }

    static Function<String, Integer> ref1() {
        return String::length;
    }

    static Function<String, Integer> ref2() {
        return String::length;
    }

    @SuppressWarnings("unchecked")
    static Supplier<Integer>[] capturingPair(String s) {
        Supplier<Integer>[] r = new Supplier[2];
        r[0] = s::length;
        r[1] = s::length;
        return r;
    }

    static Supplier<String> single() {
        return () -> "s";
    }

    public static void main(String[] args) {
        Function<String, Integer>[] first = pair();
        System.out.println("sharedEntryDistinctInstances=" + (first[0] != first[1]));
        System.out.println("sharedEntrySameClass=" + (first[0].getClass() == first[1].getClass()));
        System.out.println(
                "sharedEntryAcrossMethodsSameClass=" + (ref1().getClass() == ref2().getClass()));
        Supplier<Integer>[] caps = capturingPair("abc");
        System.out.println(
                "capturingSharedEntrySameClass=" + (caps[0].getClass() == caps[1].getClass()));

        int shared = 0;
        int sharedClass = 0;
        for (int i = 0; i < 300_000; i++) {
            Function<String, Integer>[] p = pair();
            if (p[0] != first[0] || p[1] != first[1]) {
                shared++;
            }
            if (p[0].getClass() != first[0].getClass() || p[1].getClass() != first[1].getClass()) {
                sharedClass++;
            }
        }
        System.out.println("hotSharedEntryMismatches=" + shared);
        System.out.println("hotSharedEntryClassMismatches=" + sharedClass);

        int capClass = 0;
        int sum = 0;
        for (int i = 0; i < 300_000; i++) {
            Supplier<Integer>[] c = capturingPair("xy");
            if (c[0].getClass() != caps[0].getClass() || c[1].getClass() != caps[1].getClass()) {
                capClass++;
            }
            sum += c[0].get() + c[1].get();
        }
        System.out.println("hotCapturingClassMismatches=" + capClass);
        if (sum != 1_200_000) {
            System.out.println("capturingSum=" + sum);
        }

        Supplier<String> one = single();
        int single = 0;
        for (int i = 0; i < 300_000; i++) {
            if (single() != one) {
                single++;
            }
        }
        System.out.println("hotSingleEntryMismatches=" + single);
    }
}
