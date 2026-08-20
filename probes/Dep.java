/**
 * The class `JarManifestClassPathProbe` puts in `dep.jar` and then tries to
 * load through `main.jar`'s manifest `Class-Path` alone.
 *
 * Deliberately empty and deliberately in the default package: the probe loads
 * it by the name `Dep` through a loader whose parent is the PLATFORM loader,
 * so nothing about it may depend on the application class path.
 */
public final class Dep {
    private Dep() {}
}
