import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Array;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.nio.ByteOrder;

/**
 * Reflective boxing IDENTITY — both directions.
 *
 * <h2>What this vector exists to catch, and why nothing else could</h2>
 *
 * <p>A VM has to decide, at every reflective read that hands back a primitive, whether to return the
 * <em>canonical</em> wrapper ({@code Integer.valueOf}-style, the one in the cache) or a
 * <em>fresh</em> allocation. HotSpot 25 does both, on different paths, and the difference is
 * observable with {@code ==}. Until this file landed, <b>nothing anywhere under
 * {@code regression-suite/src} asserted it</b>: a change that made every reflective path canonical
 * and its exact inverse both passed the entire suite.
 *
 * <p>The reason the gap survived so long is in every row below. Every {@code false} row here is
 * still {@code .equals}-equal to the canonical instance — {@code equalsFold} and {@code equalsFresh}
 * assert exactly that — so an equality-shaped check passes against this defect <b>in both
 * directions</b>. Only {@code ==} can see it, which is why no row in this file compares payloads.
 *
 * <h2>The asymmetry, and why it is behaviour rather than an accident</h2>
 *
 * <p>Since JDK 18 {@code Field.get} and {@code Method.invoke} run on {@code MethodHandle}-based
 * accessors ({@code MethodHandleIntegerFieldAccessorImpl} and friends) whose boxing step is a direct
 * handle to {@code Integer.valueOf} — so they inherit {@code IntegerCache} for free.
 * {@code java.lang.reflect.Array.get} is a VM native, {@code Reflection::array_get}, which boxes
 * with {@code java_lang_boxing_object::create}: that function allocates and has never consulted a
 * cache. So {@code Array.get(new int[]{7}, 0) == Integer.valueOf(7)} is {@code false}, and
 * {@code Array.get} twice is not even identical to <em>itself</em>.
 *
 * <p>That is an implementation detail of the JDK, and it is nonetheless observable, and therefore it
 * is behaviour. <b>Do not "unify" the two.</b> A vector that asserted only the canonical half would
 * pass on a VM that made everything canonical, which is why {@link #arrayget()} and
 * {@link #controls()} exist and why they are the sharper half of this file.
 *
 * <h2>Not an extension of {@code RJdkIntrinsics3}'s {@code boxid} family</h2>
 *
 * <p>{@code boxid} is 69 checks and its published result leans on the property that
 * {@code RJdkIntrinsics3} imports no {@code java.lang.reflect} type at all — that is what makes
 * "none of the 69 is affected by a change to the reflective boxing helpers" a mechanical claim
 * rather than a skim. Adding reflection there would invalidate a published result, so these rows are
 * a separate class.
 *
 * <h2>Every expected value was MEASURED on the oracle</h2>
 *
 * <p>All 107 expectations come from {@code scratchpad/f19/ReflBoxOracle.java} run on Microsoft
 * OpenJDK 25.0.3+9 (build 25.0.3+9-LTS), except the four {@code mhcollc}/{@code mhcolll} rows,
 * which come from {@code scratchpad/f29/CollBox2.java} §G and were re-measured in place by lane
 * F39 by running this whole class on the same oracle. The transcripts are §2 of
 * {@code docs/known-issues/jdk-only/F19-1-the-sixteen-that-allocated-and-the-boolean-that-was-not-TRUE-20260813.md}
 * and §2 of {@code docs/known-issues/jdk-only/F29-1-the-wrapper-class-comes-from-the-call-site-not-the-methodtype-20260813.md}.
 * Nothing here was typed from memory or inferred from the JDK sources. The oracle output was
 * byte-identical across three runs, under {@code -Xint}, and under
 * {@code -XX:-UseCompressedOops} — so no row is a JIT or a heap-layout artefact.
 *
 * <h2>Output dialect — read this before changing a println</h2>
 *
 * <p>{@code run.sh}'s {@code extract()} keeps only lines prefixed {@code PASS } or {@code CK } and
 * deletes the rest before the cross-VM diff. So:
 *
 * <ul>
 *   <li>every row funnels through {@link #ck}, which prints {@code CK RJdkReflBox <name>=<ACTUAL>}.
 *       The value is the VM's own answer, never this file's expectation, so the two VMs diff their
 *       answers against <em>each other</em> and not each against a constant.
 *   <li>the tail is {@code CK RJdkReflBox fails=N}, then {@code CK RJdkReflBox checks=N}, then
 *       {@code PASS RJdkReflBox (N checks)} on the clean path only. {@code fails} and {@code checks}
 *       are on SEPARATE lines deliberately: {@code harness_check_count} does
 *       {@code sub(/^.*checks=/, ""); print}, so a combined line publishes the "count"
 *       {@code "N fails=M"} and G3's numeric test then errors out inside its own
 *       {@code 2>/dev/null} — the guard silently no-ops.
 *   <li>nothing throws mid-run. A family that dies takes its remaining rows with it, and those rows
 *       are the evidence; each family is therefore wrapped, its throwable reported as a row, and the
 *       process exits non-zero only at the end. A VM that fails half of this file still prints the
 *       other half, which is the difference between a diff that names the defect and a diff that
 *       says "it stopped".
 * </ul>
 *
 * <p>Operands are non-{@code final} statics. A {@code static final int} initialised with a literal
 * is a compile-time constant and {@code javac} folds every use of it, which would make some rows
 * assert against {@code javac}'s constant pool rather than the VM's boxing.
 */
