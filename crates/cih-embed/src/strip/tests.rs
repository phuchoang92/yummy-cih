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
