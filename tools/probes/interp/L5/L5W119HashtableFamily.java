// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 119, lane L5: the `java.util.Hashtable` surface
// the `jdk-hashtable` HotSpot-alignment row moves onto the JDK's own bodies
// under `--compatible` (`i52-L5-proposal-a-compatible-row-per-retired-shadow-family`,
// "Wave 119"; row default OFF). Every line must be HotSpot's in `--jdk-only`,
// and in `--compatible` with the row on; with it off, `--compatible` is the
// VM's stand-ins (any line that differs there is what the row fixes).
// Iteration orders are the JDK's bucket order (deterministic for these keys).
//
// Run: javac -d . L5W119HashtableFamily.java
//      java -cp . L5W119HashtableFamily                                      (HotSpot)
//      cratonvm --jdk-only -cp . L5W119HashtableFamily
//      cratonvm --compatible -cp . L5W119HashtableFamily
//      CRATONVM_COMPAT_JDK_HASHTABLE=1 cratonvm --compatible -cp . L5W119HashtableFamily
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// to-string {b=2, a=1, e=5, d=4, c=3}
// keys [b, a, e, d, c]
// elements [2, 1, 5, 4, 3]
// key-set [b, a, e, d, c]
// values [2, 1, 5, 4, 3]
// entry-set [b=2, a=1, e=5, d=4, c=3]
// get-a 1 get-z null contains-value-4 true
// set-value a=10 now 10
// iter-remove-b size 4 has-b false
// view-remove-c size 3
// values-remove-5 size 2 [a=10, d=4]
// clone-same true clone-independent 2 vs 3
// equals-hashmap true hash-equal true
// null-key java.lang.NullPointerException
// null-value java.lang.NullPointerException
// put-if-absent 4 4 compute 40 merge 41
// cme java.util.ConcurrentModificationException
// enum-after-put [a, d, n]
// subclass-is-hashtable true size 1
// rehash-keys 100 sum 4950
// HOTSPOT_EXPECTED_END

import java.util.ArrayList;
import java.util.Collections;
import java.util.ConcurrentModificationException;
import java.util.Enumeration;
import java.util.HashMap;
import java.util.Hashtable;
import java.util.Iterator;
import java.util.List;
import java.util.Map;

public class L5W119HashtableFamily {
    static <T> List<T> list(Enumeration<T> e) {
        return Collections.list(e);
    }

    static class Sub extends Hashtable<String, Integer> {
    }

    public static void main(String[] args) {
        Hashtable<String, Integer> t = new Hashtable<>();
        t.put("a", 1);
        t.put("b", 2);
        t.put("c", 3);
        t.put("d", 4);
        t.put("e", 5);
        System.out.println("to-string " + t);
        System.out.println("keys " + list(t.keys()));
        System.out.println("elements " + list(t.elements()));
        System.out.println("key-set " + t.keySet());
        System.out.println("values " + t.values());
        System.out.println("entry-set " + t.entrySet());
        System.out.println("get-a " + t.get("a") + " get-z " + t.get("z") + " contains-value-4 " + t.containsValue(4));

        for (Map.Entry<String, Integer> e : t.entrySet()) {
            if (e.getKey().equals("a")) {
                e.setValue(10);
                System.out.println("set-value " + e + " now " + t.get("a"));
            }
        }
        Iterator<String> it = t.keySet().iterator();
        while (it.hasNext()) {
            if (it.next().equals("b")) {
                it.remove();
            }
        }
        System.out.println("iter-remove-b size " + t.size() + " has-b " + t.containsKey("b"));
        t.keySet().remove("c");
        System.out.println("view-remove-c size " + t.size());
        t.values().remove(5);
        System.out.println("values-remove-5 size " + t.size() + " " + new ArrayList<>(t.entrySet()));

        @SuppressWarnings("unchecked")
        Hashtable<String, Integer> c = (Hashtable<String, Integer>) t.clone();
        c.put("x", 9);
        System.out.println("clone-same " + c.keySet().containsAll(t.keySet()) + " clone-independent "
                + t.size() + " vs " + c.size());

        HashMap<String, Integer> hm = new HashMap<>(t);
        System.out.println("equals-hashmap " + t.equals(hm) + " hash-equal " + (t.hashCode() == hm.hashCode()));

        try {
            t.put(null, 1);
            System.out.println("null-key ok");
        } catch (RuntimeException e) {
            System.out.println("null-key " + e.getClass().getName());
        }
        try {
            t.put("n", null);
            System.out.println("null-value ok");
        } catch (RuntimeException e) {
            System.out.println("null-value " + e.getClass().getName());
        }

        Integer pia = t.putIfAbsent("d", 99);
        Integer computed = t.compute("d", (k, v) -> v * 10);
        Integer merged = t.merge("d", 1, Integer::sum);
        System.out.println("put-if-absent " + pia + " " + t.get("d") / 10 + " compute " + computed + " merge " + merged);
        t.put("d", 4);

        try {
            for (String k : t.keySet()) {
                t.put(k + "!", 0);
            }
            System.out.println("cme none");
        } catch (ConcurrentModificationException e) {
            System.out.println("cme " + e.getClass().getName());
        }
        t.keySet().removeIf(k -> k.endsWith("!"));
        t.put("n", 7);
        List<String> ks = list(t.keys());
        Collections.sort(ks);
        System.out.println("enum-after-put " + ks);

        Sub s = new Sub();
        s.put("q", 1);
        System.out.println("subclass-is-hashtable " + (s instanceof Hashtable) + " size " + s.size());

        Hashtable<Integer, Integer> big = new Hashtable<>(2);
        for (int i = 0; i < 100; i++) {
            big.put(i, i);
        }
        int sum = 0;
        for (int v : big.values()) {
            sum += v;
        }
        System.out.println("rehash-keys " + big.size() + " sum " + sum);
    }
}
