package com.fasanaradigital.dev.tasks.ob.service;

import com.fasanaradigital.dev.tasks.ob.model.PriceLevel;
import com.fasanaradigital.dev.tasks.ob.model.Side;
import org.junit.jupiter.api.Test;

import java.math.BigDecimal;

import static org.junit.jupiter.api.Assertions.assertEquals;

class OrderBookAggregatorTests {

    @Test
    void shouldAggregateBestPrices() {
        var aggregator = new OrderBookAggregator();
        
        // Add test data and verify aggregation logic
        aggregator.handleOrder(new PriceLevel(
            new BigDecimal("50000.00"),
            new BigDecimal("1.5"),
            Side.BID,
            "exchange1"
        ));
        
        var bestBids = aggregator.getBestBid();
        assertEquals(1, bestBids.size());
        // Add more assertions
    }
    
    @Test
    void shouldCalculateOptimalExecution() {
        // Test smart order routing logic
    }

    // Implement additional tests to prove the correctness of the functions for price and volume aggregation.
}