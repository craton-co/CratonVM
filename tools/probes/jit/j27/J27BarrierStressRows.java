// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 5 (lane BARHUNT): GC-barrier stress for compiled reference stores.
// Page: docs/feature-designs/jit-barrier-lowering.md
//
// Every row promotes its holders first (two System.gc() calls), then, for
// ROUNDS rounds, stores freshly allocated (young) Box objects into the OLD
// holders from a compiled loop, churns young garbage (and every 8th round runs
// System.gc()), and only then reads every stored Box back. A missing card /
// remembered-set / SATB edge lets a young collection free or move a Box the old
// holder still names: the read-back sum differs, the per-Box check word fails
// (`bad`), or the VM crashes. The stores cover putfield, aastore, putstatic
// (a static of another class, i.e. a mirror / statics table), VarHandle CAS /
// getAndSet / setRelease, AtomicReferenceArray.set, System.arraycopy,
// Arrays.copyOf, Object[].clone, ArrayList growth and a constructor that
// stores two fresh objects into a third fresh object kept by an old array.
//
// Payload of row k in round r at index i: v = a*i + r, so a row's read-back
// sum is a*ROUNDS*N(N-1)/2 + N*ROUNDS(ROUNDS-1)/2 (N=2048, ROUNDS=24:
// a*50307072 + 565248), independent of the collector and of timing.
//
//   java -cp <dir> J27BarrierStressRows > hs.txt
//   cratonvm --cp <dir> J27BarrierStressRows | diff - hs.txt
//     x {ZGC default, -XX:+UseG1GC, -XX:+UseGenerationalGC} x {--real-jdk, --compatible}
//     x {default, -Xmx256m (more young collections)}
//   optional: -Dj27.churnMb=<n> (default 24) young garbage per round;
//             -Dj27.time=true adds `time <row> <ms>` lines (not facts).
//
// Expected HotSpot 25 output (facts; identical for every collector and heap size):
//   fact putfield sum=1560084480 bad=0
//   fact aastore sum=352714752 bad=0
//   fact putstatic sum=49404 bad=0
//   fact vh-cas sum=654557184 ok=49152 bad=0
//   fact vh-getandset sum=252100608 old=241572864 bad=0
//   fact vh-setrelease sum=553943040 bad=0
//   fact atomic-ref-array sum=1459470336 bad=0
//   fact arraycopy sum=151486464 bad=0
//   fact copyof sum=855785472 bad=0
//   fact clone sum=956399616 bad=0
//   fact arraylist sum=1157627904 bad=0
//   fact ctor-nested sum=101179392 bad=0
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicReferenceArray;

public class J27BarrierStressRows {
    static final int N = 2048;
    static final int ROUNDS = 24;
    static final int CHECK = 0x5A5A5A5A;
    static final boolean TIME = Boolean.getBoolean("j27.time");
    static final int CHURN_MB = Integer.getInteger("j27.churnMb", 24);
    static volatile Object churnSink;

    static final class Box {
        final int v;
        final int chk;
        Box(int v) { this.v = v; this.chk = v ^ CHECK; }
    }

    static final class Pair {
        final Box a;
        final Box b;
        Pair(Box a, Box b) { this.a = a; this.b = b; }
    }

    static final class Holder {
        Box box;
        volatile Object vref;
        Object[] arr;
        ArrayList<Box> list;
    }

    /** A different class, so the static lives in another class's mirror / statics. */
    static final class StaticHolder {
        static Box s;
    }

