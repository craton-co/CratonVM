use super::*;
use cratonvm_native_api::NativeKind;
use cratonvm_types::compat::CompatibilityMode;

#[test]
fn synthetic_process_methods_are_refused_in_jdk_only_mode() {
    let mut compatible = NativeMethodRegistry::new();
    register_process_natives(&mut compatible);
    let mut strict = NativeMethodRegistry::new();
    strict.set_compatibility_mode(CompatibilityMode::JdkOnly);
    register_process_natives(&mut strict);
    for (name, descriptor) in [
        ("waitFor", "()I"),
        ("waitFor", "(JLjava/util/concurrent/TimeUnit;)Z"),
        ("exitValue", "()I"),
        ("isAlive", "()Z"),
        ("destroy", "()V"),
        ("destroyForcibly", "()Ljava/lang/Process;"),
        ("pid", "()J"),
        ("toHandle", "()Ljava/lang/ProcessHandle;"),
        ("getInputStream", "()Ljava/io/InputStream;"),
        ("getErrorStream", "()Ljava/io/InputStream;"),
        ("getOutputStream", "()Ljava/io/OutputStream;"),
        ("descendants", "()Ljava/util/stream/Stream;"),
        ("onExit", "()Ljava/util/concurrent/CompletableFuture;"),
    ] {
        assert_eq!(
            compatible.kind_of(SYNTHETIC_PROCESS_CLASS, name, descriptor),
            Some(NativeKind::SyntheticStub)
        );
        assert!(strict
            .find(SYNTHETIC_PROCESS_CLASS, name, descriptor)
            .is_none());
        assert_eq!(
            strict.kind_of("java/lang/Process", name, descriptor),
            Some(NativeKind::Bridge)
        );
    }
}
