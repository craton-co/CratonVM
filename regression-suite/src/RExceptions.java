import java.util.*;

/**
 * Regression: exception semantics + array covariance. Covers ArrayStoreException
 * (the aastore check regressed once — wrongly rejecting valid covariant /
 * dynamic-proxy stores), try/finally ordering, cause chains, and
 * try-with-resources close ordering.
 */
public class RExceptions {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }
    // Same assertion, uncounted: used inside the JIT warm-up loops below, which
    // run thousands of iterations and would otherwise drown the check count the
    // harness reads off the PASS line.
    static void must(boolean c, String m) { if (!c) throw new AssertionError(m); }

    // B11. Counted like `check`, but RECORDS the divergence and prints it
    // instead of throwing at the first one.
    //
    // This exists because of what happened to row 7 of W7-37: the only cast
    // shape this fixture asserted was `String` -> `Integer`, the one shape that
    // works, so a whole family stayed broken while the record said "fixed". The
    // fix is to assert the whole family — but a family of assertions that dies
    // on its first member gives a taker one member per rebuild, and a rebuild
    // of this VM is not cheap. Every `expect` row therefore reports on stdout
    // in the SAME run, and `drainDivergences()` throws once at the end with all
    // of them. A red here is still a red; it is just a red that says everything
    // it knows the first time.
    static final List<String> DIVERGENCES = new ArrayList<>();
    static void expect(boolean c, String m) {
        checks++;
        if (!c) { DIVERGENCES.add(m); System.out.println("DIVERGENCE " + m); }
    }
    static void drainDivergences() {
        if (!DIVERGENCES.isEmpty()) {
            throw new AssertionError(DIVERGENCES.size() + " divergence(s): " + DIVERGENCES);
        }
    }

    // HotSpot 25 (jdk-25.0.3.9-hotspot), measured. Both operands live in the
    // same module, so `SharedRuntime::generate_class_cast_message` emits one
    // JOINT clause rather than two `; `-separated ones, and both operands carry
    // the `class ` prefix.
    static final String HOTSPOT_CCE =
        "class java.lang.String cannot be cast to class java.lang.Integer "
        + "(java.lang.String and java.lang.Integer are in module java.base of loader 'bootstrap')";

    // Static so nothing here can be constant-folded away before the store /
    // cast the two helpers below exist to execute.
    static final Object[] STORE_TARGET = new String[1];
    static final Object STORE_VALUE = Integer.valueOf(1);
    static final Object CAST_SOURCE = "s";

    static String arrayStoreMessage() {
        try { STORE_TARGET[0] = STORE_VALUE; return "no-throw"; }
        catch (ArrayStoreException e) { return String.valueOf(e.getMessage()); }
    }

    static String classCastMessage() {
        try { Integer i = (Integer) CAST_SOURCE; return "no-throw:" + i; }
        catch (ClassCastException e) { return String.valueOf(e.getMessage()); }
    }

    interface Animal {}
    static class Cat implements Animal {}
    static class Dog implements Animal {}

    // ---------------------------------------------------------------------
    // B11 — the `aastore` covariance rule, one shape per store site.
    //
    // `arrayStoreMessage()` above is a single site with a single shape, and a
    // single shape cannot tell "the check is gone" from "the check is wrong for
    // this one pair". These eleven cover the rule: five that MUST raise
    // ArrayStoreException and six that MUST NOT, including the two the JVMS
    // singles out (a `null` element is always storable; `Object[]` accepts any
    // reference). Each store is in its OWN method so each gets its own compiled
    // site — a shared helper would give all eleven one megamorphic site and
    // would measure the wrong thing.
    //
    // Operands are static fields read at their widest type, so javac emits a
    // plain `aastore` with no compile-time narrowing and nothing here folds.
    // ---------------------------------------------------------------------
    static final Object[] AS_STR_ARR    = new String[1];
    static final Object[] AS_NUM_ARR    = new Number[1];
    static final Object[] AS_IFACE_ARR  = new Animal[1];
    static final Object[] AS_OBJ_ARR    = new Object[1];
    static final Object[] AS_ARR_ARR    = new String[1][1];   // String[][] as Object[]
    static final Object[] AS_CAT_ARR    = new Cat[1];
    static final Object AS_V_INTEGER = Integer.valueOf(1);
    static final Object AS_V_STRING  = "s";
    static final Object AS_V_CAT     = new Cat();
    static final Object AS_V_DOG     = new Dog();
    static final Object AS_V_INTARR  = new Integer[1];
    static final Object AS_V_STRARR  = new String[1];
    static final Object AS_V_NULL    = null;

    // MUST raise ArrayStoreException.
    static String as1() { try { AS_STR_ARR[0]   = AS_V_INTEGER; return "no-throw"; } catch (ArrayStoreException e) { return "ASE"; } }
    static String as2() { try { AS_NUM_ARR[0]   = AS_V_STRING;  return "no-throw"; } catch (ArrayStoreException e) { return "ASE"; } }
    static String as3() { try { AS_IFACE_ARR[0] = AS_V_STRING;  return "no-throw"; } catch (ArrayStoreException e) { return "ASE"; } }
    static String as4() { try { AS_ARR_ARR[0]   = AS_V_INTARR;  return "no-throw"; } catch (ArrayStoreException e) { return "ASE"; } }
    static String as5() { try { AS_CAT_ARR[0]   = AS_V_DOG;     return "no-throw"; } catch (ArrayStoreException e) { return "ASE"; } }
    // MUST NOT raise.
    static String as6()  { try { AS_OBJ_ARR[0] = AS_V_INTEGER; return "no-throw"; } catch (ArrayStoreException e) { return "ASE"; } }
    static String as7()  { try { AS_STR_ARR[0] = AS_V_NULL;    return "no-throw"; } catch (ArrayStoreException e) { return "ASE"; } }
    static String as8()  { try { AS_IFACE_ARR[0] = AS_V_CAT;   return "no-throw"; } catch (ArrayStoreException e) { return "ASE"; } }
    static String as9()  { try { AS_NUM_ARR[0] = AS_V_INTEGER; return "no-throw"; } catch (ArrayStoreException e) { return "ASE"; } }
    static String as10() { try { AS_ARR_ARR[0] = AS_V_STRARR;  return "no-throw"; } catch (ArrayStoreException e) { return "ASE"; } }
    static String as11() { try { AS_STR_ARR[0] = AS_V_STRING;  return "no-throw"; } catch (ArrayStoreException e) { return "ASE"; } }

    // The bulk twin of the same JVMS rule. `System.arraycopy` between two
    // reference arrays must raise ArrayStoreException on the first element that
    // is not assignable to the destination's component type. Measured green on
    // both VMs in both tiers — it is scheduled anyway, because `aastore`,
    // `Array.set` and `arraycopy` are one rule implemented three times, two of
    // the three are currently correct, and the way a shared predicate gets
    // weakened to fix the third is with nothing watching the other two.
    static final Object[] AC_SRC = { Integer.valueOf(1) };
    static final Object[] AC_DST = new String[1];
    static String arrayCopyCovariant() {
        try { System.arraycopy(AC_SRC, 0, AC_DST, 0, 1); return "no-throw"; }
        catch (ArrayStoreException e) { return "ASE"; }
    }

    static String asShape(int k) {
        switch (k) {
            case 0: return as1();  case 1: return as2();  case 2: return as3();
            case 3: return as4();  case 4: return as5();  case 5: return as6();
            case 6: return as7();  case 7: return as8();  case 8: return as9();
            case 9: return as10(); default: return as11();
        }
    }
    static final String[] AS_NAMES = {
        "String[]<-Integer", "Number[]<-String", "Animal[]<-String",
        "String[][]<-Integer[]", "Cat[]<-Dog",
        "Object[]<-Integer", "String[]<-null", "Animal[]<-Cat",
        "Number[]<-Integer", "String[][]<-String[]", "String[]<-String",
    };
    // Index < 5 must throw; index >= 5 must not.
    static final int AS_FIRST_LEGAL = 5;

    // ---------------------------------------------------------------------
    // B11 / W7-37 row 7 — the four cast shapes with an ARRAY operand.
    //
    // Measured on HotSpot 25 (jdk-25.0.3.9-hotspot), 2026-08-12. All four
    // operands are in java.base under the bootstrap loader, so all four take
    // the JOINT parenthetical, exactly like the `String` -> `Integer` row
    // above. `HOTSPOT_CCE` is the *non-array* control that already passes; the
    // point of these four is that the rewrite which produces it is skipped
    // whenever EITHER operand is an array.
    // ---------------------------------------------------------------------
    static final String HS_CCE_OBJ_TO_REFARR =
        "class java.lang.String cannot be cast to class [Ljava.lang.String; "
        + "(java.lang.String and [Ljava.lang.String; are in module java.base of loader 'bootstrap')";
    static final String HS_CCE_PRIMARR_TO_OBJ =
        "class [I cannot be cast to class java.lang.String "
        + "([I and java.lang.String are in module java.base of loader 'bootstrap')";
    static final String HS_CCE_PRIMARR_TO_REFARR =
        "class [I cannot be cast to class [Ljava.lang.String; "
        + "([I and [Ljava.lang.String; are in module java.base of loader 'bootstrap')";
    static final String HS_CCE_REFARR_TO_REFARR =
        "class [Ljava.lang.Integer; cannot be cast to class [Ljava.lang.String; "
        + "([Ljava.lang.Integer; and [Ljava.lang.String; are in module java.base of loader 'bootstrap')";

    static final Object CAST_INTARR  = new int[1];
    static final Object CAST_INTGARR = new Integer[1];
    static Object CAST_SINK;

    static String castObjToRefArr()     { try { CAST_SINK = (String[]) CAST_SOURCE;  return "no-throw"; } catch (ClassCastException e) { return String.valueOf(e.getMessage()); } }
    static String castPrimArrToObj()    { try { CAST_SINK = (String)   CAST_INTARR;  return "no-throw"; } catch (ClassCastException e) { return String.valueOf(e.getMessage()); } }
    static String castPrimArrToRefArr() { try { CAST_SINK = (String[]) CAST_INTARR;  return "no-throw"; } catch (ClassCastException e) { return String.valueOf(e.getMessage()); } }
    static String castRefArrToRefArr()  { try { CAST_SINK = (String[]) CAST_INTGARR; return "no-throw"; } catch (ClassCastException e) { return String.valueOf(e.getMessage()); } }

    static class Res implements AutoCloseable {
        final String id; final List<String> log;
        Res(String id, List<String> log) { this.id = id; this.log = log; }
        public void close() { log.add("close " + id); }
    }

    public static void main(String[] a) throws Exception {
        // ---- ArrayStoreException: storing an incompatible type throws ----
        Object[] strs = new String[2];
        boolean ase = false;
        try { strs[0] = Integer.valueOf(1); } catch (ArrayStoreException e) { ase = true; }
        check(ase, "ArrayStoreException on incompatible store");

        // ---- ...but valid covariant stores (incl. interface[] / subtype) must NOT throw ----
        Animal[] animals = new Animal[2];
        animals[0] = new Cat(); animals[1] = new Dog();   // subtype into interface[]
        check(animals[0] instanceof Cat, "covariant interface[] store");
        Object[] objs = new Object[2];
        objs[0] = "any"; objs[1] = 42;                     // Object[] accepts anything
        Number[] nums = new Number[2];
        nums[0] = Integer.valueOf(1); nums[1] = Double.valueOf(2.0);  // subtype into superclass[]
        check(nums[0].intValue() == 1, "covariant superclass[] store");
        // Annotation proxies are dynamic implementations of an annotation interface —
        // storing one into an annotation/Object[] must be allowed (regressed once).
        Override ann = RExceptions.class.getAnnotation(Override.class); // null, but type resolves
        java.lang.annotation.Annotation[] anns = RExceptions.class.getAnnotations();
        Object[] holder = new java.lang.annotation.Annotation[anns.length];
        System.arraycopy(anns, 0, holder, 0, anns.length);
        check(true, "annotation array copy");

        // ---- try / catch / finally ordering ----
        List<String> order = new ArrayList<>();
        try { order.add("try"); throw new IllegalStateException("x"); }
        catch (IllegalStateException e) { order.add("catch:" + e.getMessage()); }
        finally { order.add("finally"); }
        check(order.equals(Arrays.asList("try", "catch:x", "finally")), "try/catch/finally order");

        // ---- NPE + AIOOBE + arithmetic + CCE ----
        boolean npe = false; try { String s = null; s.length(); } catch (NullPointerException e) { npe = true; }
        check(npe, "NullPointerException");
        boolean aioobe = false; try { int[] x = new int[2]; int y = x[5]; } catch (ArrayIndexOutOfBoundsException e) { aioobe = true; }
        check(aioobe, "ArrayIndexOutOfBoundsException");
        boolean arith = false; try { int z = 1 / (a.length); } catch (ArithmeticException e) { arith = true; }
        check(arith, "ArithmeticException divide-by-zero");
        boolean cce = false; try { Object o = "s"; Integer i = (Integer) o; } catch (ClassCastException e) { cce = true; }
        check(cce, "ClassCastException");

        // ---- cause chain ----
        Exception root = new IllegalArgumentException("root");
        Exception wrap = new RuntimeException("wrap", root);
        check(wrap.getCause() == root && "root".equals(wrap.getCause().getMessage()), "cause chain");

        // ---- multi-catch ----
        String caught = null;
        try { throw new java.io.IOException("io"); }
        catch (RuntimeException | java.io.IOException e) { caught = e.getMessage(); }
        check("io".equals(caught), "multi-catch");

        // ---- try-with-resources: close in reverse order, even on exception ----
        List<String> rlog = new ArrayList<>();
        boolean tw = false;
        try (Res r1 = new Res("1", rlog); Res r2 = new Res("2", rlog)) {
            rlog.add("body"); throw new RuntimeException("boom");
        } catch (RuntimeException e) { tw = true; }
        check(tw && rlog.equals(Arrays.asList("body", "close 2", "close 1")), "try-with-resources order");

        // ---- `return x` captures the value before finally runs (returns 1, not 2) ----
        check(returnsAfterFinally() == 1, "finally does not alter already-evaluated return");

        // ---- VM-minted type-error messages, and tier-independence of them ----
        // W7-37. Both strings are generated by the VM itself, so nothing else
        // in the system can get them right, and both are parsed downstream: the
        // slashed INTERNAL name this once printed made mockk's JvmAutoHinter
        // regex capture `String` rather than `java.lang.String` and hand that to
        // Class.forName. The interpreter raises both through the VM's single
        // RuntimeError funnel, which is where HotSpot's wording is applied; the
        // two JIT helpers built the throwable themselves and never reached it,
        // so the SAME store printed different text depending on whether the
        // enclosing method had tiered up. The loops below run each site past the
        // JIT invocation threshold (default 500) and require the text not to
        // move -- that comparison is the only assertion here that can see a
        // JIT/interpreter split, and it holds on HotSpot trivially.
        String aseCold = arrayStoreMessage();
        check("java.lang.Integer".equals(aseCold),
              "ArrayStoreException names the external class name, got: " + aseCold);
        String cceCold = classCastMessage();
        // Two checks, not one, so a red localises: the first is the `class `
        // prefix on both operands plus the presence of the parenthetical -- the
        // half that fails on the bare `X cannot be cast to Y` form -- and the
        // second is full byte parity with the parenthetical's own wording.
        check(cceCold.startsWith("class java.lang.String cannot be cast to class java.lang.Integer ("),
              "ClassCastException has HotSpot's `class ` prefixes and a loader clause, got: " + cceCold);
        check(HOTSPOT_CCE.equals(cceCold),
              "ClassCastException matches HotSpot byte for byte, got: " + cceCold);
        // Capture the hot reading into a local BEFORE comparing, and quote both
        // sides in the failure. The bare "text moved" this used to throw cost a
        // taker a whole side probe to learn WHICH way it moved, and the answer
        // turned out not to be a wording drift at all: the JIT-compiled arm read
        // back `no-throw`, i.e. the compiled `aastore` performed the store and
        // raised nothing. A tier-parity assertion has two failure modes -- a
        // different message, and no message -- and only the observed value tells
        // them apart.
        // The message is built only on the failing iteration: `must`'s argument
        // is evaluated eagerly, and 2400 string concatenations inside the very
        // loop whose job is to tier these two helpers up is warm-up the loop
        // does not want to be doing.
        // ---- B11: the four ARRAY-operand cast shapes (W7-37 row 7) ----
        // These run BEFORE the tier-parity loop below, and report via `expect`
        // rather than throwing, so a single run shows every one of them
        // alongside whatever the loop does. Row 7 was recorded as fixed twice
        // while only the one working shape was scheduled; four shapes that all
        // report is what makes it adjudicable.
        //
        // Read in BOTH tiers, and that is not belt-and-braces. Measured
        // 2026-08-12, CratonVM emits THREE different strings for an array
        // operand depending on tier and on whether the array class happens to
        // be in the definition index at that moment: the bare
        // `X cannot be cast to Y`, a prefix-only `class X cannot be cast to
        // class Y` with no parenthetical, and (for one shape, reproducibly)
        // HotSpot's full string. A single cold reading would have called some
        // of these fixed. Applications regex this text, so a message that is
        // not a function of the cast alone is the defect, not a symptom of it.
        String[] ccName = { "String->String[]", "int[]->String", "int[]->String[]", "Integer[]->String[]" };
        String[] ccWant = { HS_CCE_OBJ_TO_REFARR, HS_CCE_PRIMARR_TO_OBJ,
                            HS_CCE_PRIMARR_TO_REFARR, HS_CCE_REFARR_TO_REFARR };
        String[] ccCold = { castObjToRefArr(), castPrimArrToObj(),
                            castPrimArrToRefArr(), castRefArrToRefArr() };
        String[] ccHot = new String[4];
        for (int i = 0; i < 1200; i++) {
            ccHot[0] = castObjToRefArr();     ccHot[1] = castPrimArrToObj();
            ccHot[2] = castPrimArrToRefArr(); ccHot[3] = castRefArrToRefArr();
        }
        for (int k = 0; k < 4; k++) {
            expect(ccWant[k].equals(ccCold[k]),
                   "CCE " + ccName[k] + " interpreted matches HotSpot, got: " + ccCold[k]);
            expect(ccWant[k].equals(ccHot[k]),
                   "CCE " + ccName[k] + " JIT-compiled matches HotSpot, got: " + ccHot[k]
                   + " (interpreted reading was: " + ccCold[k] + ")");
        }

        // ---- B11: the `aastore` covariance rule, all eleven shapes, both tiers ----
        // The single-shape `arrayStoreMessage()` assertion below can only say
        // that SOMETHING is wrong. This says which shapes, and — because each
        // shape is read once cold and once past the JIT threshold — whether the
        // interpreter or only the compiler is at fault. On a correct VM every
        // cell reads its MUST value in both tiers; that is trivially true on
        // HotSpot, so this cannot flake the oracle arm.
        String[] asCold = new String[AS_NAMES.length];
        String[] asHot  = new String[AS_NAMES.length];
        for (int k = 0; k < AS_NAMES.length; k++) asCold[k] = asShape(k);
        for (int i = 0; i < 1200; i++) {
            for (int k = 0; k < AS_NAMES.length; k++) {
                String r = asShape(k);
                if (i == 1199) asHot[k] = r;
            }
        }
        String acCold = arrayCopyCovariant(), acHot = null;
        for (int i = 0; i < 1200; i++) acHot = arrayCopyCovariant();
        expect("ASE".equals(acCold), "arraycopy Object[]{Integer}->String[] interpreted: want ASE, got " + acCold);
        expect("ASE".equals(acHot),  "arraycopy Object[]{Integer}->String[] JIT-compiled: want ASE, got " + acHot);
        check(AC_DST[0] == null, "a refused arraycopy leaves the destination element untouched");

        for (int k = 0; k < AS_NAMES.length; k++) {
            String want = k < AS_FIRST_LEGAL ? "ASE" : "no-throw";
            expect(want.equals(asCold[k]),
                   "aastore " + AS_NAMES[k] + " interpreted: want " + want + ", got " + asCold[k]);
            expect(want.equals(asHot[k]),
                   "aastore " + AS_NAMES[k] + " JIT-compiled: want " + want + ", got " + asHot[k]
                   + " (interpreted reading was " + asCold[k] + ")");
        }

        for (int i = 0; i < 1200; i++) {
            String aseHot = arrayStoreMessage();
            if (!aseCold.equals(aseHot)) {
                must(false, "ArrayStoreException text moved during warm-up at i="
                     + i + ": cold=[" + aseCold + "] hot=[" + aseHot + "]");
            }
            String cceHot = classCastMessage();
            if (!cceCold.equals(cceHot)) {
                must(false, "ClassCastException text moved during warm-up at i="
                     + i + ": cold=[" + cceCold + "] hot=[" + cceHot + "]");
            }
        }
        check(aseCold.equals(arrayStoreMessage()),
              "ArrayStoreException text is the same interpreted and JIT-compiled");
        check(cceCold.equals(classCastMessage()),
              "ClassCastException text is the same interpreted and JIT-compiled");

        // ---- Class.forName on an array whose ELEMENT is absent ----
        // L16. Two separate contracts, and applications branch on both.
        // (1) The SHAPE: JVMS 5.3.3 builds an array class from its element
        //     type, so an absent element means the requested thing is absent --
        //     a ClassNotFoundException, not the NoClassDefFoundError a link-time
        //     resolution failure produces. The two are in different hierarchies
        //     (checked Exception vs Error), so getting it wrong sails straight
        //     through `catch (ClassNotFoundException)`.
        // (2) The MESSAGE: HotSpot never hands an array descriptor to a loader,
        //     so it names the ELEMENT. Measured on JDK 25:
        //       Class.forName("[Lp.X;")  -> CNFE msg="p.X" cause=null
        //       Class.forName("[[Lp.X;") -> CNFE msg="p.X" cause=null
        //     Caught as Throwable rather than as ClassNotFoundException so a
        //     regression on (1) reports the type it got instead of dying
        //     uncaught.
        String absent = "com.cratonvm.absent.NoSuchClass20260812";
        Throwable arr1 = null;
        try { Class.forName("[L" + absent + ";"); } catch (Throwable t) { arr1 = t; }
        check(arr1 instanceof ClassNotFoundException,
              "Class.forName(\"[L<absent>;\") throws ClassNotFoundException, got: " + arr1);
        check(arr1 != null && absent.equals(arr1.getMessage()),
              "...naming the element, not the descriptor, got: " + (arr1 == null ? "null" : arr1.getMessage()));
        Throwable arr2 = null;
        try { Class.forName("[[L" + absent + ";"); } catch (Throwable t) { arr2 = t; }
        check(arr2 instanceof ClassNotFoundException && absent.equals(arr2.getMessage()),
              "every dimension is stripped, got: " + arr2);
        // Controls. Array resolution itself must be untouched by the above --
        // if either of these fails, the element-vs-descriptor question is beside
        // the point and array resolution is broken far more generally.
        // Asserted by NAME rather than by mirror identity: identity would be a
        // second, unrelated claim (that array mirrors are canonical), and a
        // failure there would not mean what this control is asking about.
        check("[I".equals(Class.forName("[I").getName()),
              "Class.forName resolves a primitive array");
        check("[Ljava.lang.String;".equals(Class.forName("[Ljava.lang.String;").getName()),
              "Class.forName resolves a reference array whose element exists");

        // ---- Stack's own exception type ----
        // W7-33 R2. java.util.EmptyStackException extends RuntimeException
        // DIRECTLY -- it is not a NoSuchElementException -- so raising the
        // latter means `catch (EmptyStackException)` in application code never
        // fires. pop() and peek() are one refusal in the JDK (pop calls peek),
        // and leaving one behind is how the pair drifts apart again.
        boolean ese = false, eseWrongType = false;
        try { new Stack<String>().pop(); }
        catch (EmptyStackException e) { ese = true; }
        catch (NoSuchElementException e) { eseWrongType = true; }
        check(ese && !eseWrongType, "Stack.pop() on empty throws EmptyStackException");
        boolean esePeek = false, esePeekWrongType = false;
        try { new Stack<String>().peek(); }
        catch (EmptyStackException e) { esePeek = true; }
        catch (NoSuchElementException e) { esePeekWrongType = true; }
        check(esePeek && !esePeekWrongType, "Stack.peek() on empty throws EmptyStackException");

        // Every `expect` divergence recorded above becomes one AssertionError
        // here, listing all of them. Nothing reaches this line unless the
        // throwing `check`s all passed, so a PASS still means PASS.
        drainDivergences();
        System.out.println("PASS RExceptions (" + checks + " checks)");
    }

    static int returnsAfterFinally() {
        int x = 1;
        try { return x; } finally { x = 2; /* does not change the already-evaluated return */ }
    }
}
