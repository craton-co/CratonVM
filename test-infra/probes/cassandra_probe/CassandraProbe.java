import org.apache.cassandra.utils.FBUtilities;
public class CassandraProbe {
    public static void main(String[] args) {
        try {
            String ver = FBUtilities.getReleaseVersionString();
            System.out.println("Cassandra release: " + ver);
            System.out.println("OK");
        } catch (Throwable t) { t.printStackTrace(); }
    }
}
