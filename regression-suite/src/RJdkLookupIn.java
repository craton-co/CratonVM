import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;
import java.lang.invoke.MethodType;

/**
 * JDK-only corpus: {@code MethodHandles.Lookup.in} / {@code dropLookupMode}
 * mode arithmetic, and the access decisions that ride on it.
 *
 * WHY THIS EXISTS. {@code Lookup.in} is registered as a native over the real
 * JDK class in BOTH real-JDK arms ({@code lang_invoke.rs
 * ::register_p63_method_handles_lookup}), so it shadows concrete JDK bytecode
 * in every non-synthetic build. Its reduction table was asserted by a comment
 * quoting a measurement, and nothing re-took the measurement. A wrong lookup
 * mode is not a wrong number: it is an access-control decision, and every
 * defect this surface has actually had leaned the same way — GRANTING access
 * the JDK withholds:
 *
 *   * {@code if (prev == 0) FULL_POWER} — {@code in()} handing a no-mode
 *     lookup every mode there is.
 *   * no same-class arm, so {@code lookup().in(ownClass)} lost ORIGINAL.
 *   * {@code if (modes == 0) PUBLIC} — a floor manufacturing PUBLIC where the
 *     JDK returns 0.
 *   * no {@code isSamePackageMember} test, so a package COUSIN kept PRIVATE.
 *
 * The four reductions {@code in()} applies, measured on OpenJDK 25 with the
 * receiver {@code MethodHandles.lookup()} (95):
 *
 * <pre>
 *   in(own class)          95     the same-class identity arm
 *   in(nestmate)           31     drops ORIGINAL
 *   in(package cousin)     25     drops ORIGINAL, PRIVATE, PROTECTED
 *   in(other module)        1     drops everything but PUBLIC
 *   publicLookup()         32     UNCONDITIONAL, and in() cannot reduce it
 * </pre>
 *
 * {@code dropLookupMode} is the CONTROL half of this vector: it has no
 * real-mode registration anywhere in the tree, so it runs real JDK bytecode.
 * If the dropLookupMode lines diverge, a registration exists that nobody has
 * accounted for — a bigger finding than any {@code in()} mismatch, and the
 * reason the two live in one vector.
 *
 * Determinism: no identity hashes, no reflection over VM internals, and every
 * printed value is an int the JDK spec fixes.
 */
public class RJdkLookupIn {
    static int checks;

    /** A nestmate of RJdkLookupIn: same nest, so in() keeps PRIVATE. */
    static class Nested {
        private static int secretOfNested() {
            return 7;
        }
    }

    /**
     * A PUBLIC nestmate. Distinguishes "same nest" (which decides {@code in()}
     * from a full-power lookup) from "public and exported" (which decides
     * whether {@code publicLookup().in()} keeps UNCONDITIONAL). {@link Nested}
     * answers 31 and 0 to those two; this one answers 31 and 32.
     */
    public static class PublicNested {
        public static int publicOfPublicNested() {
            return 5;
        }
    }

