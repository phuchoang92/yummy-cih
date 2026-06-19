use std::collections::BTreeSet;
use std::path::Path;

/// Registry of annotation/decorator patterns that identify entry-point methods across languages.
///
/// Built-in defaults cover Java (Spring MVC, JAX-RS, Kafka), TypeScript (NestJS), and Python
/// (Flask, FastAPI, Celery). Per-project overrides can be placed in `.cih/entry_points/*.toml`
/// using the schema:
///
/// ```toml
/// [http]
/// annotations = ["MyRoute", ...]
///
/// [event]
/// annotations = ["MyListener", ...]
///
/// [scheduled]
/// annotations = ["MyCron", ...]
/// ```
#[derive(Debug, Default, Clone)]
pub struct EntrypointRegistry {
    pub(crate) http: BTreeSet<String>,
    pub(crate) event: BTreeSet<String>,
    pub(crate) scheduled: BTreeSet<String>,
}

impl EntrypointRegistry {
    /// Build defaults and merge any `{repo}/.cih/entry_points/*.toml` overrides.
    pub fn load(repo: &Path) -> Self {
        let mut reg = Self::builtin_defaults();
        let override_dir = repo.join(".cih").join("entry_points");
        if override_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&override_dir) {
                let mut paths: Vec<_> = entries
                    .flatten()
                    .filter(|e| {
                        e.path().extension().and_then(|s| s.to_str()) == Some("toml")
                    })
                    .map(|e| e.path())
                    .collect();
                paths.sort();
                for path in paths {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        tracing::debug!(path = %path.display(), "loading entry_points override");
                        reg.merge_toml(&content);
                    }
                }
            }
        }
        reg
    }

    fn builtin_defaults() -> Self {
        let mut reg = Self::default();

        // Java — Spring MVC + JAX-RS
        for ann in [
            "GetMapping", "PostMapping", "PutMapping", "DeleteMapping", "PatchMapping",
            "RequestMapping",
            "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS",
        ] {
            reg.http.insert(ann.to_string());
        }
        // TypeScript — NestJS
        for ann in ["Get", "Post", "Put", "Delete", "Patch", "Head", "Options", "All"] {
            reg.http.insert(ann.to_string());
        }
        // Python — Flask + FastAPI
        for ann in [
            "app.route", "app.get", "app.post", "app.put", "app.delete", "app.patch",
            "router.get", "router.post", "router.put", "router.delete", "router.patch",
            "blueprint.route",
        ] {
            reg.http.insert(ann.to_string());
        }

        // Java — message listeners
        for ann in [
            "KafkaListener", "EventListener", "RabbitListener",
            "JmsListener", "SqsListener", "StreamListener",
        ] {
            reg.event.insert(ann.to_string());
        }
        // TypeScript — NestJS messaging
        for ann in ["MessagePattern", "EventPattern"] {
            reg.event.insert(ann.to_string());
        }
        // Python — Celery tasks
        for ann in ["task", "app.task", "shared_task", "celery.task"] {
            reg.event.insert(ann.to_string());
        }

        // Java
        for ann in ["Scheduled", "Cron"] {
            reg.scheduled.insert(ann.to_string());
        }
        // TypeScript — NestJS scheduling
        for ann in ["Cron", "Interval", "Timeout"] {
            reg.scheduled.insert(ann.to_string());
        }

        reg
    }

    fn merge_toml(&mut self, content: &str) {
        let Ok(table) = content.parse::<toml::Table>() else {
            return;
        };
        for (section, target) in [
            ("http", &mut self.http),
            ("event", &mut self.event),
            ("scheduled", &mut self.scheduled),
        ] {
            if let Some(anns) = table
                .get(section)
                .and_then(|v| v.as_table())
                .and_then(|t| t.get("annotations"))
                .and_then(|v| v.as_array())
            {
                for ann in anns.iter().filter_map(|v| v.as_str()) {
                    target.insert(ann.to_string());
                }
            }
        }
    }

    pub fn http_annotations(&self) -> &BTreeSet<String> {
        &self.http
    }

    pub fn event_annotations(&self) -> &BTreeSet<String> {
        &self.event
    }

    pub fn scheduled_annotations(&self) -> &BTreeSet<String> {
        &self.scheduled
    }

    pub fn total_patterns(&self) -> usize {
        self.http.len() + self.event.len() + self.scheduled.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_defaults_cover_all_three_languages() {
        let reg = EntrypointRegistry::builtin_defaults();
        // Java Spring
        assert!(reg.http.contains("GetMapping"));
        assert!(reg.http.contains("POST"));
        // TypeScript NestJS
        assert!(reg.http.contains("Get"));
        assert!(reg.event.contains("MessagePattern"));
        // Python Flask/FastAPI
        assert!(reg.http.contains("app.route"));
        assert!(reg.http.contains("router.get"));
        assert!(reg.event.contains("task"));
        // Scheduled
        assert!(reg.scheduled.contains("Scheduled"));
        assert!(reg.scheduled.contains("Cron"));
    }

    #[test]
    fn merge_toml_adds_custom_patterns() {
        let mut reg = EntrypointRegistry::default();
        let toml = r#"
[http]
annotations = ["MyRoute", "MyGet"]

[event]
annotations = ["MyListener"]

[scheduled]
annotations = ["MyCron"]
"#;
        reg.merge_toml(toml);
        assert!(reg.http.contains("MyRoute"));
        assert!(reg.http.contains("MyGet"));
        assert!(reg.event.contains("MyListener"));
        assert!(reg.scheduled.contains("MyCron"));
    }

    #[test]
    fn malformed_toml_is_ignored() {
        let mut reg = EntrypointRegistry::builtin_defaults();
        let before = reg.total_patterns();
        reg.merge_toml("this is not valid { toml !!!!");
        assert_eq!(reg.total_patterns(), before);
    }
}
