package com.acme.user;

public class PasswordRequest {

    private static final String DEFAULT = "fallback";
    private String newPassword;

    public String getNewPassword() {
        return newPassword;
    }

    public String getDefault() {
        return DEFAULT;
    }
}
