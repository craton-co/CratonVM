// JIT round 21 wave 2, lane ea: escape analysis, scalar replacement, lock elision and deopt
// rematerialisation -- correctness facts for every shape the lane reviewed, plus time rows.
//
// Every `row` method is called WARM times with `rare == false` (so both tiers compile it, and the
// rare arm is never-taken, i.e. an uncommon trap / pruned branch in an optimizing compile), then
// once with `rare == true`. The rare arm forces a transfer to the interpreter in the middle of the
// object's life: it loads and initialises a class nothing has touched (`LateN`), and in the CHA
// rows it loads a second subclass of an abstract class whose only implementor was devirtualised
// (`Circ`), invalidating the running compiled frame. After the transfer the interpreter must see
// the object exactly as the bytecode left it: same identity, same field values, same lock state.
// Each row prints the rare call's answer and a checksum over the warm calls.
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21EaRows.java):
//   P=tools/probes/out
//   cratonvm -Xmx512m -cp $P R21EaRows                       # jdk-only (launcher default)
//   cratonvm --compatible -Xmx512m -cp $P R21EaRows          # both modes, same lines
//   java -Xmx512m -cp $P R21EaRows                           # HotSpot 25 (C2, EA on by default)
// Arms (cratonvm, each must print the same non-`time` lines):
//   CRATONVM_JIT_EA_REF_COMPARE_FOLD=1      EA21-1 (opt-in): `acmp` of a confined allocation
//                                           folds; rows ref-eq-* / time ref-compare-fresh move
//   CRATONVM_JIT_EA_LOOP_CARRIED_FIELDS=0   kill switch of the r20 loop-carried fields (control)
//   CRATONVM_JIT_IR_BOX_PHI_WEB_FOLD=1      EA21-2 (wave 3, opt-in): boxed accumulators through φs;
//                                           row box-phi-deopt (rare=101: cached Integer 6 is THE instance, Long 1003 is not cached) / time boxed-sum move
//   CRATONVM_JIT_EA_FRESH_TYPECHECK_FOLD=1  EA21-4 (wave 3, opt-in): instanceof/checkcast of an
//                                           exact allocation; row typecheck-fresh (rare=101)
//   CRATONVM_DISABLE_SCALAR_REPLACEMENT=1   with wave 3 also the IR tier (EA21-3); add
//   CRATONVM_JIT_IR_HONOUR_DISABLE_SCALAR_REPLACEMENT=0 for the old single-pass-only split
//   CRATONVM_DISABLE_SCALAR_REPLACEMENT=1   single-pass tier keeps every allocation (control)
//   --nojit                                 interpreter reference
// Engagement (NOT for timing): CRATONVM_DBG_SCALAR_NEW=1 prints `[cratonvm-scalarnew] <method>:
// scalar-replaced N/M alloc(s)` and per-allocation verdicts (`refused IdentityObserved` on the
// ref-eq rows without EA21-1, `REPLACED` with it); CRATONVM_DBG_SCALAR_DEOPT=1 prints
// `[DBG_SCALAR_DEOPT] MATERIALIZE` when a rare arm rebuilt an elided object; CRATONVM_DBG_JITC=1
// with the IR stage report prints `[ir] ea ref-compare fold: N compare(s)`.
//
// HotSpot expectation: every non-`time` line identical (the `fin` row is best-effort on both: it
// prints whether at least one finalizer ran after five gc + runFinalization rounds, which HotSpot 25
// answers `true`). Time rows under C2: boxed-sum ~1-3 ns, foreach-arraylist-16 ~10-20 ns per 16-element walk,
// optional-chain ~1-3 ns, record-in-loop ~1 ns, sb-local ~20-40 ns, ref-compare-fresh ~1 ns.

import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.function.IntSupplier;
import java.util.function.Supplier;

public class R21EaRows {
    static final int WARM = 40_000;

    static final class P {
        int x;
        int y;
        P next;

