package com.acme.user;

import org.springframework.beans.factory.annotation.Qualifier;
import org.springframework.web.bind.annotation.PostMapping;
import org.springframework.web.bind.annotation.RequestBody;
import org.springframework.web.bind.annotation.RequestMapping;
import org.springframework.web.bind.annotation.RestController;

@RestController
@RequestMapping("/api/passwords")
public class PasswordController {

    private final UserAdmin userAdmin;

    public PasswordController(@Qualifier("retailUserAdminRef") UserAdmin userAdmin) {
        this.userAdmin = userAdmin;
    }

    @PostMapping("/change")
    public void change(@RequestBody PasswordRequest request) {
        this.userAdmin.modifyUserPassword(request);
    }
}
