package com.acme.user;

import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.beans.factory.annotation.Qualifier;

/**
 * Decorator that pre-validates and then delegates to the XML-wired base impl.
 * The OCB false-recursion shape: without the qualifier, the sole in-scope
 * implementor heuristic would redirect this delegation back onto this class.
 */
public class CustomUserImpl implements UserAdmin {

    private UserAdmin retailUserAdminRef;

    @Autowired
    public CustomUserImpl(@Qualifier("retailUserAdminRef") UserAdmin delegate) {
        this.retailUserAdminRef = delegate;
    }

    @Override
    public void modifyUserPassword(PasswordRequest request) {
        validate(request);
        this.retailUserAdminRef.modifyUserPassword(request);
    }

    private void validate(PasswordRequest request) {
        if (request.getNewPassword() == null) {
            throw new IllegalArgumentException("password required");
        }
    }
}
