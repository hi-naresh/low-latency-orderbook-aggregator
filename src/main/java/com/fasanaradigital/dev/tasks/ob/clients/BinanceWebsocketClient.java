package com.fasanaradigital.dev.tasks.ob.clients;

import com.fasanaradigital.dev.tasks.ob.OrderBookUpdateHandler;
import com.fasanaradigital.dev.tasks.ob.model.PriceLevel;
import com.fasanaradigital.dev.tasks.ob.model.Side;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import lombok.extern.slf4j.Slf4j;
import org.java_websocket.client.WebSocketClient;
import org.java_websocket.handshake.ServerHandshake;

import java.math.BigDecimal;
import java.net.URI;

@Slf4j
public class BinanceWebsocketClient extends WebSocketClient {
    private static final String WEBSOCKET_URL = "wss://fstream.binance.com/ws";
    private final ObjectMapper mapper = new ObjectMapper();
    private final OrderBookUpdateHandler handler;

    public BinanceWebsocketClient(OrderBookUpdateHandler handler) {
        super(URI.create(WEBSOCKET_URL));
        this.handler = handler;
    }

    @Override
    public void onOpen(ServerHandshake handshake) {
        String subscribeMsg = """
            {
                "method": "SUBSCRIBE",
                "params": ["btcusdt@depth20@100ms"],
                "id": 1
            }
            """;
        send(subscribeMsg);
    }

    @Override
    public void onMessage(String message) {
        try {
            JsonNode root = mapper.readTree(message);
            if (root.has("b") && root.has("a")) {
                // Process bids and asks
                root.get("b").forEach(bid ->
                        handler.handleOrder(new PriceLevel(
                                new BigDecimal(bid.get(0).asText()),
                                new BigDecimal(bid.get(1).asText()),
                                Side.BID,
                                "binance"
                        ))
                );
                root.get("a").forEach(ask ->
                        handler.handleOrder(new PriceLevel(
                                new BigDecimal(ask.get(0).asText()),
                                new BigDecimal(ask.get(1).asText()),
                                Side.ASK,
                                "binance"
                        ))
                );
            }
        } catch (Exception e) {
            log.error("Could not parse message", e);
        }
    }

    @Override
    public void onClose(int code, String reason, boolean remote) {
        log.info("Binance connection closed: {}", reason);
    }

    @Override
    public void onError(Exception ex) {
        log.error("Binance error", ex);
    }
}