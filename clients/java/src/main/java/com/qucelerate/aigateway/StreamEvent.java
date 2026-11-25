package com.qucelerate.aigateway;

import com.fasterxml.jackson.annotation.JsonValue;
import com.fasterxml.jackson.databind.JsonNode;
import java.util.List;

/**
 * An event of a chat stream. Events this version does not know are skipped; a
 * terminal {@code error} event ends the stream with a {@link StreamException}.
 */
public sealed interface StreamEvent {
    /** The SSE event name. */
    String eventName();

    /** The first event of a stream. */
    record ResponseStarted(String model) implements StreamEvent {
        public String eventName() {
            return "response.started";
        }
    }

    /** More content or refusal text for a choice. */
    record ChoiceDelta(int choiceIndex, String contentDelta, String refusalDelta, List<JsonNode> annotations)
            implements StreamEvent {
        public String eventName() {
            return "choice.delta";
        }
    }

    /** Part of a tool call; {@code id} and {@code name} arrive in its first delta only. */
    record ToolCallDelta(int choiceIndex, int toolCallIndex, String id, String name, String argumentsDelta)
            implements StreamEvent {
        public String eventName() {
            return "tool_call.delta";
        }
    }

    /** A choice is complete. */
    record ChoiceFinished(int choiceIndex, String finishReason) implements StreamEvent {
        public String eventName() {
            return "choice.finished";
        }
    }

    /** A provider safety signal for a choice. */
    record SafetyAnnotation(int choiceIndex, String category, boolean blocked) implements StreamEvent {
        public String eventName() {
            return "safety.annotation";
        }
    }

    /** The usage event; its payload is the usage itself. */
    record UsageReport(@JsonValue Usage usage) implements StreamEvent {
        public String eventName() {
            return "usage";
        }
    }

    /** The last event of a successful stream. */
    record ResponseCompleted(String model, Usage usage) implements StreamEvent {
        public String eventName() {
            return "response.completed";
        }
    }
}