public class RJdkReflBox {

    static int checks;
    static int fails;
    static int mark;

    // ---- operands. Non-final on purpose (see the class javadoc). ----------
    static int I7 = 7;
    static char CA = 'a';
    static byte B3 = 3;
    static short S9 = 9;
    static boolean ZT = true;
    static long J5 = 5L;
    static float F1 = 1f;
    static double D1 = 1d;
    // Outside every cache bound: IntegerCache/ShortCache/LongCache are
    // [-128,127] and CharacterCache is [0,127]. ByteCache has no fresh arm at
    // all — all 256 values are cached — which is why no `byte` row appears in
    // the out-of-bound family.
    static int I1000 = 1000;
    static char C200 = 200;
    static short S1000 = 1000;
    static long J1000 = 1000L;

    public static class Holder {
        public int i = 7;
        public char c = 'a';
        public byte b = 3;
        public short s = 9;
        public boolean z = true;
        public long j = 5L;
        public float f = 1f;
        public double d = 1d;
        public int iBig = 1000;
        public char cBig = 200;
        public short sBig = 1000;
        public long jBig = 1000L;

        public static int si = 7;
        public static char sc = 'a';
        public static boolean sz = true;
        public static long sj = 5L;
        public static int siBig = 1000;
    }

    public static int mInt() {
        return 7;
    }

    public static char mChar() {
        return 'a';
    }

    public static byte mByte() {
        return 3;
    }

    public static short mShort() {
        return 9;
    }

    public static boolean mBool() {
        return true;
    }

    public static long mLong() {
        return 5L;
    }

    public static float mFloat() {
        return 1f;
    }

    public static double mDouble() {
        return 1d;
    }

    public static int mIntBig() {
        return 1000;
    }

    public static char mCharBig() {
        return 200;
    }

    /** The collector target: hands back its first element, still boxed. */
    public static Object firstOf(Object[] xs) {
        return xs[0];
    }

    public interface Svc {
        void take(int i, char c, boolean z, long j, byte b, short s, int big);
    }

    // -- the funnel ---------------------------------------------------------

    /**
     * The single funnel. Prints the ACTUAL answer unconditionally — a line that only appears when a
     * check passes is a line the diff cannot use, and one that carried the EXPECTED value would make
     * both VMs print the same text whatever they did.
     */
    static void ck(String name, boolean actual, boolean expected) {
        checks++;
        System.out.println("CK RJdkReflBox " + name + "=" + actual);
        if (actual != expected) {
            fails++;
            System.out.println("CK RJdkReflBox FAILED " + name + " expected=" + expected);
        }
    }

    /**
     * Close a family and assert its own size. The count is a tripwire: a family that silently loses
     * rows to an edit still prints its {@code CK} line, and a hard-coded number nobody re-derives is
     * how a shrinking vector goes unnoticed. A mismatch is a FAILURE (it reaches {@code fails} and
     * the non-zero exit), not a throw — a family that died halfway must still let the families after
     * it report.
     */
    static void sectionEnd(String name, int expected) {
        int n = checks - mark;
        mark = checks;
        System.out.println("CK RJdkReflBox " + name + "=" + n);
        if (n != expected) {
            fails++;
            System.out.println(
                    "CK RJdkReflBox FAILED " + name + " ran " + n + " checks, header says "
                            + expected);
        }
    }

    // ---- 1. Field.get, instance, inside every cache bound -------------------
    //
    // MEASURED true, all nine. `Field.get` boxes through a MethodHandle field
    // accessor whose boxing step IS `X.valueOf`.

