// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zgc round zr, wave z1, lane mk (2026-10-12).
//
// A soft referent the LRU policy KEEPS must survive a collection with
// everything it owns: its reference slots, the native collection overlays of
// the java.util collections it holds (TreeMap / LinkedHashMap / LinkedList are
// native-backed on CratonVM), and the class loader of an instance whose class
// nothing else reaches. Until zr z1/mk the ZGC soft remark traced the
// reference slots only (gc/src/zgc.rs, the INT-8 remark block).
//
// It also reports the JLS strength ordering one level deeper: a WeakReference
// to an object reachable only THROUGH a kept soft referent (depth 2) must not
// be cleared (HotSpot keeps soft-reachable objects alive across weak
// processing). That half is lane rf's (the ZGC reference processor has no
// transitive tracer); the probe carries it so both rows run together.
//
// Built-in vacuity control: a weak-only holder built by the same helper must
// be cleared by the same System.gc(). If it is not, a stale frame slot is
// holding the graph strongly and every soft row below says nothing.
//
// Output (one line, HotSpot prints the same):
//   ZR1MK-SOFT rounds=<R> kept=<R> cleared=0 bad=0 loaderOk=<R> weakDeepCleared=0 control=ok
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.LinkedHashMap;
import java.util.LinkedList;
import java.util.Map;
import java.util.TreeMap;

public class Zr1MkSoftRemark {
    /** Loaded a second time by a private URLClassLoader. */
    public static class Payload {
        static final String TAG = "payload-" + 0x5eed;
        final int[] data = new int[64];
        public Payload() {
            for (int i = 0; i < data.length; i++) data[i] = i * 31;
        }
        @Override public String toString() {
            int s = 0;
            for (int v : data) s += v;
            return TAG + ":" + s;
        }
    }

    static final class Holder {
        final TreeMap<Integer, String> tree = new TreeMap<>();
        final LinkedHashMap<String, Integer> lhm = new LinkedHashMap<>();
        final LinkedList<Object> list = new LinkedList<>();
        Object payload;
        Holder(int n, Object payload) {
            for (int i = 0; i < n; i++) {
                tree.put(i, "t" + i);
                lhm.put("k" + i, i);
                list.add(new int[] {i});
            }
            this.payload = payload;
        }
    }

    static long checksum(Holder h, int n) {
        long s = 0;
        if (h.tree.size() != n || h.lhm.size() != n || h.list.size() != n) return -1;
        for (Map.Entry<Integer, String> e : h.tree.entrySet()) {
            if (!e.getValue().equals("t" + e.getKey())) return -2;
            s += e.getKey();
        }
        for (Map.Entry<String, Integer> e : h.lhm.entrySet()) {
            if (!e.getKey().equals("k" + e.getValue())) return -3;
            s += e.getValue();
        }
        int i = 0;
        for (Object o : h.list) {
            if (((int[]) o)[0] != i++) return -4;
        }
        return s;
    }

    static Object newPayload() throws Exception {
        URL here = Zr1MkSoftRemark.class.getProtectionDomain().getCodeSource().getLocation();
        // Parent = null: the bootstrap loader, so Payload is defined HERE and
        // the loader is reachable only through Payload's class.
        URLClassLoader l = new URLClassLoader(new URL[] {here}, null);
        Class<?> c = Class.forName(Zr1MkSoftRemark.class.getName() + "$Payload", true, l);
        if (c == Payload.class) throw new IllegalStateException("not a second definition");
        return c.getConstructor().newInstance();
    }

    // Helpers keep the strong graph out of main's frame.
    static SoftReference<Holder> makeSoft(int n) throws Exception {
        return new SoftReference<>(new Holder(n, newPayload()));
    }

    static WeakReference<Object> weakDeep(SoftReference<Holder> s) {
        Holder h = s.get();
        return new WeakReference<>(h == null ? null : (Object) h.tree);
    }

    static WeakReference<Holder> makeWeakOnly(int n) {
        return new WeakReference<>(new Holder(n, null));
    }

    static long touch(SoftReference<Holder> s) {
        Holder h = s.get();
        return h == null ? 0 : h.tree.size();
    }

    static void churn(int mb) {
        Object[] junk = new Object[1024];
        for (int i = 0; i < mb * 16; i++) junk[i & 1023] = new byte[64 * 1024];
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 2000;
        String expectPayload = new Payload().toString();

        int kept = 0, cleared = 0, bad = 0, loaderOk = 0, weakDeepCleared = 0;
        boolean controlOk = true;
        for (int r = 0; r < rounds; r++) {
            SoftReference<Holder> soft = makeSoft(n);
            WeakReference<Object> deep = weakDeep(soft);
            WeakReference<Holder> control = makeWeakOnly(16);
            touch(soft); // recently used: the LRU policy must keep it
            churn(8);
            System.gc();
            if (control.get() != null) controlOk = false;
            Holder h = soft.get();
            if (h == null) {
                cleared++;
                continue;
            }
            kept++;
            long want = 2L * ((long) n * (n - 1) / 2);
            if (checksum(h, n) != want) bad++;
            Object p = h.payload;
            if (p != null
                    && p.getClass().getClassLoader() instanceof URLClassLoader
                    && p.toString().equals(expectPayload)) {
                loaderOk++;
            }
            if (deep.get() == null) weakDeepCleared++;
            h = null;
        }
        System.out.println("ZR1MK-SOFT rounds=" + rounds + " kept=" + kept + " cleared=" + cleared
                + " bad=" + bad + " loaderOk=" + loaderOk + " weakDeepCleared=" + weakDeepCleared
                + " control=" + (controlOk ? "ok" : "VACUOUS"));
    }
}
