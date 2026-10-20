// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
public class Box {
    public final int value;
    public Box(int value) { this.value = value; }
    public static Box self() { return new Box(12); }
    public static int read(Box box) { return box.value; }
}
