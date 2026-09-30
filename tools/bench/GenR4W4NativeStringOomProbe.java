// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;

/**
 * gen r4w4/oom (2026-09-24): VM-created objects (Strings built by natives, the
 * reflection and stack-trace natives) on a nearly full heap must end in a
 * catchable {@code OutOfMemoryError}, never in {@code FATAL: heap exhausted
 * allocating java/lang/String} and exit status 134. Verification for the
 * unwinding-context fix of
 * {@code docs/internal/gaps/gengc-r4w3-hunter-native-string-alloc-aborts-on-full-heap-FIXED-20260927.md}
 * (landed in wave 3, never run) and for the native-unwind heap dump added in
 * wave 4.
 *
 * <p>The heap is filled with retained {@code long[128]} blocks until the first
 * {@code OutOfMemoryError}; the last few blocks are released so that ordinary
 * code can run, and then a loop calls VM natives that allocate Strings and
 * arrays, retaining their results, until the heap is exhausted again or 200 000
 * rounds pass. Which allocation meets the wall is not deterministic, so the
 * output is the same either way. Run it with and without {@code --nojit}.
 *
 * <p>HotSpot ({@code -XX:+UseSerialGC -Xmx64m}) prints:
 * <pre>
 *   fill: OutOfMemoryError "Java heap space"
 *   native-strings ok
 *   recovered ok
 *   PASS
 * </pre>
 * Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx64m -cp tools/bench GenR4W4NativeStringOomProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m -cp tools/bench GenR4W4NativeStringOomProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m --nojit -cp tools/bench GenR4W4NativeStringOomProbe
 *   java -XX:+UseG1GC -Xmx64m -cp tools/bench GenR4W4NativeStringOomProbe 4096       # G1 / ZGC oracle
 *   cratonvm --java-home "$JDK" -XX:+UseG1GC -Xmx64m -cp tools/bench GenR4W4NativeStringOomProbe 4096
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m -XX:+HeapDumpOnOutOfMemoryError \
 *       -XX:HeapDumpPath=/tmp -cp tools/bench GenR4W4NativeStringOomProbe
 * </pre>
 * The last command must additionally print, once and before {@code fill:},
 * {@code java.lang.OutOfMemoryError: Java heap space}, {@code Dumping heap to
 * /tmp/java_pid<pid>.hprof ...} and {@code Heap dump file created [<n> bytes
 * in <t> secs]}, exactly as HotSpot does. A process abort, a {@code FATAL:} line
 * or a missing line is a failure.
 *
 * <p>gcd d10/o (2026-09-28): the probe builds every string with an explicit
 * {@code StringBuilder} (or {@code String.concat}), never with {@code +}. javac
 * compiles {@code +} to an {@code invokedynamic} whose FIRST execution runs the
 * {@code StringConcatFactory} bootstrap, and HotSpot 25 ran that bootstrap for
 * the first time right after the fill, with the heap full: it died of an
 * uncaught {@code OutOfMemoryError} in {@code java.lang.classfile.Opcode.<clinit>}
 * and printed nothing, so the probe had no HotSpot oracle
 * ({@code docs/internal/gc/gcd-d8x-native-string-oom-probe-is-not-a-hotspot-oracle-FIXED-20260929.md}).
 * What the loop exercises is unchanged: stack traces, reflection, and Strings
 * made by {@code Integer.toString}, {@code String.valueOf(double)} and
 * {@code Thread.getName}.
 *
 * <p>An optional first argument is the number of 1 KiB blocks the fill
 * releases before the {@code println} (default 64, the probe's historical
 * sliver, which the Serial-oracle pages measured). HotSpot's G1 and ZGC hand
 * out whole regions / pages, and 64 scattered blocks give them nothing to
 * allocate the {@code println} in (they die in the catch; ZGC still 1 run in
 * 4 at 1024), so their oracle runs pass {@code 4096}. Checked on Temurin
 * 25.0.3 (Windows), {@code -Xmx64m}, the four lines above and rc 0:
 * {@code -XX:+UseSerialGC} (default sliver) 3/3; {@code -XX:+UseG1GC 4096}
 * 3/3; {@code -XX:+UseZGC 4096} 5/5.
 */
public final class GenR4W4NativeStringOomProbe {
    static List<long[]> fill = new ArrayList<>();
    static List<Object> made = new ArrayList<>();
    static boolean ok = true;

    public static void main(String[] args) {
        final int sliver = args.length > 0 ? Integer.parseInt(args[0]) : 64;
        try {
            while (true) {
                fill.add(new long[128]);
            }
        } catch (OutOfMemoryError e) {
            // Release a sliver so the println and the loop below can start
            // (`sliver` blocks: see the class comment).
            for (int i = 0; i < sliver && !fill.isEmpty(); i++) {
                fill.remove(fill.size() - 1);
            }
            System.out.println(new StringBuilder("fill: OutOfMemoryError \"")
                    .append(e.getMessage()).append('"').toString());
            ok &= "Java heap space".equals(e.getMessage());
        }

        try {
            for (int i = 0; i < 200_000; i++) {
                made.add(new Throwable().getStackTrace());
                made.add(GenR4W4NativeStringOomProbe.class.getDeclaredMethods());
                made.add(Integer.toString(i));
                made.add(String.valueOf(i * 0.5));
                made.add(Thread.currentThread().getName().concat(Integer.toString(i)));
            }
        } catch (OutOfMemoryError e) {
            // Expected on most runs; the point is that it was catchable.
        }
        made = null;
        fill = null;
        System.out.println("native-strings ok");

        long sum = 0;
        for (int i = 0; i < 100_000; i++) {
            sum += Integer.toString(i).length();
        }
        final boolean good = sum == 488_890L;
        System.out.println(good ? "recovered ok"
                : new StringBuilder("recovered FAILED sum=").append(sum).toString());
        ok &= good;

        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