    static void fieldget() throws Exception {
        Holder h = new Holder();
        Class<Holder> hc = Holder.class;
        ck("field.int", hc.getField("i").get(h) == Integer.valueOf(I7), true);
        ck("field.char", hc.getField("c").get(h) == Character.valueOf(CA), true);
        ck("field.byte", hc.getField("b").get(h) == Byte.valueOf(B3), true);
        ck("field.short", hc.getField("s").get(h) == Short.valueOf(S9), true);
        ck("field.bool", hc.getField("z").get(h) == Boolean.valueOf(ZT), true);
        // Not a duplicate of the row above: `Boolean.valueOf(b)` is literally
        // `return b ? TRUE : FALSE` in the JDK, so a VM can satisfy
        // `field.bool` with a private cache and still fail this one. This is
        // the identity Xerces' XML11Configuration.configurePipeline() tests
        // (`fFeatures.get(…) == Boolean.TRUE`).
        ck("field.boolTRUE", hc.getField("z").get(h) == Boolean.TRUE, true);
        ck("field.long", hc.getField("j").get(h) == Long.valueOf(J5), true);

        // Stability. A cache that is populated but consulted only on the first
        // call — or one that is invalidated by accessor inflation — passes
        // every single-shot row above and fails these two.
        boolean stableFresh = true;
        for (int k = 0; k < 100; k++) {
            if (hc.getField("i").get(h) != Integer.valueOf(I7)) {
                stableFresh = false;
            }
        }
        ck("field.intStable100", stableFresh, true);
        Field fi = hc.getField("i");
        boolean stableSame = true;
        for (int k = 0; k < 100; k++) {
            if (fi.get(h) != Integer.valueOf(I7)) {
                stableSame = false;
            }
        }
        ck("field.intStableSameField100", stableSame, true);
        sectionEnd("fieldget", 9);
    }

    // ---- 2. Field.get, static ----------------------------------------------
    //
    // A separate family because a static read takes a different slot lookup on
    // both VMs, and MEASURED it gives the same answer: canonical.

    static void fieldstatic() throws Exception {
        Class<Holder> hc = Holder.class;
        ck("fieldstatic.int", hc.getField("si").get(null) == Integer.valueOf(I7), true);
        ck("fieldstatic.char", hc.getField("sc").get(null) == Character.valueOf(CA), true);
        ck("fieldstatic.bool", hc.getField("sz").get(null) == Boolean.valueOf(ZT), true);
        ck("fieldstatic.long", hc.getField("sj").get(null) == Long.valueOf(J5), true);
        sectionEnd("fieldstatic", 4);
    }

    // ---- 3. The arms a canonical path must NOT cache ------------------------
    //
    // MEASURED false, every one. Two independent reasons live here and they
    // must not be collapsed: a value outside its type's cache bound (the
    // `valueOf` natives' own uncached arm), and `float`/`double`, which have no
    // cache on HotSpot at all. A VM that "completed the family" to eight
    // wrapper types, or that widened a bound, fails here and nowhere else.

    static void fieldfresh() throws Exception {
        Holder h = new Holder();
        Class<Holder> hc = Holder.class;
        ck("fieldoob.int1000", hc.getField("iBig").get(h) == Integer.valueOf(I1000), false);
        ck("fieldoob.char200", hc.getField("cBig").get(h) == Character.valueOf(C200), false);
        ck("fieldoob.short1000", hc.getField("sBig").get(h) == Short.valueOf(S1000), false);
        ck("fieldoob.long1000", hc.getField("jBig").get(h) == Long.valueOf(J1000), false);
        ck("fieldoob.static1000", hc.getField("siBig").get(null) == Integer.valueOf(I1000), false);
        ck("fieldnc.float", hc.getField("f").get(h) == Float.valueOf(F1), false);
        ck("fieldnc.double", hc.getField("d").get(h) == Double.valueOf(D1), false);
        // Self-identity: two reads of the SAME field are two objects. This is
        // what "fresh" means, and it cannot be satisfied by any cache.
        ck("fieldoob.selfid",
                hc.getField("iBig").get(h) == hc.getField("iBig").get(h), false);
        ck("fieldnc.floatSelfid", hc.getField("f").get(h) == hc.getField("f").get(h), false);
        // The blindness this whole file exists because of: BOTH fresh rows are
        // `.equals`-equal. If these two ever report false, the payloads are
        // wrong and every `==` row above is answering a different question
        // than the one it was written to ask.
        ck("equalsFold", hc.getField("iBig").get(h).equals(Integer.valueOf(I1000)), true);
        ck("equalsFresh", hc.getField("f").get(h).equals(Float.valueOf(F1)), true);
        sectionEnd("fieldfresh", 11);
    }

