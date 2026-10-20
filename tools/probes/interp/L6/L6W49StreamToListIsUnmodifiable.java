// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L6: `Stream.toList()` is the JDK's
// unmodifiable `ImmutableCollections$ListN` (nulls allowed) under
// `--compatible`, as on HotSpot and under `--jdk-only`. `--compatible`'s
// `Stream.toList` native handed out a MUTABLE `java.util.ArrayList`: the
// positive controls are `class=java.util.ArrayList` and `add=ok [a, b, c]`
// (switch `CRATONVM_COMPAT_JDK_IMMUTABLES`; stage 4 of
// `docs/internal/fixed-bugs/interpreter-L5-compatible-mode-list-of-is-cratonvms-own-list-class-FIXED-20261020.md`).
// `Collectors.toList()` stays mutable, as on HotSpot (control). The first row
// runs cold and again last.
//
// HotSpot 25.0.3 prints (measured; the same with -Xint):
//     class=java.util.ImmutableCollections$ListN
//     add=java.lang.UnsupportedOperationException
//     mapped=java.util.ImmutableCollections$ListN [X, Y] java.lang.UnsupportedOperationException
//     set=java.lang.UnsupportedOperationException
//     nulls=[a, null] 2
//     empty=[] java.util.ImmutableCollections$ListN
//     collectors-toList=[a, b]
//     class-again=java.util.ImmutableCollections$ListN
//
//     javac -d out L6W49StreamToListIsUnmodifiable.java
//     java|cratonvm [--compatible] [--nojit] -cp out L6W49StreamToListIsUnmodifiable
import java.util.*;
import java.util.concurrent.Callable;
import java.util.stream.*;

public class L6W49StreamToListIsUnmodifiable {
    static String t(Callable<?> r) {
        try {
            return "ok " + r.call();
        } catch (Throwable e) {
            return e.getClass().getName();
        }
    }

    public static void main(String[] args) {
        List<String> l = Stream.of("a", "b").toList();
        System.out.println("class=" + l.getClass().getName());
        System.out.println("add=" + t(() -> { l.add("c"); return l; }));
        List<String> m = List.of("x", "y").stream().map(String::toUpperCase).toList();
        System.out.println("mapped=" + m.getClass().getName() + " " + m + " " + t(() -> m.remove(0)));
        List<String> n = new ArrayList<>(List.of("p")).stream().toList();
        System.out.println("set=" + t(() -> n.set(0, "q")));
        List<String> nulls = Stream.of("a", null).toList();
        System.out.println("nulls=" + nulls + " " + nulls.size());
        List<Object> empty = Stream.empty().toList();
        System.out.println("empty=" + empty + " " + empty.getClass().getName());
        List<String> col = Stream.of("a").collect(Collectors.toList());
        col.add("b");
        System.out.println("collectors-toList=" + col);
        System.out.println("class-again=" + Stream.of("z").toList().getClass().getName());
    }
}
