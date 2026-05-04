import java.sql.*;

public class H2Step2 {
    // Step 2: only attempt to load the H2 driver class. No connection yet.
    public static void main(String[] args) throws Exception {
        Class<?> drv = Class.forName("org.h2.Driver");
        System.out.println("driver.loaded=" + drv.getName());
        Driver d = DriverManager.getDriver("jdbc:h2:mem:");
        System.out.println("driver.major=" + d.getMajorVersion() + " minor=" + d.getMinorVersion());
        System.out.println("H2Step2: PASS");
    }
}
