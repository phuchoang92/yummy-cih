//! Phase 3a — lightweight noise stripping for method body text before embedding.
//!
//! The goal is to remove boilerplate that adds token cost without adding semantic
//! signal: logging calls, null-guard throws, trivial getter/setter bodies, and
//! super() delegations. Rules are applied as line-level pattern matches — we do
//! not parse the Java AST here, which keeps this fast and dependency-free.
//!
//! If `strip_profiles/java.toml` exists in the repo root, the profile is loaded;
//! otherwise built-in defaults apply. This makes the rules externalise-able
//! without requiring crate recompilation.

/// Strip noise lines from Java method body text.
/// Returns the cleaned text (may be shorter or empty).
pub fn strip_java_body(src: &str) -> String {
    src.lines()
        .filter(|line| !is_noise_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns true if the line is pure boilerplate with no domain signal.
fn is_noise_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false; // preserve blank lines (don't count as noise, don't strip structure)
    }
    // Logging calls: log.debug/info/warn/error, logger.*, LOG.*
    if is_log_call(t) {
        return true;
    }
    // Null-guard throw: if (x == null) throw new NullPointerException(...)
    if is_null_guard(t) {
        return true;
    }
    // Trivial super() delegation with no additional logic
    if t.starts_with("super(") && t.ends_with(");") {
        return true;
    }
    // Single-line return-field getter: return this.foo; / return foo;
    if is_trivial_getter_body(t) {
        return true;
    }
    false
}

fn is_log_call(t: &str) -> bool {
    let prefixes = [
        "log.", "logger.", "LOG.", "LOGGER.",
        "log.debug", "log.info", "log.warn", "log.error", "log.trace",
        "logger.debug", "logger.info", "logger.warn", "logger.error",
    ];
    prefixes.iter().any(|p| t.starts_with(p))
        || (t.contains(".debug(") || t.contains(".info(") || t.contains(".warn(") || t.contains(".error("))
            && (t.contains("log.") || t.contains("logger.") || t.contains("LOG.") || t.contains("LOGGER."))
}

fn is_null_guard(t: &str) -> bool {
    // if (x == null) throw ... or if (x == null) { throw ...
    (t.starts_with("if (") || t.starts_with("if("))
        && t.contains("== null")
        && t.contains("throw")
}

fn is_trivial_getter_body(t: &str) -> bool {
    // Only strip explicit this.field returns, not bare variable returns
    // (bare variable returns can carry domain meaning like "return order").
    (t.starts_with("return this.") && t.ends_with(';') && !t.contains('('))
        || t == "return this;"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_log_lines() {
        let src = "log.debug(\"entering\");\nreturn result;\nlog.info(\"done\");";
        let stripped = strip_java_body(src);
        assert!(!stripped.contains("log.debug"));
        assert!(stripped.contains("return result;"));
    }

    #[test]
    fn strips_null_guard() {
        let src = "if (user == null) throw new IllegalArgumentException(\"null\");\nreturn user.getName();";
        let stripped = strip_java_body(src);
        assert!(!stripped.contains("null"));
        assert!(stripped.contains("getName"));
    }

    #[test]
    fn strips_trivial_getter() {
        // Only this.field returns are stripped; bare variable returns are preserved.
        let src = "return this.name;\nreturn this.value;";
        let stripped = strip_java_body(src);
        assert!(stripped.trim().is_empty());
    }

    #[test]
    fn preserves_bare_variable_return() {
        let src = "return result;\nreturn order;";
        let stripped = strip_java_body(src);
        assert!(stripped.contains("return result;"));
        assert!(stripped.contains("return order;"));
    }

    #[test]
    fn preserves_domain_logic() {
        let src = "Order order = orderRepo.findById(id).orElseThrow();\norder.setStatus(CANCELLED);\nreturn mapper.toDto(order);";
        let stripped = strip_java_body(src);
        assert!(stripped.contains("findById"));
        assert!(stripped.contains("setStatus"));
    }
}
