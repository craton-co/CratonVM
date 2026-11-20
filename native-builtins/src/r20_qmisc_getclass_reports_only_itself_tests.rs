use super::*;

/// NIO20-L3-1: every key of the alias table is listed, and each of its
/// answers is a target the predicate refuses.
#[test]
fn every_alias_row_is_listed_and_refused() {
    for stamp in GETCLASS_ALIAS_SOURCES {
        assert!(!getclass_reports_only_itself(stamp), "{stamp}");
        // The switch-gated row may answer `None` with its switch off.
        if let Some(aliases) = jdk_concrete_getclass_alias(stamp) {
            for alias in aliases {
                assert!(!getclass_reports_only_itself(alias), "{stamp} -> {alias}");
            }
        }
    }
    for stamp in [
        "java/nio/file/FileSystem",
        "java/util/stream/Stream",
        "javax/net/ssl/SSLSocket",
    ] {
        assert!(jdk_concrete_getclass_alias(stamp).is_some(), "{stamp}");
    }
}

/// The collection stamps, their answers, the stream stages and the fs
/// mounts are refused; ordinary classes are not.
#[test]
fn remapped_and_reported_names_are_refused() {
    for name in [
        "cratonvm/internal/UnmodifiableList",
        "cratonvm/internal/UnmodifiableItr",
        "cratonvm/internal/HucBodyStream",
        "java/util/ArrayList$Itr",
        "java/util/Spliterator",
        "java/util/Vector$Itr",
        "java/util/ImmutableCollections$ListN",
        "java/util/Collections$UnmodifiableRandomAccessList",
        "java/util/Collections$UnmodifiableCollection$1",
        "java/util/stream/ReferencePipeline$Head",
        "java/util/stream/ReferencePipeline$2",
        "jdk/nio/zipfs/ZipFileSystem",
        "sun/net/www/protocol/http/HttpURLConnection$StreamingOutputStream",
        "[Ljava/lang/String;",
        "[I",
        "",
    ] {
        assert!(!getclass_reports_only_itself(name), "{name}");
    }
    for name in [
        "java/lang/String",
        "java/lang/Integer",
        "java/util/ArrayList",
        "java/util/HashMap",
        "probe/Foo",
    ] {
        assert!(getclass_reports_only_itself(name), "{name}");
    }
    for stage in cratonvm_native_collections::synthetic_stream_stage_display_names() {
        if !stage.is_empty() {
            assert!(!getclass_reports_only_itself(stage), "{stage}");
        }
    }
    for target in GETCLASS_DISPLAY_TARGETS {
        assert!(getclass_display_target_listed(target), "{target}");
    }
}
