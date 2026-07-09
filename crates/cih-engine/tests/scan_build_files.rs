use cih_engine::scan::build_files::*;
use std::path::PathBuf;

#[test]
fn parse_maven_manifest_reads_artifact_deps_and_modules() {
    let pom = r#"
        <project>
          <parent><groupId>com.acme</groupId><artifactId>parent</artifactId></parent>
          <artifactId>payments</artifactId>
          <modules><module>api</module><module>core</module></modules>
          <dependencies>
            <dependency><groupId>com.acme</groupId><artifactId>core</artifactId></dependency>
            <dependency><groupId>org.springframework</groupId><artifactId>spring-web</artifactId></dependency>
          </dependencies>
        </project>
    "#;
    let meta = parse_pom(pom).unwrap();
    assert_eq!(meta.group_id, "com.acme");
    assert_eq!(meta.artifact_id, "payments");
    assert_eq!(meta.modules, vec!["api", "core"]);
    assert_eq!(
        meta.deps,
        vec!["com.acme:core", "org.springframework:spring-web"]
    );
}

#[test]
fn parse_gradle_manifest_reads_group_external_and_project_deps() {
    let repo = PathBuf::from("/tmp/infra");
    let gradle = r#"
        group = "com.acme"
        dependencies {
          implementation("com.acme:core:1.0.0")
          api(project(":shared:model"))
        }
    "#;
    let meta = parse_gradle(gradle, &repo).unwrap();
    assert_eq!(meta.group_id, "com.acme");
    assert_eq!(meta.artifact_id, "infra");
    assert_eq!(meta.deps, vec!["com.acme:core", "com.acme:model"]);
}

#[test]
fn gradle_settings_include_paths_are_normalized() {
    let includes = parse_gradle_includes(r#"include(":api", ":services:billing")"#);
    assert_eq!(includes, vec!["api", "services/billing"]);
}

#[test]
fn package_json_includes_peer_and_optional_dependencies() {
    let meta = parse_package_json(
        r#"
        {
          "name": "@acme/app",
          "dependencies": { "@acme/core": "1.0.0" },
          "devDependencies": { "typescript": "^5.0.0" },
          "peerDependencies": { "react": "^18.0.0" },
          "optionalDependencies": { "fsevents": "^2.0.0" }
        }
        "#,
    )
    .unwrap();

    assert_eq!(meta.group_id, "@acme");
    assert_eq!(meta.artifact_id, "app");
    assert_eq!(
        meta.deps,
        vec![
            "@acme/core".to_string(),
            "fsevents".to_string(),
            "react".to_string(),
            "typescript".to_string()
        ]
    );
}

#[test]
fn pyproject_normalizes_project_and_dependency_names() {
    let meta = parse_pyproject_toml(
        r#"
        [project]
        name = "My_Service"
        dependencies = [
          "Other.Service>=1.0",
          "Third_Party[httpx]>=2.0; python_version >= '3.10'"
        ]
        "#,
    )
    .unwrap();

    assert_eq!(meta.artifact_id, "my-service");
    assert_eq!(
        meta.deps,
        vec!["other-service".to_string(), "third-party".to_string()]
    );
}

#[test]
fn setup_cfg_normalizes_project_and_dependency_names() {
    let meta = parse_setup_cfg(
        r#"
        [metadata]
        name = My.Service

        [options]
        install_requires =
            Other_Service>=1.0
            third-party[uvicorn]>=2.0
        "#,
    )
    .unwrap();

    assert_eq!(meta.artifact_id, "my-service");
    assert_eq!(
        meta.deps,
        vec!["other-service".to_string(), "third-party".to_string()]
    );
}