    // ---- 4. java.lang.reflect.Array.get — the FRESH half --------------------
    //
    // MEASURED false for every primitive component, on BOTH VMs. This is the
    // family that fails if a VM unifies its two boxing helpers, and no
    // equality-shaped check anywhere can see it.

    static void arrayget() {
        ck("array.int", Array.get(new int[] {I7}, 0) == Integer.valueOf(I7), false);
        ck("array.char", Array.get(new char[] {CA}, 0) == Character.valueOf(CA), false);
        ck("array.byte", Array.get(new byte[] {B3}, 0) == Byte.valueOf(B3), false);
        ck("array.short", Array.get(new short[] {S9}, 0) == Short.valueOf(S9), false);
        ck("array.bool", Array.get(new boolean[] {ZT}, 0) == Boolean.valueOf(ZT), false);
        ck("array.boolTRUE", Array.get(new boolean[] {ZT}, 0) == Boolean.TRUE, false);
        ck("array.long", Array.get(new long[] {J5}, 0) == Long.valueOf(J5), false);
        int[] one = new int[] {I7};
        ck("array.selfid", Array.get(one, 0) == Array.get(one, 0), false);
        ck("array.blindEquals", Array.get(one, 0).equals(Integer.valueOf(I7)), true);
        // The two rows that stop the fresh contract from over-reaching. A
        // REFERENCE component is handed back as itself — `Array.get` allocates
        // a box, it does not copy an object — and `Array.getInt` returns a
        // primitive whose autoboxing at the call site goes through `valueOf`
        // like any other autobox, so it IS canonical.
        Object[] refs = new Object[] {Integer.valueOf(I7)};
        ck("array.refPassthrough", Array.get(refs, 0) == Integer.valueOf(I7), true);
        ck("array.getIntAutobox", (Object) Array.getInt(one, 0) == Integer.valueOf(I7), true);
        sectionEnd("arrayget", 11);
    }

    // ---- 5. Method.invoke return boxing -------------------------------------

    static void invoke() throws Exception {
        Class<?> self = RJdkReflBox.class;
        ck("invoke.int", self.getMethod("mInt").invoke(null) == Integer.valueOf(I7), true);
        ck("invoke.char", self.getMethod("mChar").invoke(null) == Character.valueOf(CA), true);
        ck("invoke.byte", self.getMethod("mByte").invoke(null) == Byte.valueOf(B3), true);
        ck("invoke.short", self.getMethod("mShort").invoke(null) == Short.valueOf(S9), true);
        ck("invoke.bool", self.getMethod("mBool").invoke(null) == Boolean.valueOf(ZT), true);
        ck("invoke.boolTRUE", self.getMethod("mBool").invoke(null) == Boolean.TRUE, true);
        ck("invoke.long", self.getMethod("mLong").invoke(null) == Long.valueOf(J5), true);
        ck("invokenc.float", self.getMethod("mFloat").invoke(null) == Float.valueOf(F1), false);
        ck("invokenc.double", self.getMethod("mDouble").invoke(null) == Double.valueOf(D1), false);
        ck("invokeoob.int1000",
                self.getMethod("mIntBig").invoke(null) == Integer.valueOf(I1000), false);
        ck("invokeoob.char200",
                self.getMethod("mCharBig").invoke(null) == Character.valueOf(C200), false);
        Method mi = self.getMethod("mIntBig");
        ck("invokeoob.selfid", mi.invoke(null) == mi.invoke(null), false);
        sectionEnd("invoke", 12);
    }

    // ---- 6. MethodHandle return adaptation ----------------------------------
    //
    // `asType(…Object.class)` inserts a `valueOf` handle for the
    // primitive->Object step, so every in-bound integral row is canonical and
    // the float / out-of-bound rows are not.

    static void mhreturn() throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Class<?> self = RJdkReflBox.class;
        MethodType toObject = MethodType.methodType(Object.class);
        MethodHandle hInt = lk.findStatic(self, "mInt", MethodType.methodType(int.class));
        MethodHandle hChar = lk.findStatic(self, "mChar", MethodType.methodType(char.class));
        MethodHandle hBool = lk.findStatic(self, "mBool", MethodType.methodType(boolean.class));
        MethodHandle hLong = lk.findStatic(self, "mLong", MethodType.methodType(long.class));
        MethodHandle hByte = lk.findStatic(self, "mByte", MethodType.methodType(byte.class));
        MethodHandle hShort = lk.findStatic(self, "mShort", MethodType.methodType(short.class));
        MethodHandle hFloat = lk.findStatic(self, "mFloat", MethodType.methodType(float.class));
        MethodHandle hIntBig = lk.findStatic(self, "mIntBig", MethodType.methodType(int.class));

