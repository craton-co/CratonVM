package com.cratonvm.jdkonly.svc;

/**
 * Lives in an EXPORTED but NOT OPENED package: callers may compile and link
 * against it, but deep reflection ({@code setAccessible}) on its private state
 * must be refused with {@code InaccessibleObjectException}.
 */
public final class Exported {
    private final String hidden = "exported-private";

    public Exported() {
    }

    public String visible() {
        return "exported-public";
    }

    /** Only reachable from inside the module. */
    String peek() {
        return hidden;
    }
}
