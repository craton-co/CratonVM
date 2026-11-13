// JIT round j27 wave 7, lane T1EA: the upcall cost of StringBuilder.append(Object) /
// String.valueOf(Object) for an argument whose toString() runs Java (r18 backlog row SBT16-P1;
// replaces the lost R16SbtraceAppendCost). Facts are deterministic; `time` lines only with
// -Dj27.time=true (best of ROUNDS, ns per call).
//
//   javac -d tools/probes/out tools/probes/jit/j27/J27T1SbAppendObjRows.java
//   java -cp tools/probes/out J27T1SbAppendObjRows                       # HotSpot 25
//   cratonvm [--compatible] -cp tools/probes/out J27T1SbAppendObjRows    # arms: default, CRATONVM_TOSTRING_UPCALL_FRAMES=0
//
// Expected HotSpot 25 output (default arms; with CRATONVM_TOSTRING_UPCALL_FRAMES=0 the
// --compatible stand-ins may lose the two JDK frames of throw-frames, which is what the switch fixes):
//   fact append-obj 300000
//   fact value-of-obj 300000
//   fact buffer-append-obj 300000
//   fact throw-frames java.lang.String.valueOf java.lang.StringBuilder.append

public class J27T1SbAppendObjRows {
    static final int N = 100_000;
    static final int ROUNDS = 5;
    static final boolean TIME = Boolean.getBoolean("j27.time");

    static final class Tag {
        final int v;

        Tag(int v) {
            this.v = v;
        }

        @Override
        public String toString() {
            return (v & 1) == 0 ? "ab" : "cdef";
        }
    }

    static final class Bad {
        @Override
        public String toString() {
            throw new IllegalStateException("bad");
        }
    }

    static long appendObj(Tag[] tags) {
        long sum = 0;
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < N; i++) {
            sb.setLength(0);
            sb.append(tags[i & 7]);
            sum += sb.length();
        }
        return sum;
    }

    static long valueOfObj(Tag[] tags) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += String.valueOf((Object) tags[i & 7]).length();
        }
        return sum;
    }

    static long bufferAppendObj(Tag[] tags) {
        long sum = 0;
        StringBuffer sb = new StringBuffer();
        for (int i = 0; i < N; i++) {
            sb.setLength(0);
            sb.append(tags[i & 7]);
            sum += sb.length();
        }
        return sum;
    }

    interface Row {
        long run(Tag[] tags);
    }

    static void row(String name, Row r, Tag[] tags) {
        long v = 0;
        long best = Long.MAX_VALUE;
        for (int k = 0; k < ROUNDS; k++) {
            long t0 = System.nanoTime();
            v = r.run(tags);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("fact " + name + " " + v);
        if (TIME) {
            System.out.println("time " + name + " " + (best / N) + " ns/call");
        }
    }

    public static void main(String[] args) {
        Tag[] tags = new Tag[8];
        for (int i = 0; i < 8; i++) {
            tags[i] = new Tag(i);
        }
        row("append-obj", J27T1SbAppendObjRows::appendObj, tags);
        row("value-of-obj", J27T1SbAppendObjRows::valueOfObj, tags);
        row("buffer-append-obj", J27T1SbAppendObjRows::bufferAppendObj, tags);
        try {
            new StringBuilder().append(new Bad());
            System.out.println("fact throw-frames no-throw");
        } catch (IllegalStateException e) {
            StackTraceElement[] st = e.getStackTrace();
            String a = st.length > 1 ? st[1].getClassName() + "." + st[1].getMethodName() : "none";
            String b = st.length > 2 ? st[2].getClassName() + "." + st[2].getMethodName() : "none";
            System.out.println("fact throw-frames " + a + " " + b);
        }
    }
}
