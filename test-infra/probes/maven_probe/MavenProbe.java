import org.apache.maven.cli.MavenCli;
import org.apache.maven.model.Dependency;
import org.apache.maven.model.Model;
import org.apache.maven.artifact.versioning.DefaultArtifactVersion;
import org.apache.maven.artifact.versioning.VersionRange;
public class MavenProbe {
    public static void main(String[] args) throws Exception {
        // 1. MavenCli class load (entry point of the maven launcher).
        System.out.println("MavenCli: " + MavenCli.class.getName());

        // 2. Build a Model object programmatically.
        Model m = new Model();
        m.setGroupId("com.probe");
        m.setArtifactId("probe");
        m.setVersion("1.0.0");
        m.setModelVersion("4.0.0");
        Dependency dep = new Dependency();
        dep.setGroupId("org.apache.commons");
        dep.setArtifactId("commons-lang3");
        dep.setVersion("3.14.0");
        m.addDependency(dep);
        System.out.println("Model: " + m.getGroupId() + ":" + m.getArtifactId() + ":" + m.getVersion());
        if (m.getDependencies().size() != 1) { System.out.println("FAIL: deps count"); System.exit(1); }

        // 3. Version comparator.
        DefaultArtifactVersion v1 = new DefaultArtifactVersion("1.0.0");
        DefaultArtifactVersion v2 = new DefaultArtifactVersion("1.2.3");
        if (v1.compareTo(v2) >= 0) { System.out.println("FAIL: version compare"); System.exit(1); }
        System.out.println("Version compare: " + v1 + " < " + v2 + " OK");

        // 4. Version range parse.
        VersionRange range = VersionRange.createFromVersionSpec("[1.0,2.0)");
        System.out.println("Range: " + range);

        System.out.println("OK");
        System.exit(0);
    }
}
