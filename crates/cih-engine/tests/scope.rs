use cih_core::{BuildSystem, ModuleInfo};
use cih_engine_lib::scan::OwnedSourceFile;
use cih_engine_lib::scope::*;

fn repo_map() -> cih_core::RepoMap {
    cih_core::RepoMap {
        root: "/repo".into(),
        build_system: BuildSystem::Maven,
        total_source_loc: 30,
        modules: vec![
            module("app", "app"),
            module("infra", "infra"),
            module(".workspace-dependencies", ".workspace-dependencies"),
        ],
        jars: Vec::new(),
        decompiled_dirs: vec![".workspace-dependencies".into()],
        architecture_hint: cih_core::ArchitectureHint::Unknown,
        total_source_files: 3,
        per_language: std::collections::BTreeMap::from([("java".into(), 3)]),
    }
}

fn module(name: &str, rel_path: &str) -> ModuleInfo {
    ModuleInfo {
        name: name.into(),
        rel_path: rel_path.into(),
        build_file: None,
        source_files: 1,
        source_loc: 10,
        packages: Vec::new(),
        depends_on: Vec::new(),
        frameworks: Vec::new(),
        per_language: std::collections::BTreeMap::from([("java".into(), 1)]),
    }
}

fn source_files() -> Vec<OwnedSourceFile> {
    vec![
        owned("app/src/main/java/App.java", Some("app")),
        owned("infra/src/main/java/Repo.java", Some("infra")),
        owned(
            ".workspace-dependencies/lib/src/main/java/Lib.java",
            Some(".workspace-dependencies"),
        ),
    ]
}

fn owned(rel: &str, module_rel: Option<&str>) -> OwnedSourceFile {
    let language = if rel.ends_with(".java") { "java" }
    else if rel.ends_with(".ts") || rel.ends_with(".tsx") { "typescript" }
    else if rel.ends_with(".py") { "python" }
    else { "java" };
    OwnedSourceFile {
        rel: rel.into(),
        module_rel: module_rel.map(str::to_string),
        language: language.into(),
    }
}

#[test]
fn module_selector_keeps_only_that_modules_files() {
    let scope = resolve(
        &repo_map(),
        &source_files(),
        ScopeRequest {
            modules: vec!["app".into()],
            ..ScopeRequest::default()
        },
    )
    .unwrap();

    assert_eq!(scope.file_count, 1);
    assert_eq!(scope.modules, vec!["app"]);
    assert_eq!(scope.files, vec!["app/src/main/java/App.java"]);
}

#[test]
fn all_excludes_decompiled_unless_requested() {
    let without_decompiled = resolve(
        &repo_map(),
        &source_files(),
        ScopeRequest {
            all: true,
            ..ScopeRequest::default()
        },
    )
    .unwrap();
    assert_eq!(without_decompiled.file_count, 2);

    let with_decompiled = resolve(
        &repo_map(),
        &source_files(),
        ScopeRequest {
            all: true,
            include_decompiled: true,
            ..ScopeRequest::default()
        },
    )
    .unwrap();
    assert_eq!(with_decompiled.file_count, 3);
}

#[test]
fn exclude_glob_removes_matches_after_selection() {
    let scope = resolve(
        &repo_map(),
        &source_files(),
        ScopeRequest {
            all: true,
            exclude: vec!["infra/**".into()],
            ..ScopeRequest::default()
        },
    )
    .unwrap();

    assert_eq!(scope.files, vec!["app/src/main/java/App.java"]);
}

#[test]
fn same_inputs_produce_same_version() {
    let request = ScopeRequest {
        all: true,
        ..ScopeRequest::default()
    };
    let first = resolve(&repo_map(), &source_files(), request.clone()).unwrap();
    let second = resolve(&repo_map(), &source_files(), request).unwrap();
    assert_eq!(first.version, second.version);
}

#[test]
fn scope_request_loads_from_toml() {
    let dir = std::env::temp_dir().join(format!(
        "cih-scope-toml-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cih.scope.toml");
    std::fs::write(
        &path,
        r#"
modules = ["app"]
exclude = ["**/generated/**"]
include_decompiled = true
"#,
    )
    .unwrap();

    let request = ScopeRequest::from_toml(&path).unwrap();
    assert_eq!(request.modules, vec!["app"]);
    assert_eq!(request.exclude, vec!["**/generated/**"]);
    assert!(request.include_decompiled);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn parent_module_selects_descendant_files() {
    let repo_map = cih_core::RepoMap {
        root: "/repo".into(),
        build_system: BuildSystem::Maven,
        total_source_loc: 20,
        modules: vec![
            module("payments", "payments"),
            module("api", "payments/api"),
            module("core", "payments/core"),
        ],
        jars: Vec::new(),
        decompiled_dirs: Vec::new(),
        architecture_hint: cih_core::ArchitectureHint::Unknown,
        total_source_files: 2,
        per_language: std::collections::BTreeMap::from([("java".into(), 2)]),
    };
    let files = vec![
        owned("payments/api/src/main/java/Api.java", Some("payments/api")),
        owned(
            "payments/core/src/main/java/Core.java",
            Some("payments/core"),
        ),
    ];

    let scope = resolve(
        &repo_map,
        &files,
        ScopeRequest {
            modules: vec!["payments".into()],
            ..ScopeRequest::default()
        },
    )
    .unwrap();

    assert_eq!(scope.file_count, 2);
    assert_eq!(
        scope.files,
        vec![
            "payments/api/src/main/java/Api.java",
            "payments/core/src/main/java/Core.java",
        ]
    );
}

#[test]
fn duplicate_module_name_selects_all_and_unknown_bails() {
    let repo_map = cih_core::RepoMap {
        root: "/repo".into(),
        build_system: BuildSystem::Gradle,
        total_source_loc: 20,
        modules: vec![
            module("billing", "services/billing"),
            module("billing", "legacy/billing"),
        ],
        jars: Vec::new(),
        decompiled_dirs: Vec::new(),
        architecture_hint: cih_core::ArchitectureHint::Unknown,
        total_source_files: 2,
        per_language: std::collections::BTreeMap::from([("java".into(), 2)]),
    };
    let files = vec![
        owned("services/billing/src/A.java", Some("services/billing")),
        owned("legacy/billing/src/B.java", Some("legacy/billing")),
    ];

    let scope = resolve(
        &repo_map,
        &files,
        ScopeRequest {
            modules: vec!["billing".into()],
            ..ScopeRequest::default()
        },
    )
    .unwrap();
    assert_eq!(scope.file_count, 2);

    let unknown = resolve(
        &repo_map,
        &files,
        ScopeRequest {
            modules: vec!["nope".into()],
            ..ScopeRequest::default()
        },
    );
    assert!(unknown.is_err());
}
