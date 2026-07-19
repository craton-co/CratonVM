# NestedJarFile super-close ZipFile native bypass

Status: FIXED (2026-07-18)

`NestedJarFile.close()` contains `invokespecial java/util/jar/JarFile.close()V`.
`JarFile` inherits that concrete method from `ZipFile`; when CratonVM native
JarFile construction left the real JDK `ZipFile.res` field unset, interpreting
the inherited body threw a null `CleanableResource.clean()` NPE.

The fix admits the registered ZipFile bridge methods to the cold override
policy and its warmed-cache counterpart. It also routes the exact
`invokespecial JarFile/ZipFile.close()V` shape to the registered ZipFile
bridge before Mockito's redefine guard. The guard still applies to ordinary
virtual mock calls.

`vm/tests/zipfile_inherited_super_native_bridge.rs` covers the first and
warmed inherited-super close dispatches in JIT and `--nojit`. Spring Boot
`JarUrlConnectionTests` and `UrlJarFilesTests` were rerun in both modes using
the task-specific binary: the documented `ZipFile$CleanableResource.clean()`
NPE is absent in all four application runs.

Remaining application failures in those classes are separate: a cached-stream
identity assertion, a Mockito spy-observation assertion, and a
`NoSuchFileException` cache-path failure. None contains the resolved
ZipFile-close marker.
