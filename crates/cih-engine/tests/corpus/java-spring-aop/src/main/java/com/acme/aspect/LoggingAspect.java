package com.acme.aspect;

import org.aspectj.lang.ProceedingJoinPoint;
import org.aspectj.lang.JoinPoint;
import org.aspectj.lang.annotation.Around;
import org.aspectj.lang.annotation.AfterReturning;
import org.aspectj.lang.annotation.Aspect;
import org.aspectj.lang.annotation.Before;
import org.aspectj.lang.annotation.Pointcut;
import org.springframework.stereotype.Component;

@Aspect
@Component
public class LoggingAspect {

    @Pointcut("@annotation(com.acme.annotation.Loggable)")
    public void loggable() {
    }

    @Around("execution(* com.acme.service.*.*(..))")
    public Object logServiceCalls(ProceedingJoinPoint pjp) throws Throwable {
        System.out.println("before " + pjp.getSignature());
        try {
            return pjp.proceed();
        } finally {
            System.out.println("after " + pjp.getSignature());
        }
    }

    @AfterReturning(pointcut = "loggable()", returning = "result")
    public void auditLoggable(JoinPoint jp, Object result) {
        System.out.println("audit " + jp.getSignature() + " -> " + result);
    }

    @Before("bean(orderService)")
    public void beforeOrderServiceBean(JoinPoint jp) {
        System.out.println("bean advice " + jp.getSignature());
    }
}
