package com.fasanaradigital.dev.tasks.ob.service;

import com.fasanaradigital.dev.tasks.ob.OrderBookUpdateHandler;
import com.fasanaradigital.dev.tasks.ob.model.OrderBook;
import com.fasanaradigital.dev.tasks.ob.model.PriceLevel;
import com.fasanaradigital.dev.tasks.ob.model.Side;

import java.math.BigDecimal;
import java.util.List;
import java.util.concurrent.ConcurrentHashMap;

public class OrderBookAggregator implements OrderBookUpdateHandler {

    private final ConcurrentHashMap<String, OrderBook> books = new ConcurrentHashMap<>();
    
    public void handleOrder(PriceLevel priceLevel) {
        books.computeIfAbsent(priceLevel.exchangeId(), OrderBook::new)
             .handleOrder(priceLevel);
    }
    
    public record Quote(BigDecimal price, BigDecimal quantity, String exchangeId) {}
    
    public List<Quote> getBestBid() {
        // Implement logic to aggregate best bids across exchanges
        throw new UnsupportedOperationException("To be implemented");
    }
    
    public List<Quote> getBestAsk() {
        // Implement logic to aggregate best asks across exchanges
        throw new UnsupportedOperationException("To be implemented");
    }

    public List<Quote> getBidVolumeForPrice(BigDecimal price) {
        // Implement logic to get available volume across all exchanges for a price X on bid side
        throw new UnsupportedOperationException("To be implemented");
    }

    public List<Quote> getAskVolumeForPrice(BigDecimal price) {
        // Implement logic to get available volume across all exchanges for a price X
        throw new UnsupportedOperationException("To be implemented");
    }

    public List<Quote> getAskPriceForVolume(BigDecimal volumeToExecute) {
        // Implement logic to get the best price at which we can execute X volume across the aggregated books
        throw new UnsupportedOperationException("To be implemented");
    }

    public List<Quote> getBidPriceForVolume(BigDecimal volumeToExecute) {
        // Implement logic to get the best price at which we can execute X volume across the aggregated books
        throw new UnsupportedOperationException("To be implemented");
    }
    
    public record ExecutionPlan(List<Quote> quotes, BigDecimal totalQuantity) {}
    
    public ExecutionPlan calculateOptimalExecution(Side side, BigDecimal targetQuantity) {
        // Implement smart order routing logic
        throw new UnsupportedOperationException("To be implemented");
    }
}