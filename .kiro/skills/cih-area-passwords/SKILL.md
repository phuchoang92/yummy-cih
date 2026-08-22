---
name: cih-area-passwords
description: "Work safely in the CIH-detected Passwords functional area."
---

# Passwords

This skill is generated from the latest CIH community artifacts. Use `query` with `Passwords` to find current execution flows, then `context` on the exact symbol. Run upstream `impact` before editing and `detect_changes` before committing.

## Representative symbols

- `Method:com.acme.user.AuditQueue#enqueue/2` — `crates/cih-engine/tests/corpus/java-spring-xml-di/src/main/java/com/acme/user/AuditQueue.java:6`
- `Method:com.acme.user.CustomUserImpl#modifyUserPassword/1` — `crates/cih-engine/tests/corpus/java-spring-xml-di/src/main/java/com/acme/user/CustomUserImpl.java:20`
- `Method:com.acme.user.CustomUserImpl#validate/1` — `crates/cih-engine/tests/corpus/java-spring-xml-di/src/main/java/com/acme/user/CustomUserImpl.java:26`
- `Method:com.acme.user.PasswordController#change/1` — `crates/cih-engine/tests/corpus/java-spring-xml-di/src/main/java/com/acme/user/PasswordController.java:19`
- `Method:com.acme.user.PasswordRequest#getNewPassword/0` — `crates/cih-engine/tests/corpus/java-spring-xml-di/src/main/java/com/acme/user/PasswordRequest.java:8`
- `Method:com.acme.user.UserImpl#modifyUserPassword/1` — `crates/cih-engine/tests/corpus/java-spring-xml-di/src/main/java/com/acme/user/UserImpl.java:12`
- `Method:com.acme.user.UserImpl#persist/1` — `crates/cih-engine/tests/corpus/java-spring-xml-di/src/main/java/com/acme/user/UserImpl.java:28`
