import java.lang.reflect.Array;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.HashMap;
import java.util.Map;

/**
 * Which Java-visible witnesses can still tell a CratonVM native {@code HashMap}
 * from real {@code java.util.HashMap} bytecode -- and through which dispatch
 * door.
 *
 * <h2>Why this file exists</h2>
 *
 * {@code CRATONVM_ENFORCE_NATIVE_SHADOW=<prefix>} is meant to make contract
 * §1.4 <em>enforced</em> rather than counted: the native stops winning, real
 * JDK bytecode runs, exactly as a permanent retirement would. Until 2026-08-21
 * it reached exactly one dispatch door of the fourteen
 * ({@code resolve_step1_native}), so arming a class armed only its cold, step-1
 * dispatches -- 6% of them on this probe, 0.01% on the {@code jit} case. Four
 * published records priced retirements with that instrument.
 *
 * <p>Reading an armed measurement therefore needs two things this probe
 * supplies together: a witness that is not blind, and a case whose dispatch
 * door is known. The VM-side other half is {@code [DIAL_DOOR]} on stderr, which
 * is printed whenever the dial is armed and counts armed {@code Bridge}
 * arrivals per door -- {@code reached} minus {@code yielded} is the price the
 * dial is not charging, and it must be zero.
 *
 * <h2>Two rules, both of which published probes have violated</h2>
 *
 * <ol>
 *   <li><b>One case per process.</b> Anything latched per process is otherwise
 *       confounded with case order; four "discriminators" of a supposed third
 *       {@code ConcurrentHashMap} defect were pure position. The case is
 *       {@code argv[0]} here and nothing else runs.</li>
 *   <li><b>State what the witness can see.</b> The {@code table} array class
 *       reports <em>one bit per map</em> -- whether that map's first insert ran
 *       bytecode. It cannot count yields, and reading it as a count is what
 *       produced three records' shared error. That is why this prints nine
 *       witnesses on one line instead of one.</li>
 * </ol>
 *
 * <h2>Which witnesses discriminate (MEASURED 2026-08-21, dev at 1fcc241e0)</h2>
 *
 * <pre>
 *   witness              HotSpot            unarmed                     discriminates?
 *   table array class    HashMap$Node[]     Object[]                    YES
 *   bucket head class    HashMap$Node       AnonymousObject$4           YES
 *   threshold, on grow   24                 12                          YES  (the native
 *                                                                             never updates
 *                                                                             it on resize)
 *   key/entry iteration  k0..k3             k0..k3                      only when HYBRID
 *   size() vs iteration  4 vs 4             4 vs 4                      only when HYBRID
 *   table length         16 / 32            16 / 32                     no
 *   occupied buckets     4 / 15             4 / 15                      no
 *   modCount             4                  4                           no
 *   size field           4                  4                           no
 * </pre>
 *
 * <p>The blindness table is a property of the BINARY, not of the witness. Four
 * of these were measured blind on one branch's binary the same week they
 * discriminate here, and one of them went blind because the VM was
 * <em>fixed</em>. Re-measure before quoting; a reader who picks one at random
 * gets a false green.
 *
 * <p>The last two rows are the hybrid detector and the reason they are printed
 * even though they agree in both healthy configurations: a half-armed
 * {@code HashMap} reported {@code size()=4} over four occupied buckets and
 * iterated <em>one</em> key. Neither a fully-native nor a fully-real map can do
 * that, so a disagreement between {@code size()} and the iteration length is
 * positive evidence of the mixed state that no retirement can reach.
 *
 * <h2>Running it</h2>
 *
 * <pre>
 *   javac DialWitness.java
 *   OPENS="--add-opens java.base/java.util=ALL-UNNAMED"
 *   "$JAVA_HOME/bin/java"      $OPENS -cp . DialWitness direct     # oracle
 *   cratonvm --jdk-only        $OPENS -cp . DialWitness direct     # control: must not move
 *   CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashMap \
 *     cratonvm --jdk-only      $OPENS -cp . DialWitness direct     # armed
 * </pre>
 *
 * On Windows the JDK home must be the WINDOWS spelling --
 * {@code cygpath -m "$(dirname "$(dirname "$(command -v javap)")")"}. The MSYS
 * POSIX form reaches {@code cratonvm.exe} unconverted (the harness exports
 * {@code MSYS_NO_PATHCONV=1}) and the VM dies in argument parsing, printing a
 * bare {@code rc=1} that is indistinguishable from a real assertion failure.
 *
 * <p>Cases, and the door each one is aimed at: {@code direct} (cold step 1),
 * {@code iface} (interface dispatch), {@code reflect} (reflective
 * {@code Method.invoke}, which has no bytecode PC to key an invoke-cache entry
 * on), {@code warm} (400 executions of one call site -- the warm invoke-cache
 * door, which carried 82% of the leak), {@code jit} (60 000, hot enough to
 * compile the caller -- 299 292 of 299 431 leaked dispatches before the fix),
 * {@code grow} (20 entries, so the real body's {@code resize} runs and
 * {@code threshold} becomes a witness).
 */
