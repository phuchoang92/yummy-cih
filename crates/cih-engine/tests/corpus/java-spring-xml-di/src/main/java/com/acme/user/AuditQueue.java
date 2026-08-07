package com.acme.user;

/** Async audit pipeline: statements are queued and written by a worker thread. */
public class AuditQueue {

    public void enqueue(String statement, Object... args) {
        // hands off to the worker; no direct JDBC here
    }
}
