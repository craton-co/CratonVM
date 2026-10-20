// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;

/**
 * gc JNI retirement (2026-10-10): a {@code WeakReference} to a strongly
 * reachable lambda must not be cleared by a collection.
 *
 * <p>The post-collection restore of a weak referent checks the referent's
 * class against a stamp the pre-collection pass recorded. The stamp kept its
 * "kind known" / "array" bits in bits 31 / 30 of a {@code u32}, and a lambda
 * proxy's class id is a counter from {@code 0x8000_0000}, so every lambda
 * referent read back as the wrong class: the restore was refused and the
 * reference cleared while the lambda was still in use. JNA reaches an
 * interface-mapped callback through exactly such a reference, so its callbacks
 * silently stopped running ({@code tools/bench/GcJniLibrariesProbe.java},
 * {@code jna-upcall} / {@code jna-native-threads}). Fixed in
 * {@code gc/src/reference.rs} ({@code REFERENT_STAMP_KIND_KNOWN} is bit 63 of
 * a {@code u64} stamp now).
 *
 * <p>A churn thread keeps the young generation collecting; each round holds a
 * referent strongly, lets collections run, then asks the reference for it.
 * HotSpot prints, and exits 0:
 * <pre>
 *   holder: PASS
 *   anon: PASS
 *   lambda: PASS
 *   PASS all 3
 * </pre>
 * A failing kind prints {@code <kind>: FAIL cleared=<n> of <rounds>}, the
 * summary {@code FAIL <n> of 3}, and exits 1.
 *
 * <pre>
 *   javac -d /tmp/lwr tools/bench/GcLambdaWeakRefProbe.java
 *   cratonvm --java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp /tmp/lwr GcLambdaWeakRefProbe
 * </pre>
 */
public final class GcLambdaWeakRefProbe {

    public interface Counter {
        int bump();
    }

    static final class Holder implements Counter {
        final int[] cell;

        Holder(int[] cell) {
            this.cell = cell;
        }

        @Override
        public int bump() {
            return ++cell[0];
        }
    }

    static volatile Object sink;
    static volatile boolean churning = true;

    static int cleared(String kind, int rounds) {
        int cleared = 0;
        for (int round = 0; round < rounds; round++) {
            int[] cell = {0};
            Counter c;
            switch (kind) {
                case "holder":
                    c = new Holder(cell);
                    break;
                case "anon":
                    c = new Counter() {
                        @Override
                        public int bump() {
                            return ++cell[0];
                        }
                    };
                    break;
                default:
                    c = () -> ++cell[0];
                    break;
            }
            WeakReference<Counter> ref = new WeakReference<>(c);
            for (int j = 0; j < 2000; j++) {
                sink = new byte[64];
            }
            if (ref.get() != c) {
                cleared++;
            }
            c.bump();
        }
        return cleared;
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 3000;
        Thread churn = new Thread(() -> {
            Object[] ring = new Object[64];
            int i = 0;
            while (churning) {
                ring[i++ & 63] = new byte[(i & 7) * 1024 + 16];
            }
        }, "churn");
        churn.setDaemon(true);
        churn.start();
        int failed = 0;
        for (String kind : new String[] {"holder", "anon", "lambda"}) {
            int n = cleared(kind, rounds);
            if (n == 0) {
                System.out.println(kind + ": PASS");
            } else {
                System.out.println(kind + ": FAIL cleared=" + n + " of " + rounds);
                failed++;
            }
        }
        churning = false;
        churn.join();
        System.out.println(failed == 0 ? "PASS all 3" : "FAIL " + failed + " of 3");
        System.exit(failed == 0 ? 0 : 1);
    }
}
