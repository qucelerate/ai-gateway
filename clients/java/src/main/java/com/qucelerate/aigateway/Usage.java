package com.qucelerate.aigateway;

/**
 * Token usage. A counter the provider did not report is {@code null}, never 0.
 * {@code reasoningTokens} is counted apart from {@code outputTokens}; {@code null}
 * means the provider does not report reasoning, which is then inside
 * {@code outputTokens}. {@code quality} is {@code exact}, {@code estimated} or
 * {@code unknown}; {@code completeness} is {@code complete}, {@code partial} or
 * {@code unavailable}. Treat other values as unknown.
 */
public record Usage(
        Long inputTokens,
        Long outputTokens,
        Long cachedInputTokens,
        Long reasoningTokens,
        Long otherTokens,
        String quality,
        String completeness) {}