        P() {}
    }

    static final class Carrier extends RuntimeException {
        final P p;

        Carrier(P p) {
            super("carrier", null, false, false);
            this.p = p;
        }
    }

    abstract static class Shape {
        abstract int area();
    }

    static final class Sq extends Shape {
        int area() {
            return 4;
        }
    }

    // Loaded only by `midLoopCha`'s rare arm (`Class.forName`): the second `Shape` subclass, whose
    // loading invalidates every compiled body that devirtualised `SHAPE.area()` to `Sq`.
    static final class Circ extends Shape {
        int area() {
            return 9;
        }
    }

    static Shape SHAPE = new Sq();

    // Classes the rare arms load for the first time (one each, so every rare arm really loads).
    static final class Late1 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late2 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late3 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late4 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late5 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late6 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late7 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late8 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late9 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late10 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late11 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late12 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late13 { static int hits; static { hits = 1; } static void touch() { hits++; } }
    static final class Late14 { static int hits; static { hits = 1; } static void touch() { hits++; } }

    // A class whose <clinit> has a visible side effect; first `new` only on a rare arm.
    static int clinitRuns;

    static final class SideInit {
        static {
            clinitRuns++;
        }

        int v;
    }

    // Finalizable class: allocations must stay real.
    static volatile int finalized;

    static final class Fin {
        int v;

        @Override
        @SuppressWarnings({"deprecation", "removal"})
        protected void finalize() {
            finalized++;
        }
    }

    static Object staticHolder;
    static final Object[] ARR = new Object[4];
    static volatile Object sink;

    // ---------------------------------------------------------------- correctness rows

    // An object published by an array store, then mutated: the array must see the write.
    static int arrayStore(int i, boolean rare) {
        P p = new P();
        ARR[i & 3] = p;
        p.x = i;
        if (rare) {
            Late1.touch();
        }
        return ((P) ARR[i & 3]).x - i + (ARR[i & 3] == p ? 1 : 0);
    }

    // An object published by a static store, then mutated.
    static int staticStore(int i, boolean rare) {
        P p = new P();
        staticHolder = p;
        p.x = i;
        if (rare) {
            Late2.touch();
        }
        return ((P) staticHolder).x - i + (staticHolder == p ? 1 : 0);
    }

    static void mayThrow(boolean rare, P p) {
        if (rare) {
            throw new Carrier(p);
        }
    }

    // A call argument that escapes only on the exception edge.
    static int exceptionEdge(int i, boolean rare) {
        P p = new P();
        p.x = i;
        try {
            mayThrow(rare, p);
        } catch (Carrier c) {
            c.p.x += 5;
            return (c.p == p ? 1000 : 0) + p.x - i;
        }
        return p.x - i;
    }

    // A φ through a loop back edge: `prev` is the previous iteration's object.
    static int loopPhi(int i, boolean rare) {
        P cur = new P();
        cur.x = i;
        P prev = null;
        for (int k = 0; k < 3; k++) {
            prev = cur;
            cur = new P();
            cur.x = prev.x + 1;
            if (rare && k == 1) {
                Late3.touch();
            }
        }
        return cur.x - i + (prev == cur ? 100 : 0) + (prev.x == cur.x - 1 ? 10 : 0);
    }

    // A monitor on a local object with notify / holdsLock; notify outside the lock must throw.
    static int monitorNotify(int i, boolean rare) {
        Object o = new Object();
        int r = 0;
        synchronized (o) {
            o.notify();
            r += Thread.holdsLock(o) ? 1 : 0;
        }
        r += Thread.holdsLock(o) ? 10 : 0;
        if (rare) {
            try {
                o.notifyAll();
            } catch (IllegalMonitorStateException e) {
                r += 100;
            }
        }
        return r;
    }

