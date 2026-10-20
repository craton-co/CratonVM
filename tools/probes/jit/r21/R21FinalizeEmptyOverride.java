// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 17: item 4 of docs/known-issues/jit/r21-review-w16-findings-20261019.md.
// HotSpot's ClassFileParser marks a finalize()V whose code is a lone `return` as an empty finalizer:
// the class gets no has_finalizer bit, and an empty override cancels an inherited finalizer, so its
// instances are never registered with java.lang.ref.Finalizer and become phantom-reachable after one GC.
// Switch: CRATONVM_FINALIZER_EMPTY_IS_NONE (default ON; =0 is the control).
//
// Run:
//   javac -d out R21FinalizeEmptyOverride.java
//   java -Xmx256m -cp out R21FinalizeEmptyOverride          # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FinalizeEmptyOverride      (and --compatible, --nojit)
// HotSpot expectation (confirmed on 25.0.3 with -XX:+UseSerialGC and the default G1):
//   plain registered=no / finalizer registered=yes / empty-override registered=no / empty-own registered=no
//   / override-of-empty registered=yes

import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Supplier;

public class R21FinalizeEmptyOverride {
    static final int N = 1000;

    static class Plain {
    }

    static class Fin {
        int x;

        @Override
        @SuppressWarnings("removal")
        protected void finalize() {
            x++;
        }
    }

    static class EmptyOverride extends Fin {
        @Override
        @SuppressWarnings("removal")
        protected void finalize() {
        }
    }

    static class EmptyOwn {
        @Override
        @SuppressWarnings("removal")
        protected void finalize() {
        }
    }

    static class OverrideOfEmpty extends EmptyOverride {
        int y;

        @Override
        @SuppressWarnings("removal")
        protected void finalize() {
            y++;
        }
    }

    static String registered(Supplier<Object> s) {
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        List<Reference<?>> keep = new ArrayList<>();
        for (int i = 0; i < N; i++) {
            keep.add(new PhantomReference<>(s.get(), q));
        }
        System.gc();
        try {
            Thread.sleep(200);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        int n = 0;
        while (q.poll() != null) {
            n++;
        }
        // A registered object is held by its Finalizer until finalize() ran and a later GC
        // found it again, so after one GC none of the N is phantom-enqueued.
        return n == 0 ? "yes" : (n == N ? "no" : "partial");
    }

    public static void main(String[] args) {
        System.out.println("plain registered=" + registered(Plain::new));
        System.out.println("finalizer registered=" + registered(Fin::new));
        System.out.println("empty-override registered=" + registered(EmptyOverride::new));
        System.out.println("empty-own registered=" + registered(EmptyOwn::new));
        System.out.println("override-of-empty registered=" + registered(OverrideOfEmpty::new));
    }
}
