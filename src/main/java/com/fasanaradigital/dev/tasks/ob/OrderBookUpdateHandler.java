package com.fasanaradigital.dev.tasks.ob;

import com.fasanaradigital.dev.tasks.ob.model.PriceLevel;

public interface OrderBookUpdateHandler {
    void handleOrder(PriceLevel priceLevel);
}