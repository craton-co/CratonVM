// j27 wave 5 (lane JVMTIFRAME): the C JVMTI table's frame and local-variable
// answers for a COMPILED caller and an OSR'd loop, against HotSpot 25
// (docs/known-issues/jit/j27-jvmti-compiled-frames-answer-opaque-20261028.md).
//
// `main` runs a 300 000-iteration loop (OSR'd on both VMs) calling the
// instance method `mid` (warm, compiled; its never-run bulk block keeps it
// past every inlining limit, so it stays its own activation). Every 1024th
// call, and on the last one, `mid` calls the native `rows`; on the last one
// only, `rows` reads `mid` (depth 1) and `main` (depth 2) through
// GetFrameCount / GetStackTrace / GetFrameLocation / GetLocal* /
// GetLocalInstance and answers one `fact` line per call. HotSpot answers
// every row from the compiled frames' debug info; CratonVM answered
// `err=32` (OPAQUE_FRAME) for every local of both frames.
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libj27jcfl.so tools/probes/jit/j27/J27JvmtiCompiledFrameLocals.c
//   javac -g -d /tmp/j27jcfl tools/probes/jit/j27/J27JvmtiCompiledFrameLocals.java
//   java     -agentpath:/tmp/libj27jcfl.so -cp /tmp/j27jcfl J27JvmtiCompiledFrameLocals /tmp/libj27jcfl.so
//   cratonvm -agentpath:/tmp/libj27jcfl.so -cp /tmp/j27jcfl J27JvmtiCompiledFrameLocals /tmp/libj27jcfl.so
//   (arms: --real-jdk, --compatible, --nojit; the fix arm adds
//   CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL=1 CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS=1,
//   with CRATONVM_DBG_JITC=1 to see `mid` compile and its home record)
//
// (`-g` matters: the slot and type checks read the LocalVariableTable.)
// HotSpot also prints its restricted-method WARNING lines for `System.load`
// on stderr.
//
// Expected stdout, HotSpot 25 (computed off-line from the Java below and
// the L1W44 measured code rules; confirm on the HotSpot reference run):
//
//   fact frame-count 3
//   fact stack rows mid main
//   fact mid-at-invoke true
//   fact main-at-invoke true
//   fact mid-this same
//   fact mid-x 299999
//   fact mid-w 1099511627776
//   fact mid-d 2.500
//   fact mid-s same
//   fact mid-probe 1
//   fact mid-acc 899997
//   fact mid-r err=34
//   fact mid-x-as-long err=34
//   fact mid-slot-past-end err=35
//   fact main-self same
//   fact main-sum 134998650003
//   fact main-i 299999
//   fact sum 134999550000
//
// `--nojit` must print exactly these lines (every frame interpreted). The
// JIT arms without the switches print `err=32` for the mid-* local rows
// from `mid-this` to `mid-slot-past-end` when `mid` ran compiled, and for
// the main-* rows when the loop ran OSR'd; the frame-count, stack and
// at-invoke rows and the sum are HotSpot's in every arm.
public class J27JvmtiCompiledFrameLocals {
    static native String rows(Object self, String s, boolean record);

    static String out = "";

    // Slots (-g): this=0, x=1, w=2-3, d=4-5, s=6, probe=7, acc=8, r=9.
    int mid(int x, long w, double d, String s, boolean probe) {
        int acc = x * 3;
        if (x < 0) {
            // Never run: 64 statements that keep `mid` past every inlining
            // size limit, so it is a compiled activation of its own.
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
            acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x; acc = acc * 31 + x;
        }
        // Taken every 1024th call too, so the call site is warm in the
        // compiled body (not a trap that would send `mid` back first).
        if ((x & 1023) == 1023 || probe) {
            String r = rows(this, s, probe);
            if (probe) {
                out = r;
            }
        }
        return acc + (int) w;
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: J27JvmtiCompiledFrameLocals <absolute path of the native shim>");
            return;
        }
        System.load(args[0]);
        J27JvmtiCompiledFrameLocals self = new J27JvmtiCompiledFrameLocals();
        long sum = 0;
        for (int i = 0; i < 300_000; i++) {
            sum += self.mid(i, 1L << 40, 2.5, "s", i == 299_999);
        }
        System.out.print(out);
        System.out.println("fact sum " + sum);
    }
}
