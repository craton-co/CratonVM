import org.apache.cassandra.utils.FBUtilities;
import org.apache.cassandra.utils.UUIDGen;
import org.apache.cassandra.utils.ByteBufferUtil;
import java.nio.ByteBuffer;
import java.util.UUID;

public class CassandraFuncProbe {
    public static void main(String[] args) throws Exception {
        System.out.println("Cassandra release: " + FBUtilities.getReleaseVersionString());
        UUID u1 = UUID.randomUUID();
        ByteBuffer bb = UUIDGen.toByteBuffer(u1);
        UUID u2 = UUIDGen.getUUID(bb);
        if (!u1.equals(u2)) {
            System.out.println("FAIL: UUID round-trip mismatch: " + u1 + " -> " + u2);
            System.exit(1);
        }
        System.out.println("UUID round-trip OK");  // don't print the UUID — baselines must be deterministic
        ByteBuffer sbb = ByteBufferUtil.bytes("cassandra-probe-payload");
        String s = ByteBufferUtil.string(sbb);
        if (!"cassandra-probe-payload".equals(s)) {
            System.out.println("FAIL: ByteBufferUtil round-trip mismatch: '" + s + "'");
            System.exit(1);
        }
        System.out.println("ByteBufferUtil round-trip OK");
        System.out.println("OK");
    }
}
