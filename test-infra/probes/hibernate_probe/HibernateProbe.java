import org.hibernate.Version;
import org.hibernate.cfg.Configuration;
import org.hibernate.dialect.H2Dialect;
public class HibernateProbe {
    public static void main(String[] args) {
        try {
            System.out.println("Hibernate version: " + Version.getVersionString());
            Configuration cfg = new Configuration();
            cfg.setProperty("hibernate.dialect", H2Dialect.class.getName());
            System.out.println("Dialect set: " + cfg.getProperty("hibernate.dialect"));
            System.out.println("OK");
        } catch (Throwable t) { t.printStackTrace(); System.exit(1); }
        System.exit(0);
    }
}
