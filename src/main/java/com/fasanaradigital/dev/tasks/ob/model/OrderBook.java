package com.fasanaradigital.dev.tasks.ob.model;

import com.fasanaradigital.dev.tasks.ob.OrderBookUpdateHandler;

import java.math.BigDecimal;

public class OrderBook implements OrderBookUpdateHandler {

    private final String exchangeId;

    public OrderBook(String exchangeId) {
        this.exchangeId = exchangeId;
    }

    @Override
    public void handleOrder(PriceLevel priceLevel) {
        // Implement method to update internal order book state based on price priority (bid side is highest price first, ask side is lowest price first).
    }

    // Implement methods to get best bid/ask price and size.
    public BigDecimal getBestBidPrice() {
        return null;
    }

    public BigDecimal getBestAskPrice() {
        return null;
    }

    public BigDecimal getBestBidSize() {
        return null;
    }

    public BigDecimal getBestAskSize() {
        return null;
    }

    public boolean isCrossed() {
        // Implement logic to check that order book is crossed (best bid price higher than best ask price)
        return false;
    }
}
