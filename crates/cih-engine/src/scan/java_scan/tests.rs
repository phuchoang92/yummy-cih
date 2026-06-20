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
