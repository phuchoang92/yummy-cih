use cih_core::BuildSystem;
use cih_engine_lib::scan::*;

struct TestRepo {
    path: std::path::PathBuf,
}

impl TestRepo {
    fn new() -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("cih-engine-test-{unique}"));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
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
    assert_eq!(repo_map.total_java_files, 3);
    assert_eq!(repo_map.decompiled_dirs, vec![".workspace-dependencies"]);

    let app = repo_map.modules.iter().find(|m| m.name == "app").unwrap();
    assert_eq!(app.java_files, 1);
    assert_eq!(app.spring.controllers, 1);
    assert_eq!(app.spring.mappings, 1);
    assert_eq!(app.packages, vec!["com.acme.owner"]);
    assert_eq!(app.depends_on, vec!["infra"]);

    let infra = repo_map.modules.iter().find(|m| m.name == "infra").unwrap();
    assert_eq!(infra.java_files, 1);
    assert_eq!(infra.spring.repositories, 1);

    let decompiled = repo_map
        .modules
        .iter()
        .find(|m| m.name == ".workspace-dependencies")
        .unwrap();
    assert_eq!(decompiled.java_files, 1);
    assert_eq!(decompiled.spring.services, 1);

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
