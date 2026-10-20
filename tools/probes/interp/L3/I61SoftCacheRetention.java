// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
// LambdaFormEditor uses nested soft caches. A retained inner referent must
// retain its whole graph, and a weak reference into that graph must survive.
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
public class I61SoftCacheRetention {
    static class Node { final Object[] names = {"alive"}; }
    public static void main(String[] args) {
        var direct = new SoftReference<>(new Node());
        Node node = new Node();
        var weak = new WeakReference<>(node);
        var nested = new SoftReference<>(new SoftReference<>(new SoftReference<>(node)));
        node = null;
        for (int i=0; i<5; i++) {
            System.gc();
            Node retained = nested.get().get().get();
            if (weak.get() != retained) throw new AssertionError("soft graph lost its weak edge");
            System.out.println(i+":"+direct.get().names[0]+":"+retained.names[0]);
        }
    }
}
