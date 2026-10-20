// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// hcb (JIT round 16): enum keys in the maps `--compatible` keeps native
// (`ConcurrentHashMap`, `Hashtable`, `TreeMap`), including constants with
// their own bodies (anonymous subclasses of the enum). The parity probe for
// `CRATONVM_COMPAT_MAP_ENUM_KEY_FACTS` (`native-collections`
// `enum_key_identity`): output must match HotSpot 25 with the switch on and off.
//
//   cratonvm --compatible --cp . HcbEnumKeyMaps

import java.util.*;
import java.util.concurrent.ConcurrentHashMap;

public class HcbEnumKeyMaps {
    enum Plain { A, B, C, D }

    enum Bodied {
        X { int w() { return 1; } },
        Y { int w() { return 2; } },
        Z;
        int w() { return 0; }
    }

    static <K> String run(String name, Map<K, Integer> m, K[] keys) {
        for (int i = 0; i < keys.length; i++) m.put(keys[i], i);
        StringBuilder sb = new StringBuilder(name).append(':');
        for (K k : keys) sb.append(' ').append(k).append('=').append(m.get(k));
        sb.append(" contains=").append(m.containsKey(keys[0]));
        sb.append(" remove=").append(m.remove(keys[keys.length - 1]));
        sb.append(" size=").append(m.size());
        sb.append(" removedGet=").append(m.get(keys[keys.length - 1]));
        return sb.toString();
    }

    public static void main(String[] args) {
        Plain[] p = Plain.values();
        Bodied[] b = Bodied.values();
        System.out.println(run("chm-plain", new ConcurrentHashMap<>(), p));
        System.out.println(run("chm-bodied", new ConcurrentHashMap<>(), b));
        System.out.println(run("ht-plain", new Hashtable<>(), p));
        System.out.println(run("ht-bodied", new Hashtable<>(), b));
        System.out.println(run("tm-plain", new TreeMap<>(), p));
        System.out.println(run("tm-bodied", new TreeMap<>(), b));
        System.out.println(run("hm-bodied", new HashMap<>(), b));
        Map<Object, Integer> mixed = new ConcurrentHashMap<>();
        mixed.put(Plain.A, 1);
        mixed.put(Bodied.X, 2);
        mixed.put("A", 3);
        System.out.println("mixed: " + mixed.get(Plain.A) + " " + mixed.get(Bodied.X) + " " + mixed.get("A")
                + " " + mixed.get(Bodied.Z) + " " + Bodied.X.getClass().isAnonymousClass()
                + " " + Bodied.X.getDeclaringClass().getSimpleName());
        System.out.println("bodies: " + Bodied.X.w() + Bodied.Y.w() + Bodied.Z.w());
    }
}
