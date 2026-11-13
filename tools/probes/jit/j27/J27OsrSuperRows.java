// j27 wave 4 (lane OSRSUPER): P2 of
// docs/feature-designs/jit-osr-exit-and-recompile.md
// -- the single-pass OSR door's guarded pass binding a `super.m()` inside a
// guarded body (`CRATONVM_JIT_OSR_GUARDED_SUPER_BIND=1`, opt-in).
//
// Every loop below runs in ONE call of its method, so it lives in an OSR body.
// The in-loop sites are monomorphic virtual calls whose receiver body is a
// covariant override `{ super.m(v); return this; }`:
//   * `ByteBuffer.position(I)` / `limit(I)` on a HeapByteBuffer (the NIO shape);
//   * `Mid.pos(I)` over `Base.pos(I)` (user classes; `Base.pos` builds its
//     exception message with an inline `invokedynamic` concat on its cold arm);
//   * `HMid.pos(I)` over `HBase.pos(I)` (the message built in a separate helper,
//     the JDK's own `createPositionException` shape).
// Rows: the setter loops (checksums), an exception thrown from the cold arm of
// the bound super method INSIDE the OSR'd loop (class, message, the frames from
// the throw up to `main`, the buffer state left behind), and a receiver of a
// class first loaded mid-loop (`Late`, overriding `Mid.pos`) that must miss the
// guard into ordinary dispatch.
//
//   java -cp <dir> J27OsrSuperRows                  > hs.txt
//   cratonvm --cp <dir> J27OsrSuperRows | diff - hs.txt
//   (both modes, with and without CRATONVM_JIT_OSR_GUARDED_SUPER_BIND=1, and --nojit)
// Timing (separate `time` lines, ns/op of a SECOND call of 20,000,000: the method
// is compiled at entry by then, so this times the method-entry body, not the OSR one):
//   java -Dj27.timing=true -cp <dir> J27OsrSuperRows
//
// Expected HotSpot 25 output (facts only; the two `frames` rows are derived
// from the JDK 25 sources and must be confirmed by the HotSpot reference run):
//   fact osr-setters 45000000
//   fact osr-throw java.lang.IllegalArgumentException newPosition > limit: (9 > 8)
//   fact osr-throw-frames java.nio.Buffer.createPositionException<java.nio.Buffer.position<java.nio.ByteBuffer.position<J27OsrSuperRows.throwLoop<J27OsrSuperRows.main
//   fact osr-throw-state pos=7 lim=8
//   fact osr-override 10750000 late=J27OsrSuperRows$Late
//   fact osr-user-throw java.lang.IllegalArgumentException neg: -5
//   fact osr-user-throw-frames J27OsrSuperRows$Base.pos<J27OsrSuperRows$Mid.pos<J27OsrSuperRows.userThrowLoop<J27OsrSuperRows.main
//   fact osr-user-throw-sum 10499978
//   fact osr-helper 10500000
//   fact osr-helper-throw java.lang.IllegalArgumentException hneg: -7
import java.nio.ByteBuffer;

public class J27OsrSuperRows {
    static final int N = 3_000_000;

    static class Base {
        int p;

        Base pos(int v) {
            if (v < 0) throw new IllegalArgumentException("neg: " + v);
            p = v;
            return this;
        }
    }

    static class Mid extends Base {
        @Override
        Mid pos(int v) {
            super.pos(v);
            return this;
        }
    }

    // Loaded only when `makeLate` first runs, inside `overrideLoop`'s loop.
    static class Late extends Mid {
        @Override
        Mid pos(int v) {
            super.pos(v + 1);
            return this;
        }
    }

    static class HBase {
        int p;

        HBase pos(int v) {
            if (v < 0) throw negative(v);
            p = v;
            return this;
        }

        private static IllegalArgumentException negative(int v) {
            return new IllegalArgumentException(new StringBuilder("hneg: ").append(v).toString());
        }
    }

    static class HMid extends HBase {
        @Override
        HMid pos(int v) {
            super.pos(v);
            return this;
        }
    }

