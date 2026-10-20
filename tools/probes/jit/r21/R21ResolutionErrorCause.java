// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 21: item 6(c) of docs/known-issues/jit/r21-review-w21-findings-20261019.md.
// JVMS 5.4.3: a failed resolution of a constant-pool entry is recorded, and later attempts throw the
// same error. HotSpot's resolution-error table also keeps the cause, so the second NoClassDefFoundError
// still carries the ClassNotFoundException. The nested class is deleted after javac, so `new Gone()`
// fails; the loop runs hot so the compiled caller takes the same path.
//
// Run:
//   javac -d out R21ResolutionErrorCause.java && rm 'out/R21ResolutionErrorCause$Gone.class'
//   java -Xmx256m -cp out R21ResolutionErrorCause          # HotSpot 25
//   cratonvm -Xmx256m -cp out R21ResolutionErrorCause      (and --compatible, --nojit)
// HotSpot expectation (confirmed on 25.0.3): every line reads
//   `java.lang.NoClassDefFoundError: R21ResolutionErrorCause$Gone cause=java.lang.ClassNotFoundException: R21ResolutionErrorCause$Gone`

public class R21ResolutionErrorCause {
    static class Gone {
    }

    static Object make() {
        return new Gone();
    }

    public static void main(String[] args) {
        String first = null;
        int same = 0;
        for (int i = 0; i < 20_000; i++) {
            try {
                make();
            } catch (Throwable t) {
                String s = t + " cause=" + t.getCause();
                if (i < 2) {
                    System.out.println(i + " " + s);
                    first = first == null ? s : first;
                } else if (s.equals(first)) {
                    same++;
                }
            }
        }
        System.out.println("hot same=" + same);
    }
}
