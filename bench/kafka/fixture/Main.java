// bench/kafka/fixture/Main.java
// WP8.7 placeholder fixture for Apache Kafka 3 (broker) forcing-function smoke.
//
// Real Kafka smoke runs $KAFKA_HOME/bin/kafka-server-start.sh against a stripped server.properties (KRaft).
// Placeholder probes: Direct ByteBuffer (log segments).
// Today's baseline pins the WP0.1 println NPE failure mode
// (memory/finding_println_regression.md). When WP0.1 lands and the rc flips
// to 0, bench-baseline.json should be updated in the same PR.
public class Main {
    public static void main(String[] args) throws Exception {
        System.out.println("kafka_fixture: starting baseline smoke");
        System.out.println("kafka_fixture: Direct ByteBuffer (log segments) probe");
        java.nio.ByteBuffer b = java.nio.ByteBuffer.allocateDirect(64);
        b.putInt(0xCAFEBABE);
        if (b.getInt(0) != 0xCAFEBABE) throw new AssertionError("dbb roundtrip");
        System.out.println("kafka_fixture: ok");
    }
}
