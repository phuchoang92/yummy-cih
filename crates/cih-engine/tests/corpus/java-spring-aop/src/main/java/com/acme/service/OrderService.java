package com.acme.service;

import com.acme.annotation.Loggable;
import org.springframework.stereotype.Service;

@Service
public class OrderService {

    @Loggable
    public String pay(String orderId, int amount) {
        return "paid:" + orderId + ":" + amount;
    }

    public String refund(String orderId) {
        return "refunded:" + orderId;
    }
}