    static final VarHandle VREF;
    static {
        try {
            VREF = MethodHandles.lookup().findVarHandle(Holder.class, "vref", Object.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static long bad;

    static long check(Object o) {
        if (!(o instanceof Box)) {
            bad++;
            return 0;
        }
        Box b = (Box) o;
        if ((b.v ^ CHECK) != b.chk) {
            bad++;
        }
        return b.v;
    }

    static Holder[] oldHolders() {
        Holder[] hs = new Holder[N];
        for (int i = 0; i < N; i++) {
            hs[i] = new Holder();
        }
        promote();
        return hs;
    }

    static void promote() {
        System.gc();
        System.gc();
    }

    /** Young garbage between the stores and the read-back; System.gc() every 8th round. */
    static void churn(int round) {
        long chunks = (long) CHURN_MB * 1024 * 1024 / 160;
        for (long k = 0; k < chunks; k++) {
            churnSink = new Object[32];
        }
        churnSink = null;
        if (round % 8 == 7) {
            System.gc();
        }
    }

    // ---- the compiled store kernels -------------------------------------

    static void storePutfield(Holder[] hs, int r) {
        for (int i = 0; i < N; i++) {
            hs[i].box = new Box(i * 31 + r);
        }
    }

    static void storeAastore(Object[] arr, int r) {
        for (int i = 0; i < N; i++) {
            arr[i] = new Box(i * 7 + r);
        }
    }

    static void storePutstatic(int r) {
        for (int i = 0; i < N; i++) {
            StaticHolder.s = new Box(i + r);
        }
    }

    static int storeCas(Holder[] hs, int r) {
        int ok = 0;
        for (int i = 0; i < N; i++) {
            Holder h = hs[i];
            Object cur = VREF.getVolatile(h);
            if (VREF.compareAndSet(h, cur, new Box(i * 13 + r))) {
                ok++;
            }
        }
        return ok;
    }

    static long storeGetAndSet(Holder[] hs, int r) {
        long old = 0;
        for (int i = 0; i < N; i++) {
            Object prev = VREF.getAndSet(hs[i], new Box(i * 5 + r));
            if (prev != null) {
                old += check(prev);
            }
        }
        return old;
    }

    static void storeSetRelease(Holder[] hs, int r) {
        for (int i = 0; i < N; i++) {
            VREF.setRelease(hs[i], new Box(i * 11 + r));
        }
    }

    static void storeAtomicArray(AtomicReferenceArray<Box> a, int r) {
        for (int i = 0; i < N; i++) {
            a.set(i, new Box(i * 29 + r));
        }
    }

    static Object[] youngBoxes(int mul, int r) {
        Object[] src = new Object[N];
        for (int i = 0; i < N; i++) {
            src[i] = new Box(i * mul + r);
        }
        return src;
    }

    static void storeArraycopy(Object[] dst, int r) {
        System.arraycopy(youngBoxes(3, r), 0, dst, 0, N);
    }

    static void storeCopyOf(Holder h, int r) {
        h.arr = Arrays.copyOf(youngBoxes(17, r), N);
    }

    static void storeClone(Holder h, int r) {
        h.arr = youngBoxes(19, r).clone();
    }

    static void storeArrayList(Holder h, int r) {
        ArrayList<Box> list = new ArrayList<>();
        for (int i = 0; i < N; i++) {
            list.add(new Box(i * 23 + r));
        }
        h.list = list;
    }

    static void storeCtorNested(Object[] arr, int r) {
        for (int i = 0; i < N; i++) {
            arr[i] = new Pair(new Box(i), new Box(i + r));
        }
    }

    // ---- read-back ------------------------------------------------------

    static long sumArray(Object[] arr) {
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += check(arr[i]);
        }
        return s;
    }

    static void fact(String row, long start, String rest) {
        System.out.println("fact " + row + " " + rest + " bad=" + bad);
        if (TIME) {
            System.out.println("time " + row + " " + (System.nanoTime() - start) / 1_000_000 + "ms");
        }
        bad = 0;
    }

    static void rowPutfield() {
        long t = System.nanoTime();
        Holder[] hs = oldHolders();
        long sum = 0;
        for (int r = 0; r < ROUNDS; r++) {
            storePutfield(hs, r);
            churn(r);
            for (int i = 0; i < N; i++) {
                sum += check(hs[i].box);
            }
        }
        fact("putfield", t, "sum=" + sum);
    }

    static void rowAastore() {
        long t = System.nanoTime();
        Object[] arr = new Object[N];
        promote();
        long sum = 0;
        for (int r = 0; r < ROUNDS; r++) {
            storeAastore(arr, r);
            churn(r);
            sum += sumArray(arr);
        }
        fact("aastore", t, "sum=" + sum);
    }

    static void rowPutstatic() {
        long t = System.nanoTime();
        promote();
        long sum = 0;
        for (int r = 0; r < ROUNDS; r++) {
            storePutstatic(r);
            churn(r);
            sum += check(StaticHolder.s);
        }
        fact("putstatic", t, "sum=" + sum);
    }

    static void rowCas() {
        long t = System.nanoTime();
        Holder[] hs = oldHolders();
        long sum = 0;
        long ok = 0;
        for (int r = 0; r < ROUNDS; r++) {
            ok += storeCas(hs, r);
            churn(r);
            for (int i = 0; i < N; i++) {
                sum += check(hs[i].vref);
            }
        }
        fact("vh-cas", t, "sum=" + sum + " ok=" + ok);
    }

    static void rowGetAndSet() {
        long t = System.nanoTime();
        Holder[] hs = oldHolders();
        long sum = 0;
        long old = 0;
        for (int r = 0; r < ROUNDS; r++) {
            old += storeGetAndSet(hs, r);
            churn(r);
            for (int i = 0; i < N; i++) {
                sum += check(hs[i].vref);
            }
        }
        fact("vh-getandset", t, "sum=" + sum + " old=" + old);
    }

    static void rowSetRelease() {
        long t = System.nanoTime();
        Holder[] hs = oldHolders();
        long sum = 0;
        for (int r = 0; r < ROUNDS; r++) {
            storeSetRelease(hs, r);
            churn(r);
            for (int i = 0; i < N; i++) {
                sum += check(hs[i].vref);
            }
        }
        fact("vh-setrelease", t, "sum=" + sum);
    }

    static void rowAtomicArray() {
        long t = System.nanoTime();
        AtomicReferenceArray<Box> a = new AtomicReferenceArray<>(N);
        promote();
        long sum = 0;
        for (int r = 0; r < ROUNDS; r++) {
            storeAtomicArray(a, r);
            churn(r);
            for (int i = 0; i < N; i++) {
                sum += check(a.get(i));
            }
        }
        fact("atomic-ref-array", t, "sum=" + sum);
    }

    static void rowArraycopy() {
        long t = System.nanoTime();
        Object[] dst = new Object[N];
        promote();
        long sum = 0;
        for (int r = 0; r < ROUNDS; r++) {
            storeArraycopy(dst, r);
            churn(r);
            sum += sumArray(dst);
        }
        fact("arraycopy", t, "sum=" + sum);
    }

    static void rowCopyOf() {
        long t = System.nanoTime();
        Holder h = new Holder();
        promote();
        long sum = 0;
        for (int r = 0; r < ROUNDS; r++) {
            storeCopyOf(h, r);
            churn(r);
            sum += sumArray(h.arr);
        }
        fact("copyof", t, "sum=" + sum);
    }

    static void rowClone() {
        long t = System.nanoTime();
        Holder h = new Holder();
        promote();
        long sum = 0;
        for (int r = 0; r < ROUNDS; r++) {
            storeClone(h, r);
            churn(r);
            sum += sumArray(h.arr);
        }
        fact("clone", t, "sum=" + sum);
    }

    static void rowArrayList() {
        long t = System.nanoTime();
        Holder h = new Holder();
        promote();
        long sum = 0;
        for (int r = 0; r < ROUNDS; r++) {
            storeArrayList(h, r);
            churn(r);
            ArrayList<Box> list = h.list;
            if (list.size() != N) {
                bad++;
            }
            for (int i = 0; i < list.size(); i++) {
                sum += check(list.get(i));
            }
        }
        fact("arraylist", t, "sum=" + sum);
    }

    static void rowCtorNested() {
        long t = System.nanoTime();
        Object[] arr = new Object[N];
        promote();
        long sum = 0;
        for (int r = 0; r < ROUNDS; r++) {
            storeCtorNested(arr, r);
            churn(r);
            for (int i = 0; i < N; i++) {
                Object o = arr[i];
                if (!(o instanceof Pair)) {
                    bad++;
                    continue;
                }
                Pair p = (Pair) o;
                sum += check(p.a) + check(p.b);
            }
        }
        fact("ctor-nested", t, "sum=" + sum);
    }

    public static void main(String[] args) {
        rowPutfield();
        rowAastore();
        rowPutstatic();
        rowCas();
        rowGetAndSet();
        rowSetRelease();
        rowAtomicArray();
        rowArraycopy();
        rowCopyOf();
        rowClone();
        rowArrayList();
        rowCtorNested();
    }
}
