package com.acme.user;

/** Base implementation, declared in XML as bean id {@code retailUserAdminRef}. */
public class UserImpl implements UserAdmin {

    private static final String INSERT_AUDIT_LOG =
        "INSERT INTO AUDIT_LOG (ID, ACTION, ACTOR) VALUES (?, ?, ?)";

    private AuditQueue auditQueue;

    @Override
    public void modifyUserPassword(PasswordRequest request) {
        persist(request);
        auditQueue.enqueue(INSERT_AUDIT_LOG, "PASSWORD_CHANGE");
    }

    private void persist(PasswordRequest request) {
        // write path stub
    }
}