        ck("mh.asTypeInt", hInt.asType(toObject).invoke() == Integer.valueOf(I7), true);
        ck("mh.asTypeChar", hChar.asType(toObject).invoke() == Character.valueOf(CA), true);
        ck("mh.asTypeBool", hBool.asType(toObject).invoke() == Boolean.valueOf(ZT), true);
        ck("mh.asTypeLong", hLong.asType(toObject).invoke() == Long.valueOf(J5), true);
        ck("mh.asTypeByte", hByte.asType(toObject).invoke() == Byte.valueOf(B3), true);
        ck("mh.asTypeShort", hShort.asType(toObject).invoke() == Short.valueOf(S9), true);
        ck("mhnc.asTypeFloat", hFloat.asType(toObject).invoke() == Float.valueOf(F1), false);
        ck("mhoob.asTypeInt1000",
                hIntBig.asType(toObject).invoke() == Integer.valueOf(I1000), false);
        // `invoke` (not `invokeExact`) against an Object-typed call site does
        // the same adaptation without an explicit `asType`.
        Object viaInvoke = hInt.invoke();
        ck("mh.invokeAsObject", viaInvoke == Integer.valueOf(I7), true);
        ck("mh.invokeWithArgsInt", hInt.invokeWithArguments() == Integer.valueOf(I7), true);
        ck("mh.invokeWithArgsChar", hChar.invokeWithArguments() == Character.valueOf(CA), true);
        ck("mh.invokeWithArgsLong", hLong.invokeWithArguments() == Long.valueOf(J5), true);
        sectionEnd("mhreturn", 12);
    }

    // ---- 7. MethodHandle collector / varargs element boxing -----------------
    //
    // Two different questions live here and the split between them is not the
    // one F19-1 N2 drew. That record said the wrapper CLASS comes from the
    // target handle's `MethodType`; F29-1 §1 MEASURED that it does not —
    // `coll.type()` is `(Object)Object` and one such handle produces six
    // different wrapper classes. The class comes from the CALL SITE's static
    // parameter type, which `asType` boxes by.
    //
    // So the rows here divide by whether the call site is recoverable:
    //
    //   * a WRAPPER-TYPED component (`Character[]`, `Long[]`) settles the
    //     class at the collector arm itself, and HotSpot answers every
    //     cross-type call to such a collector with a
    //     `WrongMethodTypeException` (F29-1 §2), so for every call that runs
    //     at all the component IS the answer. Those rows are below.
    //   * an `Object[]` component settles nothing, and CratonVM cannot see
    //     the call-site descriptor from a native at all (F29-1 §2.3 /
    //     NOMINATION 2). `mhcoll.char`, `mhcoll.bool` and `mhvar.char` are
    //     ALSO canonical on HotSpot — measured — and are written out verbatim
    //     in F29-1 NOMINATION 4b, held back because they would be red for a
    //     defect this file is not the gate for. They move here, and this
    //     family's denominator moves 8 → 11, when NOMINATION 2 lands.

    static void mhcollect() throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle idn = lk.findStatic(RJdkReflBox.class, "firstOf",
                MethodType.methodType(Object.class, Object[].class));
        MethodHandle coll = idn.asCollector(Object[].class, 1);
        ck("mhcoll.int", coll.invoke(I7) == Integer.valueOf(I7), true);
        ck("mhcoll.long", coll.invoke(J5) == Long.valueOf(J5), true);
        ck("mhcolloob.int1000", coll.invoke(I1000) == Integer.valueOf(I1000), false);
        MethodHandle var = idn.asVarargsCollector(Object[].class);
        ck("mhvar.int", var.invoke(I7) == Integer.valueOf(I7), true);

        // The wrapper CLASS, which is a different question from the identity
        // and is settled by the component here. MEASURED on HotSpot 25.0.3+9
        // (F29-1 §2). `instanceof` rather than `getClass().getName().equals`:
        // the dialect wants one boolean observable per CK line, and
        // `instanceof` is the narrowest thing that fails when the VM boxes a
        // `char` as an `Integer`. The `Id` row beside it keeps the identity
        // question asserted — neither row stands in for the other, and a VM
        // could pass either one alone.
        MethodHandle collC = lk.findStatic(RJdkReflBox.class, "firstOfChar",
                MethodType.methodType(Object.class, Character[].class))
                .asCollector(Character[].class, 1);
        ck("mhcollc.charClass", collC.invoke(CA) instanceof Character, true);
        ck("mhcollc.charId", collC.invoke(CA) == Character.valueOf(CA), true);
        MethodHandle collL = lk.findStatic(RJdkReflBox.class, "firstOfLong",
                MethodType.methodType(Object.class, Long[].class))
                .asCollector(Long[].class, 1);
        ck("mhcolll.longClass", collL.invoke(J5) instanceof Long, true);
        ck("mhcolll.longId", collL.invoke(J5) == Long.valueOf(J5), true);
        sectionEnd("mhcollect", 8);
    }

    /** Wrapper-typed collector targets — see the family header above. */
    public static Object firstOfChar(Character[] xs) {
        return xs[0];
    }

    public static Object firstOfLong(Long[] xs) {
        return xs[0];
    }

    // ---- 8. VarHandle, every shape ------------------------------------------
    //
    // Field (by index and static), array element, byte-array view, and the
    // read-modify-write modes, which hand back the PREVIOUS value and are
    // therefore boxing sites in their own right.

    static void varhandle() throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Holder h = new Holder();
        VarHandle vi = lk.findVarHandle(Holder.class, "i", int.class);
        VarHandle vc = lk.findVarHandle(Holder.class, "c", char.class);
        VarHandle vz = lk.findVarHandle(Holder.class, "z", boolean.class);
        VarHandle vj = lk.findVarHandle(Holder.class, "j", long.class);
        VarHandle vb = lk.findVarHandle(Holder.class, "b", byte.class);
        VarHandle vs = lk.findVarHandle(Holder.class, "s", short.class);
        VarHandle vf = lk.findVarHandle(Holder.class, "f", float.class);
        VarHandle vBig = lk.findVarHandle(Holder.class, "iBig", int.class);
        VarHandle vsi = lk.findStaticVarHandle(Holder.class, "si", int.class);
        VarHandle vsiBig = lk.findStaticVarHandle(Holder.class, "siBig", int.class);

        ck("vh.fieldInt", ((Object) vi.get(h)) == Integer.valueOf(I7), true);
        ck("vh.fieldChar", ((Object) vc.get(h)) == Character.valueOf(CA), true);
        ck("vh.fieldBool", ((Object) vz.get(h)) == Boolean.valueOf(ZT), true);
        ck("vh.fieldBoolTRUE", ((Object) vz.get(h)) == Boolean.TRUE, true);
        ck("vh.fieldLong", ((Object) vj.get(h)) == Long.valueOf(J5), true);
        ck("vh.fieldByte", ((Object) vb.get(h)) == Byte.valueOf(B3), true);
        ck("vh.fieldShort", ((Object) vs.get(h)) == Short.valueOf(S9), true);
        ck("vhnc.fieldFloat", ((Object) vf.get(h)) == Float.valueOf(F1), false);
        ck("vhoob.fieldInt1000", ((Object) vBig.get(h)) == Integer.valueOf(I1000), false);
        ck("vh.staticInt", ((Object) vsi.get()) == Integer.valueOf(I7), true);
        ck("vhoob.staticInt1000", ((Object) vsiBig.get()) == Integer.valueOf(I1000), false);

        // Read-modify-write: the value handed BACK is the old one, boxed.
        Holder rmw = new Holder();
        ck("vh.getAndSetInt", ((Object) vi.getAndSet(rmw, 42)) == Integer.valueOf(I7), true);
        // The `long` twin of the row above, and NOT a duplicate of it. The
        // RMW funnel is a fourth boxing site beside `VarHandle.get`'s three
        // field arms, and a fix that reads only `get` leaves it behind — three
        // of four looks like the whole set. `long` is the width that exposes
        // it: the raw slot can present as a compact int, so the wrapper comes
        // back carrying the wrong bits, which no `int` row can see. MEASURED
        // on HotSpot 25.0.3+9 (F29-1 §4): vh.getAndSetLong hands back 5, and
        // it is `== Long.valueOf(5)`.
        Holder rmwJ = new Holder();
        ck("vh.getAndSetLong", ((Object) vj.getAndSet(rmwJ, 42L)) == Long.valueOf(J5), true);
        Holder rmw2 = new Holder();
        ck("vh.compareAndExchangeInt",
                ((Object) vi.compareAndExchange(rmw2, 7, 42)) == Integer.valueOf(I7), true);

        VarHandle ai = MethodHandles.arrayElementVarHandle(int[].class);
        VarHandle ac = MethodHandles.arrayElementVarHandle(char[].class);
        VarHandle az = MethodHandles.arrayElementVarHandle(boolean[].class);
        VarHandle al = MethodHandles.arrayElementVarHandle(long[].class);
        // The contrast with `arrayget()` above: the SAME int[] element read
        // through a VarHandle is canonical and through Array.get is fresh.
        ck("vh.arrInt", ((Object) ai.get(new int[] {I7}, 0)) == Integer.valueOf(I7), true);
        ck("vh.arrChar", ((Object) ac.get(new char[] {CA}, 0)) == Character.valueOf(CA), true);
        ck("vh.arrBool", ((Object) az.get(new boolean[] {ZT}, 0)) == Boolean.valueOf(ZT), true);
        ck("vh.arrLong", ((Object) al.get(new long[] {J5}, 0)) == Long.valueOf(J5), true);
        ck("vhoob.arrInt1000",
                ((Object) ai.get(new int[] {I1000}, 0)) == Integer.valueOf(I1000), false);

        VarHandle bv = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.LITTLE_ENDIAN);
        byte[] raw4 = new byte[] {7, 0, 0, 0};
        ck("vh.byteViewInt", ((Object) bv.get(raw4, 0)) == Integer.valueOf(I7), true);
        VarHandle bvl = MethodHandles.byteArrayViewVarHandle(long[].class, ByteOrder.LITTLE_ENDIAN);
        byte[] raw8 = new byte[] {5, 0, 0, 0, 0, 0, 0, 0};
        ck("vh.byteViewLong", ((Object) bvl.get(raw8, 0)) == Long.valueOf(J5), true);
        sectionEnd("varhandle", 21);
    }

    // ---- 9. FFM layout VarHandle --------------------------------------------

    static void ffm() throws Throwable {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(16);
            VarHandle lvi = ValueLayout.JAVA_INT.varHandle();
            seg.set(ValueLayout.JAVA_INT, 0, I7);
            ck("ffm.layoutInt", ((Object) lvi.get(seg, 0L)) == Integer.valueOf(I7), true);
            seg.set(ValueLayout.JAVA_INT, 0, I1000);
            ck("ffmoob.layoutInt1000",
                    ((Object) lvi.get(seg, 0L)) == Integer.valueOf(I1000), false);
            seg.set(ValueLayout.JAVA_CHAR, 8, CA);
            ck("ffm.layoutChar",
                    ((Object) ValueLayout.JAVA_CHAR.varHandle().get(seg, 8L))
                            == Character.valueOf(CA), true);
            seg.set(ValueLayout.JAVA_LONG, 0, J5);
            ck("ffm.layoutLong",
                    ((Object) ValueLayout.JAVA_LONG.varHandle().get(seg, 0L))
                            == Long.valueOf(J5), true);
            seg.set(ValueLayout.JAVA_BYTE, 12, B3);
            ck("ffm.layoutByte",
                    ((Object) ValueLayout.JAVA_BYTE.varHandle().get(seg, 12L))
                            == Byte.valueOf(B3), true);
        }
        sectionEnd("ffm", 5);
    }

    // ---- 10. Proxy: primitive ARGS boxed into the handler's Object[] ---------
    //
    // A generated proxy class boxes each primitive argument with a `valueOf`
    // invocation in its own bytecode, so every in-bound row is canonical.
    // `proxy.boolTRUE` is the row Xerces' XML11Configuration depends on
    // (`fFeatures.get(…) == Boolean.TRUE` chooses its scanner on it).
    //
    // This family is also a ROUTE DISCRIMINATOR, which is why it is worth its
    // eight rows even though the whole file is about identity. CratonVM has
    // TWO argument-boxing implementations for proxies — the `X.valueOf`
    // bytecode `classloading/src/proxy_gen.rs` emits into every generated
    // `$ProxyN`, which is canonical by construction, and
    // `vm/src/vm/vm_exec.rs`'s `proxy_box_value_for_desc`, which is reached
    // through the interpreter's `is_proxy_dispatch` interception. Reading the
    // sources says the interception fires first and the emitted bytecode is
    // unreached; only a run settles it, and this family is that run.

    static void proxy() {
        final Object[][] seen = new Object[1][];
        Svc svc = (Svc) Proxy.newProxyInstance(Svc.class.getClassLoader(),
                new Class<?>[] {Svc.class},
                new InvocationHandler() {
                    @Override
                    public Object invoke(Object proxy, Method m, Object[] a) {
                        if ("take".equals(m.getName())) {
                            seen[0] = a;
                        }
                        return null;
                    }
                });
        svc.take(I7, CA, ZT, J5, B3, S9, I1000);
        Object[] a = seen[0];
        ck("proxy.int", a[0] == Integer.valueOf(I7), true);
        ck("proxy.char", a[1] == Character.valueOf(CA), true);
        ck("proxy.bool", a[2] == Boolean.valueOf(ZT), true);
        ck("proxy.boolTRUE", a[2] == Boolean.TRUE, true);
        ck("proxy.long", a[3] == Long.valueOf(J5), true);
        ck("proxy.byte", a[4] == Byte.valueOf(B3), true);
        ck("proxy.short", a[5] == Short.valueOf(S9), true);
        ck("proxyoob.int1000", a[6] == Integer.valueOf(I1000), false);
        sectionEnd("proxy", 8);
    }

    // ---- 11. Controls -------------------------------------------------------
    //
    // Every family above reads through some reflective machinery. These six
    // rows read through none of it, so they separate "the reflective path is
    // wrong" from "`==` on this VM has stopped discriminating" and from "the
    // caches themselves are wrong". If `controls` is red, nothing above it
    // means what it says.

    static void controls() {
        ck("neg.newObjects", new Object() == new Object(), false);
        ck("neg.floatValueOf", Float.valueOf(0f) == Float.valueOf(0f), false);
        ck("neg.doubleValueOf", Double.valueOf(0d) == Double.valueOf(0d), false);
        ck("ctrl.integerValueOfCached", Integer.valueOf(I7) == Integer.valueOf(I7), true);
        ck("ctrl.integerValueOfUncached", Integer.valueOf(I1000) == Integer.valueOf(I1000), false);
        // ByteCache covers all 256 values, so a `Byte` has no fresh arm at all
        // — which is why no `byte` row appears in `fieldfresh`.
        ck("ctrl.byteValueOfAll", Byte.valueOf((byte) -128) == Byte.valueOf((byte) -128), true);
        sectionEnd("controls", 6);
    }

    // -- driver --------------------------------------------------------------

    static final String[] FAMILIES = {
        "fieldget", "fieldstatic", "fieldfresh", "arrayget", "invoke", "mhreturn",
        "mhcollect", "varhandle", "ffm", "proxy", "controls",
    };

    /**
     * Run one family. A throwable is reported as a row and swallowed: the families after this one
     * carry evidence too, and a VM that dies in {@link #ffm()} must still be diffable on
     * {@link #proxy()} and {@link #controls()}.
     */
    static void runFamily(String name) {
        try {
            switch (name) {
                case "fieldget" -> fieldget();
                case "fieldstatic" -> fieldstatic();
                case "fieldfresh" -> fieldfresh();
                case "arrayget" -> arrayget();
                case "invoke" -> invoke();
                case "mhreturn" -> mhreturn();
                case "mhcollect" -> mhcollect();
                case "varhandle" -> varhandle();
                case "ffm" -> ffm();
                case "proxy" -> proxy();
                case "controls" -> controls();
                default -> {
                    fails++;
                    System.out.println("CK RJdkReflBox FAILED unknown-family " + name);
                }
            }
        } catch (Throwable t) {
            fails++;
            System.out.println(
                    "CK RJdkReflBox FAILED " + name + " threw=" + t.getClass().getName());
            // Re-sync the family denominator so the NEXT family's sectionEnd
            // still measures its own rows and not this one's shortfall.
            mark = checks;
        }
    }

    public static void main(String[] args) {
        String only = null;
        for (String arg : args) {
            if (arg.startsWith("--only=")) {
                only = arg.substring("--only=".length());
            } else if ("--list".equals(arg)) {
                for (String f : FAMILIES) {
                    System.out.println("CK RJdkReflBox family=" + f);
                }
                return;
            }
        }
        if (only == null) {
            for (String f : FAMILIES) {
                runFamily(f);
            }
        } else {
            System.out.println("CK RJdkReflBox only=" + only);
            runFamily(only);
        }
        // SEPARATE lines, and fails before checks. harness_check_count takes
        // the whole rest of the `checks=` line, so a combined line publishes a
        // non-numeric "count" and G3 silently no-ops inside its own 2>/dev/null.
        System.out.println("CK RJdkReflBox fails=" + fails);
        System.out.println("CK RJdkReflBox checks=" + checks);
        if (fails != 0) {
            throw new RuntimeException(fails + " reflective boxing identity checks failed");
        }
        System.out.println("PASS RJdkReflBox (" + checks + " checks)");
    }
}
