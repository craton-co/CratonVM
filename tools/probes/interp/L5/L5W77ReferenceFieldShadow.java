// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 77, lane L5 -- `Reference.clear()` on a subclass
// that declares its own fields named `referent` and `queue` (legal Java:
// `Reference.referent` / `queue` are private), the defect
// `docs/internal/fixed-bugs/interpreter-i77-L5-reference-clear0-writes-a-subclass-field-named-referent-FIXED-20261111.md`.
//
// HotSpot addresses `Reference.referent` / `Reference.queue` whatever the
// subclass declares. CratonVM's `java.lang.ref` natives resolved them BY NAME
// on the receiver's class, where the subclass's own field wins. Host, wave 77
// (binary w77a): the `enqueue()` bridge read the subclass's `queue` (the
// String "own-q") and handed it to `ReferenceQueue.enqueue` as the receiver,
// dying with `NullPointerException: ... "this.lock" is null` in both modes
// after the first two lines (the `clear()` bridge clears slot 0, right; only
// its registry retirement read the subclass's `queue`). With
// CRATONVM_REFERENCE_LAYOUT_MEMO=1 (`clear()`, `clear0()` and `enqueue()` on
// `Reference`'s own slots, resolved once per VM) the output is the block
// below in both modes.
//
// Run:
//   $J/javac -d out L5W77ReferenceFieldShadow.java
//   $J/java -cp out L5W77ReferenceFieldShadow                                           # HotSpot
//   $B --java-home $J/.. -cp out L5W77ReferenceFieldShadow                              # off: dies at enqueue
//   env CRATONVM_REFERENCE_LAYOUT_MEMO=1 $B --java-home $J/.. -cp out L5W77ReferenceFieldShadow
// (each CratonVM line also with --compatible and with --nojit).
//
// Wave 79 (L5): the two `soft-shadow` rows -- a queue-less `SoftReference`
// subclass that shadows both fields; the constructor bridge
// (`reference::ref_init_impl`) and the registry retirement now address
// `Reference`'s own slots too. `enqueue()` on a queue-less reference clears
// the referent and answers false (`ReferenceQueue.Null.enqueue`).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// weak-shadow ctor get=same own-referent=null own-queue=null
// weak-shadow clear get=null own-referent=own own-queue=own-q
// weak-shadow enqueue=true polled=same
// phantom-shadow clear refersTo=false own-referent=own
// plain clear get=null
// plain enqueue=true polled=same
// soft-shadow ctor get=same own-referent=null own-queue=null
// soft-shadow enqueue=false get=null own-referent=null own-queue=null
// HOTSPOT_EXPECTED_END

import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;

public class L5W77ReferenceFieldShadow {
    static final class WeakShadow extends WeakReference<Object> {
        Object referent;
        Object queue;

        WeakShadow(Object o, ReferenceQueue<Object> q) {
            super(o, q);
        }
    }

    static final class PhantomShadow extends PhantomReference<Object> {
        Object referent;

        PhantomShadow(Object o, ReferenceQueue<Object> q) {
            super(o, q);
        }
    }

    static final class SoftShadow extends SoftReference<Object> {
        Object referent;
        Object queue;

        SoftShadow(Object o) {
            super(o);
        }
    }

    static final class Plain extends WeakReference<Object> {
        Plain(Object o, ReferenceQueue<Object> q) {
            super(o, q);
        }
    }

    static String same(Object got, Object want) {
        return got == null ? "null" : got == want ? "same" : "other";
    }

    static String text(Object o) {
        return o == null ? "null" : o.toString();
    }

    public static void main(String[] args) {
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        Object o = new Object();

        WeakShadow w = new WeakShadow(o, q);
        System.out.println("weak-shadow ctor get=" + same(w.get(), o)
                + " own-referent=" + text(w.referent) + " own-queue=" + text(w.queue));
        w.referent = "own";
        w.queue = "own-q";
        w.clear();
        System.out.println("weak-shadow clear get=" + same(w.get(), o)
                + " own-referent=" + text(w.referent) + " own-queue=" + text(w.queue));
        boolean enq = w.enqueue();
        Reference<?> polled = q.poll();
        System.out.println("weak-shadow enqueue=" + enq + " polled=" + same(polled, w));

        PhantomShadow p = new PhantomShadow(o, q);
        p.referent = "own";
        p.clear();
        System.out.println("phantom-shadow clear refersTo=" + p.refersTo(o)
                + " own-referent=" + text(p.referent));

        Plain plain = new Plain(o, q);
        plain.clear();
        System.out.println("plain clear get=" + same(plain.get(), o));
        boolean penq = plain.enqueue();
        Reference<?> ppolled = q.poll();
        System.out.println("plain enqueue=" + penq + " polled=" + same(ppolled, plain));
        SoftShadow s = new SoftShadow(o);
        System.out.println("soft-shadow ctor get=" + same(s.get(), o)
                + " own-referent=" + text(s.referent) + " own-queue=" + text(s.queue));
        boolean senq = s.enqueue();
        System.out.println("soft-shadow enqueue=" + senq + " get=" + same(s.get(), o)
                + " own-referent=" + text(s.referent) + " own-queue=" + text(s.queue));
        Reference.reachabilityFence(o);
    }
}