    // A lock elided on a confined object, with a transfer INSIDE the locked region.
    static int lockedDeopt(int i, boolean rare) {
        Object o = new Object();
        int r = 0;
        synchronized (o) {
            r += i & 1;
            if (rare) {
                Late4.touch();
                r += Thread.holdsLock(o) ? 1000 : 0;
            }
        }
        r += Thread.holdsLock(o) ? 10_000 : 0;
        return r - (i & 1);
    }

    // An exception thrown out of a locked region on a confined object.
    static int lockedThrow(int i, boolean rare) {
        Object o = new Object();
        int r = 0;
        try {
            synchronized (o) {
                r += 1;
                if (rare) {
                    throw new IllegalStateException("x");
                }
            }
        } catch (IllegalStateException e) {
            r += 10;
        }
        return r + (Thread.holdsLock(o) ? 100 : 0);
    }

    // Identity hash before and after a transfer must be one value.
    static int identityHash(int i, boolean rare) {
        P p = new P();
        p.x = i;
        int h = System.identityHashCode(p);
        if (rare) {
            Late5.touch();
        }
        int r = (h == System.identityHashCode(p) ? 1 : 0) + (p.hashCode() == h ? 10 : 0);
        return r + p.x - i;
    }

    // Reference equality of two fresh objects and of an alias.
    static int refEqFresh(int i, boolean rare) {
        P a = new P();
        P b = new P();
        a.x = i;
        b.x = i;
        P c = rare ? a : b;
        if (rare) {
            Late6.touch();
        }
        return (a == b ? 1 : 0) + (a == c ? 10 : 0) + (b == c ? 100 : 0) + a.x + b.x - 2 * i;
    }

    // A fresh object against a parameter (never equal) and against itself through a φ.
    static int refEqParam(int i, Object q, boolean rare) {
        P a = new P();
        a.x = i;
        Object r = rare ? a : q;
        return (a == q ? 1 : 0) + (a == r ? 10 : 0) + a.x - i;
    }

    // A local object captured by a lambda.
    static int lambdaCapture(int i, boolean rare) {
        P p = new P();
        p.x = i;
        IntSupplier s = () -> p.x;
        Supplier<P> g = () -> p;
        if (rare) {
            Late7.touch();
            p.y = 3;
        }
        return s.getAsInt() - i + (g.get() == p ? 10 : 0) + g.get().y;
    }

    // `new` of a class not initialised yet, first reached on the rare arm: <clinit> must run.
    static int clinitSideEffect(int i, boolean rare) {
        if (rare) {
            SideInit s = new SideInit();
            s.v = i;
            return s.v - i + clinitRuns;
        }
        return 0;
    }

    // Field values of a loop-carried object after a transfer mid-loop.
    static int midLoop(int n, boolean rare) {
        P acc = new P();
        for (int j = 0; j < n; j++) {
            acc.x += j;
            acc.y ^= j * 7;
            if (rare && j == n / 2) {
                Late8.touch();
            }
        }
        return acc.x * 31 + acc.y;
    }

    // Same, invalidated by CHA: `SHAPE.area()` is devirtualised to Sq until `Circ` loads.
    static int midLoopCha(int n, boolean rare) {
        P acc = new P();
        for (int j = 0; j < n; j++) {
            acc.x += SHAPE.area();
            acc.y += j;
            if (rare && j == n / 2) {
                loadCirc();
            }
        }
        return acc.x * 31 + acc.y;
    }

    static void loadCirc() {
        try {
            Class.forName("R21EaRows$Circ");
        } catch (ClassNotFoundException e) {
            throw new AssertionError(e);
        }
    }

    // Nested virtual objects.
    static int nested(int i, boolean rare) {
        P outer = new P();
        P inner = new P();
        outer.next = inner;
        inner.x = i;
        outer.x = i + 1;
        if (rare) {
            Late9.touch();
        }
        return outer.next.x + outer.x - 2 * i + (outer.next == inner ? 1000 : 0);
    }

