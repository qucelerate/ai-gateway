package com.qucelerate.aigateway;

import java.util.List;

/** A page of batches, newest first. */
public record BatchList(List<Batch> data, boolean hasMore) {
    public BatchList {
        data = data == null ? List.of() : List.copyOf(data);
    }
}
