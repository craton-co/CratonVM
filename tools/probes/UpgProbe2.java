// org.h2.tools.Upgrade of a 1.2.120 database in one process, with or without the current H2 warmed first. Classpath: <h2 target/classes>.
//
// Record: nonpassed-classbyclass-census-RESOLVED-20260923.md (D5, D6)
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] UpgProbe2 up|warm+up
// Compare with the same command on HotSpot (java -cp ...).
import java.sql.*;
import java.util.Properties;

public class UpgProbe2 {
    public static void main(String[] a) throws Exception {
        String mode = a[0];
        if (mode.contains("warm")) {
            try (Connection c = DriverManager.getConnection("jdbc:h2:./warm;COMPRESS=TRUE", "sa", "")) {
                Statement st = c.createStatement();
                st.execute("CREATE TABLE W(ID INT PRIMARY KEY, S VARCHAR)");
                for (int i = 0; i < 2000; i++) st.execute("INSERT INTO W VALUES(" + i + ", 'x" + i + "')");
                st.execute("DROP ALL OBJECTS DELETE FILES");
            }
            byte[] in = new byte[4096]; byte[] out = new byte[9000];
            org.h2.compress.CompressLZF lzf = new org.h2.compress.CompressLZF();
            for (int i = 0; i < 20000; i++) { in[i % 4096] = (byte) i; lzf.compress(in, 0, in.length, out, 0); }
            System.out.println("warmed new h2");
        }
        byte[] bytes = new byte[10_000];
        new java.util.Random(1).nextBytes(bytes);
        String s = new String(bytes, java.nio.charset.StandardCharsets.ISO_8859_1);
        String url = "jdbc:h2:./up2/testUpgrade";
        Properties p = new Properties(); p.put("user", "sa"); p.put("password", "password");
        Driver d = org.h2.tools.Upgrade.loadH2(120);
        try (Connection c = d.connect(url, p)) {
            c.createStatement().execute("CREATE TABLE TEST(ID BIGINT AUTO_INCREMENT PRIMARY KEY, B BINARY, L BLOB, C CLOB)");
            PreparedStatement prep = c.prepareStatement("INSERT INTO TEST(B, L, C) VALUES (?, ?, ?)");
            prep.setBytes(1, bytes); prep.setBytes(2, bytes); prep.setString(3, s); prep.execute();
            System.out.println("old phase ok");
        } catch (Throwable t) {
            Throwable c = t; while (c.getCause() != null) c = c.getCause();
            System.out.println("old phase FAIL " + c);
            for (StackTraceElement e : c.getStackTrace()) System.out.println("    " + e);
            return;
        } finally { org.h2.tools.Upgrade.unloadH2(d); }
        if (mode.contains("up")) {
            try {
                System.out.println("upgrade=" + org.h2.tools.Upgrade.upgrade(url, p, 120));
                try (Connection c = DriverManager.getConnection(url, p); ResultSet rs = c.createStatement().executeQuery("TABLE TEST")) {
                    rs.next();
                    System.out.println("new phase ok eq=" + java.util.Arrays.equals(bytes, rs.getBytes(2)));
                }
            } catch (Throwable t) {
                Throwable c = t; while (c.getCause() != null) c = c.getCause();
                System.out.println("new phase FAIL " + c);
                for (StackTraceElement e : c.getStackTrace()) System.out.println("    " + e);
            }
        }
    }
}