    static long userSum;

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    static String ex(Throwable t) {
        return t.getClass().getName() + " " + t.getMessage();
    }

    /** `class.method` from the throw up to and including `main`. */
    static String frames(Throwable t) {
        StringBuilder sb = new StringBuilder();
        for (StackTraceElement e : t.getStackTrace()) {
            if (sb.length() > 0) sb.append('<');
            sb.append(e.getClassName()).append('.').append(e.getMethodName());
            if (e.getClassName().equals("J27OsrSuperRows") && e.getMethodName().equals("main")) {
                break;
            }
        }
        return sb.toString();
    }

    // sum(i & 7) + sum(8 + (i & 7)) over N = 10,500,000 + 34,500,000.
    static long setterLoop(ByteBuffer bb, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            bb.limit(8 + (i & 7));
            bb.position(i & 7);
            s += bb.position() + bb.limit();
        }
        return s;
    }

    static long throwLoop(ByteBuffer bb, int n, int throwAt) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            bb.limit(8);
            bb.position(i == throwAt ? 9 : (i & 7));
            s += bb.position();
        }
        return s;
    }

    static Mid makeLate() {
        return new Late();
    }

    // sum(i & 7) over N, plus 1 for each i >= switchAt with (i & 3) == 1.
    static long overrideLoop(Mid[] rs, int n, int switchAt) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            if (i == switchAt) rs[1] = makeLate();
            s += rs[i & 3].pos(i & 7).p;
        }
        return s;
    }

    static void userThrowLoop(Mid[] rs, int n, int throwAt) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += rs[i & 3].pos(i == throwAt ? -5 : (i & 7)).p;
            userSum = s;
        }
    }

    static long helperLoop(HMid[] rs, int n, int throwAt) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += rs[i & 3].pos(i == throwAt ? -7 : (i & 7)).p;
        }
        return s;
    }

    static Mid[] mids() {
        return new Mid[] {new Mid(), new Mid(), new Mid(), new Mid()};
    }

    static HMid[] hmids() {
        return new HMid[] {new HMid(), new HMid(), new HMid(), new HMid()};
    }

    public static void main(String[] args) {
        fact("osr-setters", setterLoop(ByteBuffer.allocate(16), N));

        ByteBuffer tb = ByteBuffer.allocate(16);
        try {
            fact("osr-throw", "no exception " + throwLoop(tb, N, 2_500_000));
        } catch (IllegalArgumentException e) {
            fact("osr-throw", ex(e));
            fact("osr-throw-frames", frames(e));
        }
        fact("osr-throw-state", "pos=" + tb.position() + " lim=" + tb.limit());

        Mid[] rs = mids();
        long o = overrideLoop(rs, N, 2_000_000);
        fact("osr-override", o + " late=" + rs[1].getClass().getName());

        try {
            userThrowLoop(mids(), N, 2_999_999 - 3); // (throwAt & 3) == 0: a Mid
            fact("osr-user-throw", "no exception");
        } catch (IllegalArgumentException e) {
            fact("osr-user-throw", ex(e));
            fact("osr-user-throw-frames", frames(e));
        }
        // sum(i & 7) for i < 2,999,996 = 374,999 full cycles * 28 + (0+1+2+3) = 10,499,978.
        fact("osr-user-throw-sum", userSum);

        fact("osr-helper", helperLoop(hmids(), N, -1));
        try {
            fact("osr-helper-throw", "no exception " + helperLoop(hmids(), N, 2_400_000));
        } catch (IllegalArgumentException e) {
            fact("osr-helper-throw", ex(e));
        }

        if (Boolean.getBoolean("j27.timing")) {
            int t = 20_000_000;
            long t0 = System.nanoTime();
            long r = setterLoop(ByteBuffer.allocate(16), t);
            long t1 = System.nanoTime();
            System.out.println("time osr-setters " + (t1 - t0) / (double) t + " (" + r + ")");
            t0 = System.nanoTime();
            r = helperLoop(hmids(), t, -1);
            t1 = System.nanoTime();
            System.out.println("time osr-helper " + (t1 - t0) / (double) t + " (" + r + ")");
        }
    }
}
