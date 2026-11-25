package com.qucelerate.aigateway;

/** An uploaded batch input file, or a batch's output or error file. */
public record FileObject(String id, long bytes, String filename, String purpose, long createdAt) {}
