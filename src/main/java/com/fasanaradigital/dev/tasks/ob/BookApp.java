package com.fasanaradigital.dev.tasks.ob;

import com.fasanaradigital.dev.tasks.ob.clients.BinanceWebsocketClient;
import com.fasanaradigital.dev.tasks.ob.clients.BybitWebsocketClient;
import com.fasanaradigital.dev.tasks.ob.service.OrderBookAggregator;
import lombok.extern.slf4j.Slf4j;

import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;

@Slf4j
public class BookApp {

    public static void main(String[] args) {

        var aggregator = new OrderBookAggregator();

        // Create websocket clients
        var bybitClient = new BybitWebsocketClient(aggregator);
        var binanceClient = new BinanceWebsocketClient(aggregator);

        // Connect to feeds
        binanceClient.connect();
        bybitClient.connect();

        // Schedule periodic best price printing
        ScheduledExecutorService scheduler = Executors.newScheduledThreadPool(1);
        scheduler.scheduleAtFixedRate(() -> {
            log.info("=== Best Prices ===");
            aggregator.getBestBid().forEach(quote ->
                    log.info("Best Bid: {} @ {} ({})", quote.price(), quote.quantity(), quote.exchangeId()));
            aggregator.getBestAsk().forEach(quote ->
                    log.info("Best Ask: {} @ {} ({})", quote.price(), quote.quantity(), quote.exchangeId()));
        }, 0, 1, TimeUnit.SECONDS);

        // Add shutdown hook
        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            binanceClient.close();
            bybitClient.close();
            scheduler.shutdown();
        }));
    }
}
