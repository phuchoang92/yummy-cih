package com.acme.user;

/** Base implementation, declared in XML as bean id {@code retailUserAdminRef}. */
public class UserImpl implements UserAdmin {

    private static final String INSERT_AUDIT_LOG =
        "INSERT INTO AUDIT_LOG (ID, ACTION, ACTOR) VALUES (?, ?, ?)";
    private static final String unicodeLabel = "é€";

    private AuditQueue auditQueue;

    @Override
    public void modifyUserPassword(PasswordRequest request) {
        persist(request);
        auditQueue.enqueue(INSERT_AUDIT_LOG, "PASSWORD_CHANGE");
    }

    /** Models an inherited/custom wrapper whose invocation has no explicit receiver. */
    private void enqueueAudit() {
        enqueue(INSERT_AUDIT_LOG, "PASSWORD_CHANGE");
    }

    /** Non-SQL Unicode text must be ignored without panicking in SQL detection. */
    private void ignoreUnicodeLabel() {
        enqueue(unicodeLabel);
    }

    private void persist(PasswordRequest request) {
        // write path stub
    }
}
