---
name: Feature request
about: Suggest a capability for MCPTracer
title: "[feat] "
labels: enhancement
---

## Problem

What are you trying to do that MCPTracer does not support today?

## Proposed capability

Describe the feature. If it involves a new subcommand or output, sketch the CLI
shape.

## Which crate owns this?

If you know, name the owning crate (`mcptracer-protocol`, `mcptracer-storage`,
`mcptracer-proxy`, `mcptracer-redact`). See the crate-level documentation.

## Constraints to keep in mind

- stdout stays clean during `record`.
- Sharing/export requires redaction first.
