//! Per-`.java` extraction: LOC (newline count, no parse), package declaration,
//! and a cheap Spring signal (substring counts - adapts GitNexus's
//! `detectFrameworkFromAST` to a parse-free scan). Parallel via rayon.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use cih_core::SpringSignal;
use rayon::prelude::*;

use super::{JavaFileInfo, ScannedFile};

pub(super) fn collect_java_files(root: &Path, files: &[ScannedFile]) -> Vec<JavaFileInfo> {
    let java_count = files.iter().filter(|f| f.path.ends_with(".java")).count();
    tracing::debug!(java_files = java_count, "java_scan: starting per-file LOC/package/Spring extraction");

    let result: Vec<JavaFileInfo> = files
        .par_iter()
        .filter(|file| file.path.ends_with(".java"))
        .filter_map(|file| {
            let full_path = root.join(&file.path);
            let content = fs::read_to_string(&full_path).ok()?;
            let spring = detect_spring_signal(&content);
            tracing::debug!(
                file = %file.path,
                loc = content.bytes().filter(|b| *b == b'\n').count(),
                spring_controller = spring.controllers,
                spring_service = spring.services,
                "java_scan: parsed file"
            );
            Some(JavaFileInfo {
                path: file.path.clone(),
                loc: content.bytes().filter(|b| *b == b'\n').count() as u64,
                package: extract_package(&content),
                spring,
            })
        })
        .collect();

    let spring_files = result
        .iter()
        .filter(|f| {
            f.spring.controllers + f.spring.services + f.spring.repositories
                + f.spring.components + f.spring.configs + f.spring.entities > 0
        })
        .count();
    tracing::debug!(
        parsed = result.len(),
        spring_annotated = spring_files,
        "java_scan: extraction complete"
    );
    result
}

pub(super) fn collect_decompiled_dirs(files: &[ScannedFile]) -> Vec<String> {
    let mut dirs = BTreeSet::new();
    for file in files {
        if file.path == ".workspace-dependencies"
            || file.path.starts_with(".workspace-dependencies/")
        {
            dirs.insert(".workspace-dependencies".to_string());
        }
    }
    dirs.into_iter().collect()
}

pub(super) fn add_spring_signal(target: &mut SpringSignal, signal: &SpringSignal) {
    target.controllers += signal.controllers;
    target.services += signal.services;
    target.repositories += signal.repositories;
    target.components += signal.components;
    target.configs += signal.configs;
    target.entities += signal.entities;
    target.mappings += signal.mappings;
}

fn extract_package(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("package ") {
            return rest
                .split(';')
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
        }
    }
    None
}

fn detect_spring_signal(content: &str) -> SpringSignal {
    SpringSignal {
        controllers: contains_any(content, &["@RestController", "@Controller"]) as u32,
        services: content.contains("@Service") as u32,
        repositories: content.contains("@Repository") as u32,
        components: content.contains("@Component") as u32,
        configs: content.contains("@Configuration") as u32,
        entities: content.contains("@Entity") as u32,
        mappings: contains_any(
            content,
            &[
                "@RequestMapping",
                "@GetMapping",
                "@PostMapping",
                "@PutMapping",
                "@PatchMapping",
                "@DeleteMapping",
            ],
        ) as u32,
    }
}

fn contains_any(content: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| content.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_and_spring_detection_are_file_level() {
        let java = r#"
            package com.acme.owner;
            import org.springframework.web.bind.annotation.GetMapping;
            @RestController
            class OwnerController {
              @GetMapping("/owners")
              String owners() { return ""; }
            }
        "#;
        let spring = detect_spring_signal(java);
        assert_eq!(extract_package(java).as_deref(), Some("com.acme.owner"));
        assert_eq!(spring.controllers, 1);
        assert_eq!(spring.mappings, 1);
        assert_eq!(spring.services, 0);
    }
}
