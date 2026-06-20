use super::*;

#[test]
fn ignore_rules_cover_directories_files_and_extensions() {
    assert!(should_ignore_path("target/generated/App.java"));
    assert!(should_ignore_path("lib/example.jar"));
    assert!(should_ignore_path("src/main/App.generated.java"));
    assert!(should_ignore_path("Cargo.lock"));
    assert!(!should_ignore_path("src/main/java/com/acme/App.java"));
}
