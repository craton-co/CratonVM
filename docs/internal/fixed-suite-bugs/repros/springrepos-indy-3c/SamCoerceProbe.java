import groovy.lang.GroovyClassLoader;
import org.gradle.api.Action;
import org.gradle.api.artifacts.repositories.MavenArtifactRepository;
import org.gradle.api.artifacts.repositories.MavenRepositoryContentDescriptor;
import static org.mockito.Mockito.mock;
import static org.mockito.BDDMockito.willAnswer;
import static org.mockito.ArgumentMatchers.any;

public class SamCoerceProbe {
  public static void main(String[] a) throws Throwable {
    GroovyClassLoader gcl = new GroovyClassLoader(SamCoerceProbe.class.getClassLoader());
    // Groovy: call mavenContent { it.snapshotsOnly() } on a mock (Action SAM coercion)
    String src =
      "class S {\n" +
      "  def go(repo) { repo.mavenContent { mc -> mc.snapshotsOnly() } }\n" +
      "}\n";
    Class<?> c = gcl.parseClass(src);
    Object s = c.getDeclaredConstructor().newInstance();

    MavenArtifactRepository repo = mock(MavenArtifactRepository.class);
    int[] fired = {0}, executed = {0};
    willAnswer(inv -> {
      fired[0]++;
      System.out.println("  mavenContent(Action) stub fired");
      Action<MavenRepositoryContentDescriptor> act = inv.getArgument(0);
      MavenRepositoryContentDescriptor mc = mock(MavenRepositoryContentDescriptor.class);
      willAnswer(ci -> { executed[0]++; System.out.println("  snapshotsOnly executed"); return null; })
        .given(mc).snapshotsOnly();
      act.execute(mc);
      return null;
    }).given(repo).mavenContent(any(Action.class));

    c.getDeclaredMethod("go", Object.class).invoke(s, repo);
    System.out.println("fired=" + fired[0] + " executed=" + executed[0] + " (expect 1 1)");
    System.out.println("SAMCOERCEPROBE_DONE");
  }
}
