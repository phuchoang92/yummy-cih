use cih_core::BuildSystem;
use cih_engine::scan::*;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST_REPO_ID: AtomicU64 = AtomicU64::new(0);

struct TestRepo {
    path: std::path::PathBuf,
}

impl TestRepo {
    fn new() -> Self {
        loop {
            let unique = NEXT_TEST_REPO_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("cih-engine-test-{}-{unique}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => panic!("failed to create test repo {}: {err}", path.display()),
            }
        }
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.path.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn scan_repo_builds_modules_counts_and_sibling_deps() {
    let repo = TestRepo::new();
    repo.write(
        "pom.xml",
        r#"
          <project>
            <groupId>com.acme</groupId>
            <artifactId>root</artifactId>
            <modules><module>app</module><module>infra</module></modules>
          </project>
        "#,
    );
    repo.write(
        "app/pom.xml",
        r#"
          <project>
            <groupId>com.acme</groupId>
            <artifactId>app</artifactId>
            <dependencies>
              <dependency><groupId>com.acme</groupId><artifactId>infra</artifactId></dependency>
            </dependencies>
          </project>
        "#,
    );
    repo.write(
        "app/src/main/java/com/acme/owner/OwnerController.java",
        "package com.acme.owner;\n@RestController\n@GetMapping(\"/owners\")\nclass OwnerController {}\n",
    );
    repo.write(
        "infra/build.gradle",
        "group = 'com.acme'\ndependencies { implementation('org.springframework:spring-core:6.0.0') }\n",
    );
    repo.write(
        "infra/src/main/java/com/acme/owner/OwnerRepository.java",
        "package com.acme.owner;\n@Repository\nclass OwnerRepository {}\n",
    );
    repo.write(
        "target/generated/Generated.java",
        "package ignored;\n@Service\nclass Generated {}\n",
    );
    repo.write(
        ".workspace-dependencies/lib/src/main/java/com/acme/lib/LibService.java",
        "package com.acme.lib;\n@Service\nclass LibService {}\n",
    );

    let scan = scan_repo(&repo.path).unwrap();
    let repo_map = &scan.repo_map;
    assert_eq!(repo_map.build_system, BuildSystem::Maven);
    assert_eq!(repo_map.total_source_files, 3);
    assert_eq!(repo_map.per_language.get("java"), Some(&3));
    assert_eq!(repo_map.decompiled_dirs, vec![".workspace-dependencies"]);

    let app = repo_map.modules.iter().find(|m| m.name == "app").unwrap();
    assert_eq!(app.source_files, 1);
    assert!(app.frameworks.contains(&"spring".to_string()));
    assert_eq!(app.packages, vec!["com.acme.owner"]);
    assert_eq!(app.depends_on, vec!["infra"]);

    let infra = repo_map.modules.iter().find(|m| m.name == "infra").unwrap();
    assert_eq!(infra.source_files, 1);
    assert!(infra.frameworks.contains(&"spring".to_string()));

    let decompiled = repo_map
        .modules
        .iter()
        .find(|m| m.name == ".workspace-dependencies")
        .unwrap();
    assert_eq!(decompiled.source_files, 1);
    assert!(decompiled.frameworks.contains(&"spring".to_string()));

    assert_eq!(scan.source_files.len(), 3);
    assert_eq!(
        scan.source_files
            .iter()
            .find(|file| file.rel.ends_with("OwnerController.java"))
            .and_then(|file| file.module_rel.as_deref()),
        Some("app")
    );
    assert_eq!(
        scan.source_files
            .iter()
            .find(|file| file.rel.ends_with("OwnerRepository.java"))
            .and_then(|file| file.module_rel.as_deref()),
        Some("infra")
    );
    assert_eq!(
        scan.source_files
            .iter()
            .find(|file| file.rel.ends_with("LibService.java"))
            .and_then(|file| file.module_rel.as_deref()),
        Some(".workspace-dependencies")
    );
}

#[test]
fn scan_mixed_repo_java_ts_python() {
    let repo = TestRepo::new();
    // Java
    repo.write(
        "pom.xml",
        r#"
          <project>
            <groupId>com.acme</groupId>
            <artifactId>java-app</artifactId>
          </project>
        "#,
    );
    repo.write(
        "src/main/java/com/acme/App.java",
        "package com.acme;\n@RestController\nclass App {}\n",
    );

    // Node (NestJS TS)
    repo.write(
        "frontend/package.json",
        r#"
          {
            "name": "frontend-web",
            "dependencies": {
              "@nestjs/core": "^10.0.0"
            }
          }
        "#,
    );
    repo.write("frontend/src/index.ts", "@Injectable()\nclass Logger {}\n");

    // Python (FastAPI)
    repo.write(
        "backend/pyproject.toml",
        r#"
          [project]
          name = "python-backend"
          dependencies = [
              "fastapi>=0.100.0"
          ]
        "#,
    );
    repo.write(
        "backend/app.py",
        "from fastapi import FastAPI\napp = FastAPI()\n",
    );

    let scan = scan_repo(&repo.path).unwrap();
    let repo_map = &scan.repo_map;

    // Build system is detected as Mixed
    assert_eq!(repo_map.build_system, BuildSystem::Mixed);
    assert_eq!(repo_map.total_source_files, 3);
    assert_eq!(repo_map.per_language.get("java"), Some(&1));
    assert_eq!(repo_map.per_language.get("typescript"), Some(&1));
    assert_eq!(repo_map.per_language.get("python"), Some(&1));

    // Java Module (root module)
    let java_mod = repo_map
        .modules
        .iter()
        .find(|m| m.name == "java-app")
        .unwrap();
    assert_eq!(java_mod.source_files, 1);
    assert!(java_mod.frameworks.contains(&"spring".to_string()));

    // Node Module
    let node_mod = repo_map
        .modules
        .iter()
        .find(|m| m.name == "frontend-web")
        .unwrap();
    assert_eq!(node_mod.source_files, 1);
    assert!(node_mod.frameworks.contains(&"nestjs".to_string()));

    // Python Module
    let py_mod = repo_map
        .modules
        .iter()
        .find(|m| m.name == "python-backend")
        .unwrap();
    assert_eq!(py_mod.source_files, 1);
    assert!(py_mod.frameworks.contains(&"fastapi".to_string()));
}

#[test]
fn scan_node_repo_package_json() {
    let repo = TestRepo::new();
    repo.write(
        "package.json",
        r#"
          {
            "name": "root-project",
            "dependencies": {
              "api-server": "file:./api-server"
            }
          }
        "#,
    );
    repo.write(
        "api-server/package.json",
        r#"
          {
            "name": "api-server",
            "dependencies": {}
          }
        "#,
    );
    repo.write("index.ts", "console.log('root');");
    repo.write("api-server/index.ts", "console.log('api');");

    let scan = scan_repo(&repo.path).unwrap();
    let repo_map = &scan.repo_map;

    assert_eq!(repo_map.build_system, BuildSystem::Node);
    assert_eq!(repo_map.total_source_files, 2);

    let root_mod = repo_map
        .modules
        .iter()
        .find(|m| m.name == "root-project")
        .unwrap();
    assert_eq!(root_mod.source_files, 1);
    assert!(root_mod.depends_on.contains(&"api-server".to_string()));

    let api_mod = repo_map
        .modules
        .iter()
        .find(|m| m.name == "api-server")
        .unwrap();
    assert_eq!(api_mod.source_files, 1);
}

#[test]
fn scan_node_repo_peer_and_optional_dependencies_link_local_modules() {
    let repo = TestRepo::new();
    repo.write(
        "package.json",
        r#"
          {
            "name": "shell-app",
            "peerDependencies": {
              "@acme/design-system": "^1.0.0"
            },
            "optionalDependencies": {
              "worker-service": "file:./worker-service"
            }
          }
        "#,
    );
    repo.write(
        "design-system/package.json",
        r#"
          {
            "name": "@acme/design-system"
          }
        "#,
    );
    repo.write(
        "worker-service/package.json",
        r#"
          {
            "name": "worker-service"
          }
        "#,
    );
    repo.write("index.ts", "console.log('root');");
    repo.write("design-system/index.ts", "export const button = 1;");
    repo.write("worker-service/index.ts", "console.log('worker');");

    let scan = scan_repo(&repo.path).unwrap();
    let shell = scan
        .repo_map
        .modules
        .iter()
        .find(|m| m.name == "shell-app")
        .unwrap();

    assert_eq!(
        shell.depends_on,
        vec![
            "@acme/design-system".to_string(),
            "worker-service".to_string()
        ]
    );
}

#[test]
fn scan_python_repo_pyproject_toml() {
    let repo = TestRepo::new();
    repo.write(
        "pyproject.toml",
        r#"
          [project]
          name = "my-fastapi-app"
        "#,
    );
    repo.write("main.py", "import flask\n");

    let scan = scan_repo(&repo.path).unwrap();
    let repo_map = &scan.repo_map;

    assert_eq!(repo_map.build_system, BuildSystem::Python);
    assert_eq!(repo_map.total_source_files, 1);

    let main_mod = repo_map
        .modules
        .iter()
        .find(|m| m.name == "my-fastapi-app")
        .unwrap();
    assert_eq!(main_mod.source_files, 1);
    assert!(main_mod.frameworks.contains(&"flask".to_string()));
}

#[test]
fn scan_python_repo_normalizes_names_for_local_dependency_matching() {
    let repo = TestRepo::new();
    repo.write(
        "pyproject.toml",
        r#"
          [project]
          name = "My_Service"
          dependencies = [
            "Shared.Utils>=1.0"
          ]
        "#,
    );
    repo.write("app.py", "import fastapi\n");
    repo.write(
        "libs/shared_utils/pyproject.toml",
        r#"
          [project]
          name = "shared-utils"
        "#,
    );
    repo.write("libs/shared_utils/util.py", "value = 1\n");

    let scan = scan_repo(&repo.path).unwrap();
    let repo_map = &scan.repo_map;

    let service = repo_map
        .modules
        .iter()
        .find(|m| m.name == "my-service")
        .unwrap();
    assert_eq!(service.depends_on, vec!["shared-utils".to_string()]);

    let shared = repo_map
        .modules
        .iter()
        .find(|m| m.name == "shared-utils")
        .unwrap();
    assert_eq!(shared.source_files, 1);
}

#[test]
fn scan_python_repo_requirements_txt() {
    let repo = TestRepo::new();
    repo.write("requirements.txt", "flask\n");
    repo.write("app.py", "import flask\n");

    let scan = scan_repo(&repo.path).unwrap();
    let repo_map = &scan.repo_map;

    assert_eq!(repo_map.build_system, BuildSystem::Python);
    assert_eq!(repo_map.total_source_files, 1);

    // Should fall back to the folder name or root directory name for the module name
    let main_mod = &repo_map.modules[0];
    assert_eq!(main_mod.source_files, 1);
    assert!(main_mod.frameworks.contains(&"flask".to_string()));
}
