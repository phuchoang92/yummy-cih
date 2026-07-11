//! Pure string helpers shared by the per-language framework detectors
//! (Spring/Feign/Kafka contract extraction). Hoisted from the Java parser so
//! Kotlin (and later languages) reuse the exact same normalization — tree
//! walking stays per-language (grammars differ), only string logic is shared.

/// RestTemplate method name → HTTP verb.
pub(crate) fn rest_template_http_method(method: &str) -> Option<&'static str> {
    match method {
        "getForObject" | "getForEntity" => Some("GET"),
        "postForObject" | "postForEntity" | "postForLocation" => Some("POST"),
        "put" => Some("PUT"),
        "delete" => Some("DELETE"),
        "patchForObject" => Some("PATCH"),
        "exchange" => None,
        _ => None,
    }
}

/// Infer the verb of a WebClient `.uri(...)` call from its receiver chain
/// (`client.get().uri(...)` → GET).
pub(crate) fn infer_webclient_http_method(receiver: &str) -> Option<&'static str> {
    for (needle, method) in [
        (".get()", "GET"),
        (".post()", "POST"),
        (".put()", "PUT"),
        (".delete()", "DELETE"),
        (".patch()", "PATCH"),
    ] {
        if receiver.contains(needle) {
            return Some(method);
        }
    }
    None
}

/// Spring route annotation simple name → HTTP verb.
pub(crate) fn spring_http_method(annotation: &str) -> Option<&'static str> {
    match annotation {
        "GetMapping" => Some("GET"),
        "PostMapping" => Some("POST"),
        "PutMapping" => Some("PUT"),
        "DeleteMapping" => Some("DELETE"),
        "PatchMapping" => Some("PATCH"),
        _ => None,
    }
}

/// Strip a raw type down to its simple base name: drops generics, arrays,
/// and package qualifiers (`java.util.List<Foo>[]` → `List`).
pub(crate) fn base_type_simple(raw: &str) -> String {
    raw.split('<')
        .next()
        .unwrap_or(raw)
        .replace("[]", "")
        .rsplit('.')
        .next()
        .unwrap_or(raw)
        .trim()
        .to_string()
}

/// Normalize an outbound-call URL to its path part: strips scheme + host,
/// collapses duplicate slashes. Non-path fragments pass through unchanged.
/// `pub` (re-exported as `cih_lang::normalize_external_url`) so the resolve
/// phase folds dynamic URLs with the exact same normalization the parsers use.
pub fn normalize_external_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
    {
        return rest
            .find('/')
            .map(|idx| collapse_slashes(&rest[idx..]))
            .unwrap_or_else(|| "/".to_string());
    }
    if trimmed.starts_with('/') {
        collapse_slashes(trimmed)
    } else {
        trimmed.to_string()
    }
}

/// Join a class-level route prefix with a method-level path, collapsing
/// duplicate slashes and guaranteeing a leading `/`.
pub(crate) fn normalize_route_path(route_path: &str, prefix: &str) -> String {
    let path_part = route_path.trim().trim_matches('/');
    let prefix_part = prefix.trim().trim_matches('/');
    let joined = if prefix_part.is_empty() {
        format!("/{path_part}")
    } else if path_part.is_empty() {
        format!("/{prefix_part}")
    } else {
        format!("/{prefix_part}/{path_part}")
    };
    collapse_slashes(&joined)
}

fn collapse_slashes(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut previous_slash = false;
    for ch in path.chars() {
        if ch == '/' {
            if !previous_slash {
                out.push(ch);
            }
            previous_slash = true;
        } else {
            out.push(ch);
            previous_slash = false;
        }
    }
    if out.is_empty() {
        "/".into()
    } else {
        out
    }
}
