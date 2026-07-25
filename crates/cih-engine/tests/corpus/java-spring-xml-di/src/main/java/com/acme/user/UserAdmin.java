package com.acme.user;

/** Service interface wired only through Spring XML — no stereotype annotations. */
public interface UserAdmin {

    void modifyUserPassword(PasswordRequest request);
}
