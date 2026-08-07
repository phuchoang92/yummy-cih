package com.acme.web;

import com.acme.annotation.Loggable;
import com.acme.service.OrderService;
import org.springframework.web.bind.annotation.PostMapping;
import org.springframework.web.bind.annotation.RequestBody;
import org.springframework.web.bind.annotation.RestController;

@RestController
public class OrderController {

    private final OrderService orderService;

    public OrderController(OrderService orderService) {
        this.orderService = orderService;
    }

    @Loggable
    @PostMapping("/api/orders")
    public String create(@RequestBody String orderId) {
        return orderService.pay(orderId, 100);
    }
}
