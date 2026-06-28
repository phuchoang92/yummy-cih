//! `pom.xml` (streaming XML stack-parse) + `build.gradle[.kts]` / `settings.gradle`
//! parsing to group/artifact/dependencies/modules. Mirrors GitNexus's
//! `java-workspace-extractor.ts` but uses a real XML reader for Maven.

use std::collections::BTreeSet;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

use super::BuildMeta;

pub fn parse_pom(content: &str) -> Option<BuildMeta> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut stack: Vec<String> = Vec::new();
    let mut project_group: Option<String> = None;
    let mut parent_group: Option<String> = None;
    let mut project_artifact: Option<String> = None;
    let mut parent_artifact: Option<String> = None;
    let mut dep_group: Option<String> = None;
    let mut dep_artifact: Option<String> = None;
    let mut deps = BTreeSet::new();
    let mut modules = BTreeSet::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = String::from_utf8_lossy(event.name().as_ref()).to_string();
                if name == "dependency" {
                    dep_group = None;
                    dep_artifact = None;
                }
                stack.push(name);
            }
            Ok(Event::End(event)) => {
                let name = String::from_utf8_lossy(event.name().as_ref()).to_string();
                if name == "dependency" {
                    if let (Some(group), Some(artifact)) = (&dep_group, &dep_artifact) {
                        deps.insert(format!("{group}:{artifact}"));
                    }
                }
                stack.pop();
            }
            Ok(Event::Text(event)) => {
                let text = String::from_utf8_lossy(event.as_ref()).trim().to_string();
                if text.is_empty() {
                    continue;
                }
                let current = stack.last().map(|s| s.as_str()).unwrap_or_default();
                let in_dependency = stack.iter().any(|s| s == "dependency");
                let in_parent = stack.iter().any(|s| s == "parent");
                let in_modules = stack.iter().any(|s| s == "modules");

                match current {
                    "groupId" if in_dependency => dep_group = Some(text),
                    "artifactId" if in_dependency => dep_artifact = Some(text),
                    "groupId" if in_parent => parent_group = Some(text),
                    "artifactId" if in_parent => parent_artifact = Some(text),
                    "groupId" => project_group = Some(text),
                    "artifactId" => project_artifact = Some(text),
                    "module" if in_modules => {
                        modules.insert(text.trim_matches('/').to_string());
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }

    let group_id = project_group.or(parent_group)?;
    let artifact_id = project_artifact.or(parent_artifact)?;
    Some(BuildMeta {
        group_id,
        artifact_id,
        deps: deps.into_iter().collect(),
        modules: modules.into_iter().collect(),
    })
}

pub fn parse_gradle(content: &str, repo_path: &Path) -> Option<BuildMeta> {
    let group_id = find_gradle_group(content).unwrap_or_default();
    let artifact_id = repo_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("root")
        .to_string();
    let mut deps = BTreeSet::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if is_gradle_dependency_line(trimmed) && !trimmed.contains("project(") {
            if let Some(dep) = first_quoted(trimmed) {
                let parts: Vec<&str> = dep.split(':').collect();
                if parts.len() >= 2 {
                    deps.insert(format!("{}:{}", parts[0], parts[1]));
                }
            }
        }
        if trimmed.contains("project(") {
            if let Some(project) = first_quoted(trimmed) {
                if !group_id.is_empty() {
                    let sub_name = project.trim_start_matches(':').replace(':', "/");
                    let artifact = sub_name.rsplit('/').next().unwrap_or(&sub_name);
                    deps.insert(format!("{group_id}:{artifact}"));
                }
            }
        }
    }

    if group_id.is_empty() && deps.is_empty() {
        return None;
    }

    Some(BuildMeta {
        group_id,
        artifact_id,
        deps: deps.into_iter().collect(),
        modules: Vec::new(),
    })
}

pub fn parse_gradle_includes(content: &str) -> Vec<String> {
    let mut includes = BTreeSet::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("include") {
            continue;
        }
        for quoted in all_quoted(trimmed) {
            let rel = quoted.trim_start_matches(':').replace(':', "/");
            if !rel.is_empty() {
                includes.insert(rel);
            }
        }
    }
    includes.into_iter().collect()
}

fn find_gradle_group(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("group") {
            continue;
        }
        if let Some(value) = first_quoted(trimmed) {
            return Some(value);
        }
    }
    None
}

fn is_gradle_dependency_line(line: &str) -> bool {
    ["implementation", "api", "compileOnly", "runtimeOnly"]
        .iter()
        .any(|config| line.starts_with(config) || line.contains(&format!("{config}(")))
}

