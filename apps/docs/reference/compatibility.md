# Stability and compatibility

**Status: stable.** The inference API under `/api/inference/v1/` is stable as of contract version 1.0.0 (2026-11-06). The [OpenAPI artifact](/openapi/inference-v1.json) is the contract; its `info.version` gives the version you are reading.

## What can change within v1

Version 1 changes only compatibly. A later 1.x contract may add:

- new endpoints;
- new optional request fields;
- new response fields, including inside `usage`, choices, and problem details;
- new SSE event names;
- new values of open strings, such as `finish_reason`, problem `code`, and annotation `kind`.

Within v1 the gateway will not remove or rename a field, endpoint, or SSE event, make an optional request field required, change a field's type, or change a terminal event's meaning. The closed lists in the contract, such as message `role` and usage `quality` and `completeness`, gain no new values without a new version.

## What clients must do

- **Ignore what you do not know.** Accept response fields and SSE events that your client does not recognize, and do not reject a response because of them.
- **Treat open strings as open.** Handle an unknown `finish_reason` or problem `code` by its HTTP status.
- **Send only documented request fields.** Request schemas are closed: an unknown request field is refused with `422`, so a typo is caught rather than ignored.

## Breaking changes

A breaking change ships under a new URL version, with the `v1` path segment replaced and its own OpenAPI artifact. Version 1 keeps working alongside it; how long is announced with the new version.

Each change to the contract is checked in CI against the frozen 1.0.0 contract, and fails if it would break a version 1 client.

> **Scoped:** compatibility covers the HTTP contract. Which models a deployment offers, their prices, and their limits are deployment configuration, not part of the contract.
