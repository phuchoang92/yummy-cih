use super::*;
use std::fs;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

fn tmp_dir(tag: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("cih-jars-{tag}-{unique}"));
    fs::create_dir_all(&p).unwrap();
    p
}

/// Create a minimal valid ZIP at `path` with `class_count` fake `.class` entries.
fn write_fake_jar(path: &Path, class_count: usize) {
    let file = fs::File::create(path).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    let opts = zip::write::FileOptions::<()>::default()
        .compression_method(zip::CompressionMethod::Stored);
    for i in 0..class_count {
        writer
            .start_file(format!("com/example/Class{i}.class"), opts)
            .unwrap();
        writer.write_all(b"CAFEBABE").unwrap();
    }
    writer.finish().unwrap();
}

#[test]
fn discover_jars_finds_local_lib_jars() {
    let root = tmp_dir("lib");
    let lib_dir = root.join("lib");
    fs::create_dir_all(&lib_dir).unwrap();
    let jar_path = lib_dir.join("guava-33.0.jar");
    write_fake_jar(&jar_path, 5);

    let jars = discover_jars(&root, &[], "com.example");
    assert_eq!(jars.len(), 1);
    assert_eq!(jars[0].path, jar_path.to_string_lossy());
    assert_eq!(jars[0].artifact.as_deref(), Some("guava-33.0"));
    assert_eq!(jars[0].group_id, None);
    assert!(!jars[0].is_own);
    assert_eq!(jars[0].classes, 5);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn sources_and_javadoc_jars_are_excluded() {
    let root = tmp_dir("exclude");
    let lib_dir = root.join("libs");
    fs::create_dir_all(&lib_dir).unwrap();
    write_fake_jar(&lib_dir.join("core-1.0.jar"), 2);
    write_fake_jar(&lib_dir.join("core-1.0-sources.jar"), 2);
    write_fake_jar(&lib_dir.join("core-1.0-javadoc.jar"), 2);
    write_fake_jar(&lib_dir.join("core-1.0-tests.jar"), 2);

    let jars = discover_jars(&root, &[], "");
    assert_eq!(jars.len(), 1);
    assert_eq!(jars[0].artifact.as_deref(), Some("core-1.0"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn is_own_matches_exact_and_prefix() {
    assert!(is_own("com.example", "com.example"));
    assert!(is_own("com.example.sub", "com.example"));
    assert!(!is_own("com.other", "com.example"));
    assert!(!is_own("com.examplefoo", "com.example"));
    assert!(!is_own("org.springframework", ""));
}

#[test]
fn group_artifact_from_maven_path() {
    let path = PathBuf::from(
        "/home/user/.m2/repository/org/springframework/spring-web/6.0.0/spring-web-6.0.0.jar",
    );
    let (group, artifact) = group_artifact_from_path(&path);
    assert_eq!(group.as_deref(), Some("org.springframework"));
    assert_eq!(artifact.as_deref(), Some("spring-web"));
}

#[test]
fn group_artifact_from_gradle_path() {
    let path = PathBuf::from(
        "/home/user/.gradle/caches/modules-2/files-2.1/com.google.guava/guava/33.0-jre/abcdef1234/guava-33.0-jre.jar",
    );
    let (group, artifact) = group_artifact_from_path(&path);
    assert_eq!(group.as_deref(), Some("com.google.guava"));
    assert_eq!(artifact.as_deref(), Some("guava"));
}

#[test]
fn count_jar_classes_counts_correctly() {
    let dir = tmp_dir("count");
    let jar_path = dir.join("test.jar");
    write_fake_jar(&jar_path, 7);
    assert_eq!(count_jar_classes(&jar_path).unwrap(), 7);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn count_jar_classes_returns_zero_for_invalid_zip() {
    let dir = tmp_dir("invalid");
    let jar_path = dir.join("notazip.jar");
    fs::write(&jar_path, b"not a zip").unwrap();
    assert_eq!(count_jar_classes(&jar_path).unwrap_or(0), 0);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn discover_jars_marks_own_group_jars() {
    let root = tmp_dir("own");
    let lib_dir = root.join("lib");
    fs::create_dir_all(&lib_dir).unwrap();
    // Fake Maven layout so group_artifact_from_path can extract the group.
    let m2_jar_dir = lib_dir
        .join("repository")
        .join("com")
        .join("example")
        .join("core")
        .join("1.0");
    fs::create_dir_all(&m2_jar_dir).unwrap();
    write_fake_jar(&m2_jar_dir.join("core-1.0.jar"), 1);

    let jars = discover_jars(&root, &[], "com.example");
    assert_eq!(jars.len(), 1);
    assert_eq!(jars[0].group_id.as_deref(), Some("com.example"));
    assert!(jars[0].is_own);

    let _ = fs::remove_dir_all(root);
}
