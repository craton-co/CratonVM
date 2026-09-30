// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 24, lane L4: a record's generated `toString()`
// starts with `Class.getSimpleName()`, which HotSpot reads from the record's
// OWN `InnerClasses` entry. CratonVM renders it itself
// (`vm/src/runtime/invokedynamic.rs`, `record_simple_name`) and took the
// segment after the last `$`, so a TOP-LEVEL record whose name contains `$`
// lost its prefix. Nested, local and `$`-named member records are controls.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W24RecordSimpleName
//
// HotSpot 25 (25.0.3) prints:
//   top-level A$B: Top$Rec[x=1]
//   nested: Nested[y=2]
//   nested with $: In$ner[z=3]
//   local: Local[w=4]
//   simple names: Top$Rec Nested In$ner Local
//
// Before wave 24 (read from the code, not run): `top-level A$B` printed
// `Rec[x=1]` and `nested with $` printed `ner[z=3]`.

public class L4W24RecordSimpleName {
    record Nested(int y) {}
    record In$ner(int z) {}

    public static void main(String[] args) {
        record Local(int w) {}
        System.out.println("top-level A$B: " + new Top$Rec(1));
        System.out.println("nested: " + new Nested(2));
        System.out.println("nested with $: " + new In$ner(3));
        System.out.println("local: " + new Local(4));
        System.out.println("simple names: " + Top$Rec.class.getSimpleName() + " "
                + Nested.class.getSimpleName() + " " + In$ner.class.getSimpleName() + " "
                + Local.class.getSimpleName());
    }
}

record Top$Rec(int x) {}
