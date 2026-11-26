# Crypto Order Book Aggregator

## Overview
This project implements a real-time order book aggregator for cryptocurrency markets. It connects to Binance and Bybit WebSocket feeds for BTCUSDT perpetual futures, maintains order books for each venue, and provides a combined view of liquidity across exchanges.

## Task Requirements

### 1. Order Book Implementation
- Implement order book handling in `OrderBook.java`
- Support adding and removing price levels based on WebSocket messages
- Maintain sorted price levels for bids and asks
- Implement methods to query best bid/ask prices and calculate market impact

### 2. Order Book Aggregator
- Implement aggregation logic in `OrderBookAggregator.java`
- Combine order books from multiple exchanges
- Provide methods to:
    - Get best N levels across all venues
    - Calculate optimal execution path for a given size
    - Handle exchange disconnections gracefully

### 3. Testing Requirements
- Write unit tests for OrderBook operations
- Write unit tests for OrderBookAggregator
- Test edge cases like:
    - Zero quantity updates (price level removal)
    - Multiple updates at same price level
    - Order book crossing scenarios
    - Race conditions in concurrent updates

## Live Data Considerations
Please document potential pitfalls and challenges when handling live order book data from multiple venues. Consider:
- Race conditions
- Data consistency
- Performance bottlenecks
- Error handling
- Market structure issues

Include your analysis in a separate document named `CONSIDERATIONS.md`.

## Project Structure
```
src/
├── main/java/com/fasanaradigital/dev/tasks/ob
│   ├── BookApp.java           # Entry point
│   ├── model/
│   │   ├── PriceLevel.java         # OrderBook price level record
│   │   ├── Side.java               # Enum for bid/ask
│   │   └── OrderBook.java          # Single venue book
│   ├── service/
│   │   └── OrderBookAggregator.java # Multi-venue aggregator
│   └── clients/                        # Exchange WebSocket clients
└── test/java/com/fasanaradigital/dev/tasks/ob
    ├── model/
    │   └── OrderBookTests.java
    └── service/
        └── OrderBookAggregatorTests.java
```

## Evaluation Criteria
- Code organisation and clarity
- Proper handling of concurrent updates
- Test coverage and quality
- Performance considerations
- Error handling and logging
- Documentation
## Appendix: the skeleton's interfaces

The Java skeleton isn't part of this repository; these are the methods it asked for, which
the Rust code's documentation refers to. `README.md` maps each one to its Rust counterpart.

```java
public record PriceLevel(BigDecimal price, BigDecimal quantity, Side side, String exchangeId) {}

public class OrderBook implements OrderBookUpdateHandler {
    public void handleOrder(PriceLevel priceLevel);   // bids highest first, asks lowest first
    public BigDecimal getBestBidPrice();
    public BigDecimal getBestAskPrice();
    public BigDecimal getBestBidSize();
    public BigDecimal getBestAskSize();
    public boolean isCrossed();                       // best bid above best ask
}

public class OrderBookAggregator implements OrderBookUpdateHandler {
    public record Quote(BigDecimal price, BigDecimal quantity, String exchangeId) {}
    public record ExecutionPlan(List<Quote> quotes, BigDecimal totalQuantity) {}

    public void handleOrder(PriceLevel priceLevel);
    public List<Quote> getBestBid();                                  // across exchanges
    public List<Quote> getBestAsk();
    public List<Quote> getBidVolumeForPrice(BigDecimal price);        // volume available at price X
    public List<Quote> getAskVolumeForPrice(BigDecimal price);
    public List<Quote> getAskPriceForVolume(BigDecimal volumeToExecute); // best price for volume X
    public List<Quote> getBidPriceForVolume(BigDecimal volumeToExecute);
    public ExecutionPlan calculateOptimalExecution(Side side, BigDecimal targetQuantity);
}
```

`BookApp` was the entry point: it connected the Binance and Bybit WebSocket clients, which
fed every price level to the aggregator's `handleOrder`, and logged the best bid and ask
every second.