fn first_quoted(s: &str) -> Option<String> {
    all_quoted(s).into_iter().next()
}

fn all_quoted(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.char_indices();
    while let Some((start_idx, ch)) = chars.next() {
        if ch != '"' && ch != '\'' {
            continue;
        }
        let quote = ch;
        let value_start = start_idx + ch.len_utf8();
        for (end_idx, end_ch) in chars.by_ref() {
            if end_ch == quote {
                out.push(s[value_start..end_idx].to_string());
                break;
            }
        }
    }
    out
}

fn normalize_python_distribution_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut normalized = String::with_capacity(trimmed.len());
    let mut prev_sep = false;

    for ch in trimmed.chars() {
        if matches!(ch, '-' | '_' | '.') {
            if !normalized.is_empty() && !prev_sep {
                normalized.push('-');
            }
            prev_sep = true;
            continue;
        }

        for lower in ch.to_lowercase() {
            normalized.push(lower);
        }
        prev_sep = false;
    }

    while normalized.ends_with('-') {
        normalized.pop();
    }

    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn parse_python_requirement_name(spec: &str) -> Option<String> {
    let base = spec
        .split(&['<', '>', '=', '!', '~', ';'][..])
        .next()
        .unwrap_or("")
        .trim();
    let base = base.split('[').next().unwrap_or("").trim();
    normalize_python_distribution_name(base)
}

pub fn parse_package_json(content: &str) -> Option<BuildMeta> {
    let val: serde_json::Value = serde_json::from_str(content).ok()?;
    let name = val.get("name")?.as_str()?;
    let mut deps = BTreeSet::new();
    for section in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(deps_obj) = val.get(section).and_then(|d| d.as_object()) {
            for key in deps_obj.keys() {
                deps.insert(key.clone());
            }
        }
    }

    let (group_id, artifact_id) = if name.starts_with('@') && name.contains('/') {
        let mut parts = name.splitn(2, '/');
        (
            parts.next().unwrap_or("").to_string(),
            parts.next().unwrap_or("").to_string(),
        )
    } else {
        ("".to_string(), name.to_string())
    };

    Some(BuildMeta {
        group_id,
        artifact_id,
        deps: deps.into_iter().collect(),
        modules: Vec::new(),
    })
}

pub fn parse_pyproject_toml(content: &str) -> Option<BuildMeta> {
    let val: toml::Value = toml::from_str(content).ok()?;
    let name = val
        .get("project")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .or_else(|| {
            val.get("tool")
                .and_then(|t| t.get("poetry"))
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
        })?;

    let artifact_id = normalize_python_distribution_name(name)?;
    let mut deps = BTreeSet::new();

    if let Some(deps_array) = val
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
    {
        for dep in deps_array {
            if let Some(dep_str) = dep.as_str() {
                if let Some(name) = parse_python_requirement_name(dep_str) {
                    deps.insert(name);
                }
            }
        }
    }

    if let Some(poetry_deps) = val
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        for key in poetry_deps.keys() {
            if key != "python" {
                if let Some(name) = normalize_python_distribution_name(key) {
                    deps.insert(name);
                }
            }
        }
    }

    Some(BuildMeta {
        group_id: "".to_string(),
        artifact_id,
        deps: deps.into_iter().collect(),
        modules: Vec::new(),
    })
}

pub fn parse_setup_cfg(content: &str) -> Option<BuildMeta> {
    let mut name = None;
    let mut deps = BTreeSet::new();
    let mut in_metadata = false;
    let mut in_options = false;
    let mut in_requires = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_metadata = trimmed == "[metadata]";
            in_options = trimmed == "[options]";
            in_requires = false;
            continue;
        }

        if in_metadata && trimmed.starts_with("name") {
            if let Some(val) = trimmed.split('=').nth(1) {
                name = normalize_python_distribution_name(val);
            }
        }

        if in_options && trimmed.starts_with("install_requires") {
            if let Some(val) = trimmed.split('=').nth(1) {
                let val_trimmed = val.trim();
                if !val_trimmed.is_empty() {
                    if let Some(dep) = parse_python_requirement_name(val_trimmed) {
                        deps.insert(dep);
                    }
                }
                in_requires = true;
            }
            continue;
        }

        if in_requires {
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if line.starts_with(' ') || line.starts_with('\t') {
                if let Some(dep) = parse_python_requirement_name(trimmed) {
                    deps.insert(dep);
                }
            } else {
                in_requires = false;
            }
        }
    }

    let artifact_id = name?;
    Some(BuildMeta {
        group_id: "".to_string(),
        artifact_id,
        deps: deps.into_iter().collect(),
        modules: Vec::new(),
    })
}
