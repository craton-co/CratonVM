package org.hibernate.orm.test.id.uuid.rfc9562;

public final class UuidExactProbe {
    public static void main(String[] args) {
        long started = System.nanoTime();
        new UUidV6V7GeneratorTest().testMonotonicityUuid6();
        System.out.println("@@EXACT uuid6_ms=" + ((System.nanoTime() - started) / 1_000_000));
    }
}
