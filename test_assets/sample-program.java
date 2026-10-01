package com.example.editor.samples;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.time.Instant;
import java.util.*;
import java.util.concurrent.*;
import java.util.function.Predicate;
import java.util.stream.Collectors;

/**
 * Sample Java file for editor syntax highlighting, code folding,
 * and language server testing.
 */
public class EventRouter<T> implements AutoCloseable {

    public record RouteKey(String topic, int partition) {
        public RouteKey {
            Objects.requireNonNull(topic, "Topic cannot be null");
            if (partition < 0) {
                throw new IllegalArgumentException("Partition must be non-negative");
            }
        }
    }

    public record Envelope<T>(
            String id,
            RouteKey key,
            T payload,
            Instant timestamp,
            Map<String, String> headers
    ) {
        public static <T> Envelope<T> of(RouteKey key, T payload) {
            return new Envelope<>(
                    UUID.randomUUID().toString(),
                    key,
                    payload,
                    Instant.now(),
                    Collections.emptyMap()
            );
        }
    }

    @FunctionalInterface
    public interface MessageHandler<T> {
        void handle(Envelope<T> message) throws Exception;
    }

    private final String routerName;
    private final ConcurrentMap<RouteKey, List<MessageHandler<T>>> routes = new ConcurrentHashMap<>();
    private final ExecutorService executorService;
    private volatile boolean running = true;

    public EventRouter(String routerName, int threadPoolSize) {
        this.routerName = Objects.requireNonNull(routerName);
        this.executorService = Executors.newFixedThreadPool(
                Math.max(1, threadPoolSize),
                new ThreadFactory() {
                    private int count = 0;
                    @Override
                    public synchronized Thread newThread(Runnable r) {
                        Thread thread = new Thread(r, routerName + "-worker-" + (++count));
                        thread.setDaemon(true);
                        return thread;
                    }
                }
        );
    }

    public void registerHandler(RouteKey key, MessageHandler<T> handler) {
        routes.computeIfAbsent(key, k -> new CopyOnWriteArrayList<>()).add(handler);
    }

    public CompletableFuture<Void> routeAsync(Envelope<T> envelope) {
        if (!running) {
            return CompletableFuture.failedFuture(new IllegalStateException("Router is stopped"));
        }

        List<MessageHandler<T>> handlers = routes.getOrDefault(envelope.key(), Collections.emptyList());
        if (handlers.isEmpty()) {
            return CompletableFuture.completedFuture(null);
        }

        List<CompletableFuture<Void>> futures = handlers.stream()
                .map(handler -> CompletableFuture.runAsync(() -> {
                    try {
                        handler.handle(envelope);
                    } catch (Exception ex) {
                        System.err.printf("Error handling message %s: %s%n", envelope.id(), ex.getMessage());
                    }
                }, executorService))
                .toList();

        return CompletableFuture.allOf(futures.toArray(new CompletableFuture[0]));
    }

    public List<RouteKey> findRoutesMatching(Predicate<RouteKey> predicate) {
        return routes.keySet().stream()
                .filter(predicate)
                .sorted(Comparator.comparing(RouteKey::topic).thenComparingInt(RouteKey::partition))
                .collect(Collectors.toList());
    }

    @Override
    public void close() throws Exception {
        running = false;
        executorService.shutdown();
        if (!executorService.awaitTermination(3, TimeUnit.SECONDS)) {
            executorService.shutdownNow();
        }
    }

    public static void main(String[] args) throws Exception {
        try (var router = new EventRouter<String>("sample-router", 2)) {
            RouteKey ordersKey = new RouteKey("orders.v1", 0);

            router.registerHandler(ordersKey, msg -> {
                System.out.printf("[%s] Received order payload: %s%n", Thread.currentThread().getName(), msg.payload());
            });

            var message = Envelope.of(ordersKey, "Order #12345 confirmed");
            router.routeAsync(message).get(5, TimeUnit.SECONDS);
        }
    }
}
