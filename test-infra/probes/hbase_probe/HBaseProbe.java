import org.apache.hadoop.conf.Configuration;
import org.apache.hadoop.hbase.HConstants;

public class HBaseProbe {
    // NOTE: TableName.valueOf() touches Bytes$LexicographicalComparerHolder
    // which currently trips an AIOOBE in CratonVM — tracked as a separate VM
    // bug. Keep this probe scoped to Configuration + HConstants so it can
    // serve as a regression baseline today; expand once that's fixed.
    public static void main(String[] args) {
        try {
            // 1. HBase Configuration uses Hadoop Configuration underneath.
            // Set HBase keys + verify round-trip.
            Configuration conf = new Configuration(false);
            conf.set(HConstants.ZOOKEEPER_QUORUM, "zk1,zk2,zk3");
            conf.setInt(HConstants.ZOOKEEPER_CLIENT_PORT, 2181);
            conf.set(HConstants.HBASE_DIR, "hdfs://nn:9000/hbase");

            if (!"zk1,zk2,zk3".equals(conf.get(HConstants.ZOOKEEPER_QUORUM))) {
                System.out.println("FAIL: ZK quorum"); System.exit(1);
            }
            if (conf.getInt(HConstants.ZOOKEEPER_CLIENT_PORT, 0) != 2181) {
                System.out.println("FAIL: ZK port"); System.exit(1);
            }
            System.out.println("Configuration OK: " + conf.get(HConstants.HBASE_DIR));

            // 2. HConstants surface — verify a handful of well-known constants
            // are non-null / sensible (catches static-init breakage).
            if (HConstants.UTF8_ENCODING == null || HConstants.LATEST_TIMESTAMP <= 0) {
                System.out.println("FAIL: HConstants surface"); System.exit(1);
            }
            System.out.println("HConstants OK (utf8=" + HConstants.UTF8_ENCODING
                + ", latest_ts=" + HConstants.LATEST_TIMESTAMP + ")");

            System.out.println("OK");
        } catch (Throwable t) {
            t.printStackTrace();
            System.exit(1);
        }
        System.exit(0);
    }
}