public class DialWitness {

    static Field f(String n) throws Exception {
        Field f = HashMap.class.getDeclaredField(n);
        f.setAccessible(true);
        return f;
    }

    static String cls(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    static void report(String tag, HashMap<String, String> m) throws Exception {
        Object table = f("table").get(m);
        int tlen = table == null ? -1 : Array.getLength(table);
        Object head = null;
        int occupied = 0;
        if (table != null) {
            for (int i = 0; i < tlen; i++) {
                Object b = Array.get(table, i);
                if (b != null) {
                    occupied++;
                    if (head == null) {
                        head = b;
                    }
                }
            }
        }
        StringBuilder order = new StringBuilder();
        for (String k : m.keySet()) {
            order.append(k).append('|');
        }
        StringBuilder eorder = new StringBuilder();
        for (Map.Entry<String, String> e : m.entrySet()) {
            eorder.append(e.getKey()).append('=').append(e.getValue()).append('|');
        }
        System.out.println(tag
                + " tableCls=" + cls(table)
                + " tableLen=" + tlen
                + " buckets=" + occupied
                + " threshold=" + f("threshold").get(m)
                + " modCount=" + f("modCount").get(m)
                + " sizeField=" + f("size").get(m)
                + " loadFactor=" + f("loadFactor").get(m)
                + " headCls=" + cls(head)
                + " size()=" + m.size()
                + " keyOrder=" + order
                + " entryOrder=" + eorder);
    }

    static HashMap<String, String> fillDirect(int n) {
        HashMap<String, String> m = new HashMap<String, String>();
        for (int i = 0; i < n; i++) {
            m.put("k" + i, "v" + i);
        }
        return m;
    }

    static HashMap<String, String> fillIface(int n) {
        Map<String, String> m = new HashMap<String, String>();
        for (int i = 0; i < n; i++) {
            m.put("k" + i, "v" + i);
        }
        return (HashMap<String, String>) m;
    }

    static HashMap<String, String> fillReflect(int n) throws Exception {
        Method put = HashMap.class.getMethod("put", Object.class, Object.class);
        HashMap<String, String> m = new HashMap<String, String>();
        for (int i = 0; i < n; i++) {
            put.invoke(m, "k" + i, "v" + i);
        }
        return m;
    }

    public static void main(String[] a) throws Exception {
        String c = a.length > 0 ? a[0] : "direct";
        if (c.equals("direct")) {
            report("[direct]", fillDirect(4));
        } else if (c.equals("iface")) {
            report("[iface]", fillIface(4));
        } else if (c.equals("reflect")) {
            report("[reflect]", fillReflect(4));
        } else if (c.equals("warm")) {
            // Warm the invoke cache at ONE call site, then report a map built
            // entirely through the warmed site.
            HashMap<String, String> last = null;
            for (int r = 0; r < 400; r++) {
                last = fillDirect(4);
            }
            report("[warm]", last);
        } else if (c.equals("jit")) {
            // Same site, hot enough to compile the caller.
            HashMap<String, String> last = null;
            for (int r = 0; r < 60000; r++) {
                last = fillDirect(4);
            }
            report("[jit]", last);
        } else if (c.equals("grow")) {
            // 20 entries forces one resize; the real body's resize is the code
            // that reads and rewrites `threshold`.
            report("[grow]", fillDirect(20));
        } else {
            System.out.println("PROBE-BAD-CASE " + c);
        }
    }
}
