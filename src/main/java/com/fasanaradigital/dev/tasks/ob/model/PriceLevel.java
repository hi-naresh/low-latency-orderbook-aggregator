package com.fasanaradigital.dev.tasks.ob.model;

import java.math.BigDecimal;

public record PriceLevel(
    BigDecimal price,
    BigDecimal quantity,
    Side side,
    String exchangeId
) {}