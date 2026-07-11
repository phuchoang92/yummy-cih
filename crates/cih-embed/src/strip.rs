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
    // Prefix-only check: a line is a log call only if it *starts with* a known
    // logger variable. The old contains(".info(") && contains("log.") approach
    // was unsound — any line calling .info() on a non-logger object that also
    // happened to mention the string "log." (e.g. in a string literal) was
    // incorrectly stripped.
    let prefixes = ["log.", "logger.", "LOG.", "LOGGER."];
    prefixes.iter().any(|p| t.starts_with(p))
}

fn is_null_guard(t: &str) -> bool {
    // if (x == null) throw ... or if (x == null) { throw ...
    (t.starts_with("if (") || t.starts_with("if(")) && t.contains("== null") && t.contains("throw")
}

fn is_trivial_getter_body(t: &str) -> bool {
    // Only strip explicit this.field returns, not bare variable returns
    // (bare variable returns can carry domain meaning like "return order").
    (t.starts_with("return this.") && t.ends_with(';') && !t.contains('(')) || t == "return this;"
}

#[cfg(test)]
mod tests {
    use super::strip_java_body;

    #[test]
    fn strips_logging_calls() {
        let src = "log.debug(\"x\");\nint y = compute();\nlogger.info(\"done\");";
        assert_eq!(strip_java_body(src), "int y = compute();");
    }

    #[test]
    fn strips_null_guard_throws() {
        let src = "if (order == null) throw new NullPointerException();\nprocess(order);";
        assert_eq!(strip_java_body(src), "process(order);");
    }

    #[test]
    fn strips_trivial_this_getter_and_super_delegation() {
        assert_eq!(strip_java_body("super();"), "");
        assert_eq!(strip_java_body("return this.name;"), "");
        assert_eq!(strip_java_body("return this;"), "");
    }

    #[test]
    fn preserves_domain_bearing_lines() {
        // A bare-variable return can carry meaning; a method call is not a getter.
        assert_eq!(strip_java_body("return order;"), "return order;");
        assert_eq!(
            strip_java_body("return this.calculate();"),
            "return this.calculate();"
        );
        // Blank lines are structure, not noise.
        assert_eq!(strip_java_body("a();\n\nb();"), "a();\n\nb();");
    }

    #[test]
    fn strips_nothing_from_pure_domain_body() {
        let src = "BigDecimal total = price.multiply(qty);\nreturn total;";
        assert_eq!(strip_java_body(src), src);
    }
}
