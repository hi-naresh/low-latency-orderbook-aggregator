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