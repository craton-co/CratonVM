// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Fixture for `cli_javaagent_transform.rs`.
 *
 * <p>The marker string is exactly eight ASCII characters so the agent can swap
 * it for another eight-character string by byte substitution in the constant
 * pool, without an ASM dependency and without shifting a single offset in the
 * class file. What the test asserts is that the swapped bytes are the ones the
 * VM actually defined the class from.
 *
 * <p>Deliberately free of string concatenation: `+` on a non-constant compiles
 * to `invokedynamic makeConcatWithConstants`, and a failure in that machinery
 * would fail this test for a reason that has nothing to do with agents.
 */
public class XformTarget {
    static String marker() {
        return "ORIGINAL";
    }

    public static void main(String[] args) {
        System.out.println(marker());
    }
}
