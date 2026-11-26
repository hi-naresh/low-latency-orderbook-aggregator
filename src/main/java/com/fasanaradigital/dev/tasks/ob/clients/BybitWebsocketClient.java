package com.fasanaradigital.dev.tasks.ob.clients;

import com.fasanaradigital.dev.tasks.ob.OrderBookUpdateHandler;
import com.fasanaradigital.dev.tasks.ob.model.PriceLevel;
import com.fasanaradigital.dev.tasks.ob.model.Side;
import lombok.extern.slf4j.Slf4j;
import org.java_websocket.client.WebSocketClient;
import org.java_websocket.handshake.ServerHandshake;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.JsonNode;

import java.math.BigDecimal;
import java.net.URI;

@Slf4j
public class BybitWebsocketClient extends WebSocketClient {

    private static final String WEBSOCKET_URL = "wss://stream.bybit.com/v5/public/linear";
    private final ObjectMapper mapper = new ObjectMapper();
    private final OrderBookUpdateHandler handler;

    public BybitWebsocketClient(OrderBookUpdateHandler handler) {
        super(URI.create(WEBSOCKET_URL));
        this.handler = handler;
    }

    @Override
    public void onOpen(ServerHandshake handshake) {
        log.info("Bybit connection opened");
        String subscribeMsg = """
            {
                "req_id": "depth",
                "op": "subscribe",
                "args": ["orderbook.50.BTCUSDT"]
            }
            """;

        send(subscribeMsg);
    }

    @Override
    public void onMessage(String message) {
        log.debug(message);
        try {
            JsonNode root = mapper.readTree(message);
            if (root.has("data")) {
                JsonNode data = root.get("data");
                // Process bids and asks
                data.get("b").forEach(bid ->
                        handler.handleOrder(new PriceLevel(
                                new BigDecimal(bid.get(0).asText()),
                                new BigDecimal(bid.get(1).asText()),
                                Side.BID,
                                "bybit"
                        ))
                );
                data.get("a").forEach(ask ->
                        handler.handleOrder(new PriceLevel(
                                new BigDecimal(ask.get(0).asText()),
                                new BigDecimal(ask.get(1).asText()),
                                Side.ASK,
                                "bybit"
                        ))
                );
            }
        } catch (Exception e) {
            log.error("Could not parse message", e);
        }
    }

    @Override
    public void onClose(int code, String reason, boolean remote) {
        log.info("Bybit connection closed: {}", reason);
    }

    @Override
    public void onError(Exception ex) {
        log.error("Bybit error", ex);
    }
}