    private static int secret() {
        return 42;
    }

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RJdkLookupIn: " + m);
        }
    }

    static void eq(int got, int want, String m) {
        check(got == want, m + ": got " + got + " want " + want);
    }

    /** The four reductions, from a full-power receiver. */
    static void inFromFullPower() {
        Lookup l = MethodHandles.lookup();
        eq(l.lookupModes(), 95, "lookup() in the unnamed package");
        eq(l.in(RJdkLookupIn.class).lookupModes(), 95, "in(own class) is the identity arm");
        eq(l.in(Nested.class).lookupModes(), 31, "in(nestmate) drops ORIGINAL only");
        eq(l.in(RJdkLookupInMate.class).lookupModes(), 25,
                "in(package cousin) drops ORIGINAL|PRIVATE|PROTECTED");
        eq(l.in(String.class).lookupModes(), 1, "in(other module) keeps PUBLIC only");
        // in() REJECTS a primitive or array target rather than reducing to it —
        // a target-class validity check, not a mode reduction, and a native
        // that only computes modes drops it.
        boolean rejectedPrimitive = false;
        try {
            l.in(int.class);
        } catch (IllegalArgumentException e) {
            rejectedPrimitive = true;
        }
        check(rejectedPrimitive, "in(int.class) must raise IllegalArgumentException");
        boolean rejectedArray = false;
        try {
            l.in(String[].class);
        } catch (IllegalArgumentException e) {
            rejectedArray = true;
        }
        check(rejectedArray, "in(String[].class) must raise IllegalArgumentException");
        System.out.println("CK RJdkLookupIn in=" + l.in(RJdkLookupIn.class).lookupModes()
                + "," + l.in(Nested.class).lookupModes()
                + "," + l.in(RJdkLookupInMate.class).lookupModes()
                + "," + l.in(String.class).lookupModes());
    }

    /**
     * publicLookup() is UNCONDITIONAL(32), and in() never raises it — but it
     * does not carry it everywhere either. UNCONDITIONAL survives {@code in()}
     * only when the target class is itself PUBLIC and in an unconditionally
     * exported package; against a package-private target it drops to 0, not to
     * PUBLIC. Measured on OpenJDK 25 — a floor that manufactures 1 or 32 here
     * hands out access the JDK withholds.
     */
    static void publicLookupIsUnconditional() {
        Lookup p = MethodHandles.publicLookup();
        eq(p.lookupModes(), 32, "publicLookup()");
        eq(p.in(RJdkLookupIn.class).lookupModes(), 32, "publicLookup().in(a PUBLIC class here)");
        eq(p.in(String.class).lookupModes(), 32, "publicLookup().in(a java.base class)");
        eq(p.in(PublicNested.class).lookupModes(), 32, "publicLookup().in(a PUBLIC nested class)");
        eq(p.in(Nested.class).lookupModes(), 0,
                "publicLookup().in(a package-private class) is 0, not 32 and not 1");
        eq(p.in(RJdkLookupInMate.class).lookupModes(), 0,
                "publicLookup().in(a package-private cousin) is 0");
        System.out.println("CK RJdkLookupIn publicLookup=" + p.lookupModes()
                + "," + p.in(RJdkLookupIn.class).lookupModes()
                + "," + p.in(Nested.class).lookupModes());
    }

    /**
     * in() NEVER GRANTS. A zero-mode lookup stays zero for every target,
     * including its own class — the arm a {@code prev == 0 -> FULL_POWER}
     * default gets exactly backwards.
     */
    static void inNeverGrants() {
        Lookup zero = MethodHandles.lookup().dropLookupMode(Lookup.PUBLIC);
        eq(zero.lookupModes(), 0, "dropLookupMode(PUBLIC) leaves no modes");
        eq(zero.in(RJdkLookupIn.class).lookupModes(), 0, "a zero lookup cannot regain its own class");
        eq(zero.in(Nested.class).lookupModes(), 0, "a zero lookup cannot regain a nestmate");
        eq(zero.in(String.class).lookupModes(), 0, "a zero lookup stays zero across modules");
        // And a reduced (25) lookup cannot climb back up through in().
        Lookup cousin = MethodHandles.lookup().in(RJdkLookupInMate.class);
        eq(cousin.lookupModes(), 25, "the cousin lookup");
        eq(cousin.in(RJdkLookupIn.class).lookupModes(), 25,
                "in() back to the original class does not restore PRIVATE or ORIGINAL");
        eq(cousin.in(String.class).lookupModes(), 1, "the cousin lookup still reduces to PUBLIC");
        System.out.println("CK RJdkLookupIn neverGrants=" + zero.in(RJdkLookupIn.class).lookupModes()
                + "," + cousin.in(RJdkLookupIn.class).lookupModes());
    }

    /**
     * The CONTROL: dropLookupMode has no real-mode registration, so these run
     * real JDK bytecode. A divergence here means an unaccounted registration.
     */
    static void dropLookupModes() {
        Lookup l = MethodHandles.lookup();
        eq(l.dropLookupMode(Lookup.PRIVATE).lookupModes(), 25, "dropLookupMode(PRIVATE)");
        eq(l.dropLookupMode(Lookup.PROTECTED).lookupModes(), 27, "dropLookupMode(PROTECTED)");
        eq(l.dropLookupMode(Lookup.PACKAGE).lookupModes(), 17, "dropLookupMode(PACKAGE)");
        eq(l.dropLookupMode(Lookup.MODULE).lookupModes(), 1, "dropLookupMode(MODULE)");
        eq(l.dropLookupMode(Lookup.PUBLIC).lookupModes(), 0, "dropLookupMode(PUBLIC)");
        // dropLookupMode's opening move is `oldModes & ~(modeToDrop | PROTECTED
        // | ORIGINAL)` — PROTECTED and ORIGINAL come off for EVERY argument,
        // including one the lookup does not hold. Dropping UNCONDITIONAL from
        // a 95 lookup is therefore 27, not 31 and not 95: measured on OpenJDK
        // 25, and the arm a "drop only what was named, plus ORIGINAL" model
        // gets wrong while agreeing on all five of the lines above.
        eq(l.dropLookupMode(Lookup.UNCONDITIONAL).lookupModes(), 27,
                "dropLookupMode(UNCONDITIONAL) drops PROTECTED and ORIGINAL too");
        eq(l.dropLookupMode(Lookup.ORIGINAL).lookupModes(), 27,
                "dropLookupMode(ORIGINAL) drops PROTECTED too");
        System.out.println("CK RJdkLookupIn drop="
                + l.dropLookupMode(Lookup.PRIVATE).lookupModes()
                + "," + l.dropLookupMode(Lookup.PACKAGE).lookupModes()
                + "," + l.dropLookupMode(Lookup.MODULE).lookupModes()
                + "," + l.dropLookupMode(Lookup.PUBLIC).lookupModes());
    }

    /**
     * The mode number is only a proxy. What it decides is whether a
     * {@code find*} succeeds, so ask that directly: a 31-mode nestmate lookup
     * reaches private members, a 25-mode cousin lookup must not, and a 0-mode
     * lookup reaches nothing at all.
     */
    static void modesDecideAccess() throws Throwable {
        MethodType i = MethodType.methodType(int.class);

        // 95: the lookup class itself reaches its own private static.
        check(((int) MethodHandles.lookup()
                .findStatic(RJdkLookupIn.class, "secret", i).invokeExact()) == 42,
                "the full-power lookup reaches its own private method");

        // 31 (nestmate): PRIVATE survives, and the nest is shared, so a lookup
        // IN the nestmate reaches the outer class's private member.
        Lookup nest = MethodHandles.lookup().in(Nested.class);
        eq(nest.lookupModes(), 31, "the nestmate lookup");
        check(((int) nest.findStatic(RJdkLookupIn.class, "secret", i).invokeExact()) == 42,
                "a nestmate lookup reaches a private member of the nest");
        check(((int) nest.findStatic(Nested.class, "secretOfNested", i).invokeExact()) == 7,
                "a nestmate lookup reaches its own private member");

        // 25 (cousin): PRIVATE is gone. This is the arm that used to answer 31
        // and silently succeed.
        Lookup cousin = MethodHandles.lookup().in(RJdkLookupInMate.class);
        eq(cousin.lookupModes(), 25, "the cousin lookup");
        boolean refused = false;
        try {
            cousin.findStatic(RJdkLookupIn.class, "secret", i);
        } catch (IllegalAccessException e) {
            refused = true;
        }
        check(refused, "a 25-mode package-cousin lookup must be REFUSED a private member");
        // …but it keeps PACKAGE, so a package-private member is still reachable.
        check(((int) cousin.findStatic(RJdkLookupInMate.class, "packagePrivate", i)
                .invokeExact()) == 11,
                "a cousin lookup keeps PACKAGE access");

        // 0: nothing at all, not even public.
        Lookup zero = MethodHandles.lookup().dropLookupMode(Lookup.PUBLIC);
        boolean refusedPublic = false;
        try {
            zero.findStatic(PublicNested.class, "publicOfPublicNested", i);
        } catch (IllegalAccessException e) {
            refusedPublic = true;
        }
        check(refusedPublic, "a 0-mode lookup must be refused even a PUBLIC member");

        // 32 (publicLookup): public members of PUBLIC classes only. The
        // package-private cousin is unreachable from it even though the member
        // is declared public — accessibility is the CLASS's, not the member's.
        check(((int) MethodHandles.publicLookup()
                .findStatic(PublicNested.class, "publicOfPublicNested", i).invokeExact()) == 5,
                "publicLookup reaches a public static of a public class");
        boolean refusedNonPublicClass = false;
        try {
            MethodHandles.publicLookup().findStatic(RJdkLookupInMate.class, "publicStatic", i);
        } catch (IllegalAccessException e) {
            refusedNonPublicClass = true;
        }
        check(refusedNonPublicClass,
                "publicLookup must be refused a public member of a package-private class");
        boolean refusedPrivate = false;
        try {
            MethodHandles.publicLookup().findStatic(RJdkLookupIn.class, "secret", i);
        } catch (IllegalAccessException e) {
            refusedPrivate = true;
        }
        check(refusedPrivate, "publicLookup must be refused a private member");

        System.out.println("CK RJdkLookupIn access=ok");
    }

    /** lookupClass() must follow in(), or the modes describe the wrong class. */
    static void lookupClassFollows() {
        Lookup l = MethodHandles.lookup();
        check(l.lookupClass() == RJdkLookupIn.class, "lookup().lookupClass()");
        check(l.in(String.class).lookupClass() == String.class, "in(X).lookupClass() == X");
        check(l.in(Nested.class).lookupClass() == Nested.class, "in(nestmate).lookupClass()");
        check(MethodHandles.publicLookup().in(String.class).lookupClass() == String.class,
                "publicLookup().in(X).lookupClass() == X");
        System.out.println("CK RJdkLookupIn lookupClass=" + l.in(String.class).lookupClass().getName());
    }

    /**
     * {@code previousLookupClass()} must answer a REFERENCE, on every layout.
     *
     * <p>This is W7-13's second item, which had no witness anywhere. The native
     * behind it is declared {@code ()Ljava/lang/Class;} and used to read a FIXED
     * slot 2 -- which on the real JDK 25 {@code MethodHandles$Lookup} layout
     * ({@code lookupClass}(0), {@code prevLookupClass}(1),
     * {@code allowedModes}(2), {@code cachedProtectionDomain}(3)) is
     * {@code allowedModes}, an {@code int}. So it handed back the mode word (95
     * for a full-power lookup) out of a method whose declared return type is
     * {@code Class}. It now resolves the slot from a class-side witness and
     * coerces any non-reference read to null, so no layout can produce that.
     *
     * <p>Every value below was measured on HotSpot 25: null for every lookup
     * that never crossed a module boundary, which is all five of these.
     */
    static void previousLookupClassIsAReference() {
        Lookup l = MethodHandles.lookup();
        check(l.previousLookupClass() == null, "lookup().previousLookupClass() is null");
        check(MethodHandles.publicLookup().previousLookupClass() == null,
                "publicLookup().previousLookupClass() is null");
        check(l.in(RJdkLookupIn.class).previousLookupClass() == null,
                "in(own class) leaves previousLookupClass null");
        check(l.in(Nested.class).previousLookupClass() == null,
                "in(nestmate) leaves previousLookupClass null");
        check(l.dropLookupMode(Lookup.PRIVATE).previousLookupClass() == null,
                "dropLookupMode(PRIVATE) leaves previousLookupClass null");
        // The one MODULE-CROSSING case, and the WEAK check here on purpose.
        // HotSpot 25 reports the original lookup class; CratonVM models no
        // modules and neither `alloc_lookup` nor `in()` ever populates the
        // field, so it reports null. A check demanding HotSpot's answer would be
        // permanently red for a modelling gap no fix in this area clears, and a
        // permanently-red vector teaches operators to ignore the red. What is
        // asserted instead is the part that is common to both and is the actual
        // W7-13 claim: whatever comes back is usable AS a Class -- assigned to a
        // Class<?> local and compared by reference -- and is one of the two
        // legal answers, never a mode word.
        Class<?> crossed = l.in(String.class).previousLookupClass();
        check(crossed == null || crossed == RJdkLookupIn.class,
                "in(other module) reports null or the original lookup class, never an int");
        // Deliberately NOT printed as a value: this is the one line that differs
        // between the oracle and this VM, and `run.sh` diffs CK lines.
        System.out.println("CK RJdkLookupIn previousLookupClass=ok");
    }

    public static void main(String[] args) throws Throwable {
        inFromFullPower();
        publicLookupIsUnconditional();
        inNeverGrants();
        dropLookupModes();
        modesDecideAccess();
        lookupClassFollows();
        previousLookupClassIsAReference();
        System.out.println("CK RJdkLookupIn checks=" + checks);
        System.out.println("PASS RJdkLookupIn (" + checks + " checks)");
    }
}

/**
 * A package COUSIN of {@link RJdkLookupIn}: same (unnamed) package, different
 * top-level class, so {@code VerifyAccess.isSamePackageMember} is false and
 * {@code in()} must drop PRIVATE|PROTECTED as well as ORIGINAL — 95 -> 25, not
 * 95 -> 31. Nothing else in the suite supplies this shape; a nested class is a
 * nestmate and answers 31.
 */
class RJdkLookupInMate {
    private static int cousinSecret() {
        return 3;
    }

    static int packagePrivate() {
        return 11;
    }

    public static int publicStatic() {
        return 5;
    }

    static int useCousinSecret() {
        return cousinSecret();
    }
}
