package com.acme.user;

import org.springframework.context.event.EventListener;
import org.springframework.scheduling.annotation.Async;
import org.springframework.stereotype.Component;

@Component
public class PasswordChangedListener {

    @Async
    @EventListener
    public void onPasswordChanged(PasswordChangedEvent event) {
        // async audit projection
    }
}

class PasswordChangedEvent {}