    // A small array read with a variable index.
    static int arrayVarIndex(int i, boolean rare) {
        int[] a = new int[4];
        a[0] = i;
        a[1] = i + 1;
        a[2] = i + 2;
        a[3] = i + 3;
        if (rare) {
            Late10.touch();
        }
        return a[i & 3] - i - (i & 3);
    }

    // Load before store and branchy stores.
    static int loadBeforeStore(int i, boolean rare) {
        P p = new P();
        int a = p.x;
        p.x = 42;
        if ((i & 1) == 0) {
            p.y = 1;
        } else {
            p.y = 2;
        }
        if (rare) {
            Late11.touch();
        }
        return a + p.x + p.y - ((i & 1) == 0 ? 1 : 2);
    }

    // A self reference read back through its own field.
    static int selfRef(int i, boolean rare) {
        P p = new P();
        p.next = p;
        p.next.x = i;
        if (rare) {
            Late12.touch();
        }
        return p.x - i + (p.next == p ? 1 : 0);
    }

    // The Integer cache identity (JLS 5.1.7, default AutoBoxCacheMax).
    static int boxIdentity(int i, boolean rare) {
        int v = rare ? 127 : (i & 63);
        Integer a = v;
        Integer b = v;
        Integer c = v + 1000;
        Integer d = v + 1000;
        return (a == b ? 1 : 0) + (c == d ? 10 : 0) + (a.equals(b) ? 100 : 0) + (c.equals(d) ? 1000 : 0);
    }

    // Wave 3 (EA21-2): a boxed accumulator carried through a loop φ, with a transfer
    // mid-loop. After it the box must be THE cached instance for a cached value (HotSpot's
    // get_cached_box), and a fresh one past the cache.
    static int boxPhiDeopt(int i, boolean rare) {
        Integer s = 0;
        int r = 0;
        int n = 8 + (i & 3);
        for (int j = 0; j < n; j++) {
            s += j;
            if (rare && j == 3) {
                Late13.touch();
                r += (s == Integer.valueOf(6)) ? 1 : 0;
            }
        }
        Long l = 1000L;
        for (int j = 0; j < n; j++) {
            l += j;
            if (rare && j == 2) {
                Late14.touch();
                r += (l == Long.valueOf(1003L)) ? 10 : 0;
                r += l.equals(1003L) ? 100 : 0;
            }
        }
        return r + s - n * (n - 1) / 2 + (int) (l - 1000L - n * (n - 1) / 2);
    }

    // Wave 3 (EA21-4): instanceof / checkcast of a fresh object of an exact class.
    static int typecheckFresh(int i, boolean rare) {
        Object o = new P();
        int r = 0;
        if (o instanceof P q) {
            q.x = i;
            r += 1;
        }
        r += (o instanceof Runnable) ? 10 : 0;
        r += ((P) o).x - i;
        if (rare) {
            r += (o instanceof Object) ? 100 : 0;
        }
        return r;
    }

    // Finalizable allocations that never escape.
    static int finRow(int i) {
        Fin f = new Fin();
        f.v = i;
        return f.v - i;
    }

    // ---------------------------------------------------------------- driver

    interface Row {
        int run(int i, boolean rare);
    }

    static void row(String name, Row r) {
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += r.run(i, false);
        }
        int rare = r.run(WARM, true);
        System.out.println("row " + name + " rare=" + rare + " warm=" + sum);
    }

    public static void main(String[] args) throws Exception {
        row("array-store", R21EaRows::arrayStore);
        row("static-store", R21EaRows::staticStore);
        row("exception-edge", R21EaRows::exceptionEdge);
        row("loop-phi", R21EaRows::loopPhi);
        row("monitor-notify", R21EaRows::monitorNotify);
        row("locked-deopt", R21EaRows::lockedDeopt);
        row("locked-throw", R21EaRows::lockedThrow);
        row("identity-hash", R21EaRows::identityHash);
        row("ref-eq-fresh", R21EaRows::refEqFresh);
        Object foreign = new Object();
        row("ref-eq-param", (i, rare) -> refEqParam(i, foreign, rare));
        row("lambda-capture", R21EaRows::lambdaCapture);
        row("clinit-side-effect", R21EaRows::clinitSideEffect);
        row("mid-loop", (i, rare) -> midLoop(64 + (i & 7), rare));
        row("mid-loop-cha", (i, rare) -> midLoopCha(64 + (i & 7), rare));
        row("nested", R21EaRows::nested);
        row("array-var-index", R21EaRows::arrayVarIndex);
        row("load-before-store", R21EaRows::loadBeforeStore);
        row("self-ref", R21EaRows::selfRef);
        row("box-identity", R21EaRows::boxIdentity);
        row("box-phi-deopt", R21EaRows::boxPhiDeopt);
        row("typecheck-fresh", R21EaRows::typecheckFresh);
        long finSum = 0;
        for (int i = 0; i < WARM; i++) {
            finSum += finRow(i);
        }
        for (int k = 0; k < 5 && finalized == 0; k++) {
            System.gc();
            System.runFinalization();
            Thread.sleep(50);
        }
        System.out.println("row fin ran=" + (finalized > 0) + " warm=" + finSum);
        System.out.println("row late-hits " + Late1.hits + Late2.hits + Late3.hits + Late4.hits
                + Late5.hits + Late6.hits + Late7.hits + Late8.hits + Late9.hits + Late10.hits
                + Late11.hits + Late12.hits + Late13.hits + Late14.hits);

        timeRows();
    }

    // ---------------------------------------------------------------- time rows

    static final int N = 1_000_000;
    static final int REPS = 7;
    static int acc;

    record Pt(int x, int y) {}

    static List<Integer> LIST = new ArrayList<>();

    static {
        for (int k = 0; k < 16; k++) {
            LIST.add(k);
        }
    }

    static String[] STRS = {"a", "bb", null, "dddd"};

    interface Op {
        int run(int n);
    }

    static int boxedSum(int n) {
        Integer s = 0;
        for (int i = 0; i < n; i++) {
            s += i & 1023;
        }
        return s;
    }

    static int foreachList(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            for (Integer v : LIST) {
                s += v;
            }
        }
        return s;
    }

    static int optionalChain(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += Optional.ofNullable(STRS[i & 3]).map(String::length).orElse(7);
        }
        return s;
    }

    static int recordLoop(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            Pt p = new Pt(i, i + 1);
            s += p.x() * 3 + p.y();
        }
        return s;
    }

    static int sbLocal(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += new StringBuilder().append(i & 1023).append(',').length();
        }
        return s;
    }

    static final Object OTHER = new Object();

    static boolean eqLike(P a, Object o) {
        if (a == o) {
            return true;
        }
        return o instanceof P q && q.x == a.x;
    }

    static int refCompareFresh(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            P a = new P();
            a.x = i;
            s += eqLike(a, OTHER) ? 1 : a.x & 1;
        }
        return s;
    }

    static void time(String name, int perRep, Op op) {
        for (int w = 0; w < 3; w++) {
            acc += op.run(perRep);
        }
        long[] t = new long[REPS];
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            acc += op.run(perRep);
            t[r] = System.nanoTime() - t0;
        }
        java.util.Arrays.sort(t);
        System.out.println("time " + name + " " + (t[REPS / 2] / perRep) + " ns/op");
    }

    static void timeRows() {
        time("boxed-sum", N, R21EaRows::boxedSum);
        time("foreach-arraylist-16", N / 16, R21EaRows::foreachList);
        time("optional-chain", N, R21EaRows::optionalChain);
        time("record-in-loop", N, R21EaRows::recordLoop);
        time("sb-local", N, R21EaRows::sbLocal);
        time("ref-compare-fresh", N, R21EaRows::refCompareFresh);
        System.out.println("acc " + (acc != 0x7fffffff));
    }
}
