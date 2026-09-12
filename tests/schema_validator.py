"""Minimal JSON Schema (draft 2020-12 subset) validator, with no external
dependencies. This project's test suite is pure stdlib on purpose; this
validator only ever needs to check MCPTracer's own schemas/*.schema.json
files against real CLI output, and those schemas are deliberately written to
stay within the subset implemented here: type, properties, required,
additionalProperties, enum, const, items, oneOf, $ref to a local $defs
entry, minimum/maximum. It does not aim to be a general-purpose validator --
do not reach for it outside this test suite.
"""

from __future__ import annotations

from typing import Any


class SchemaValidationError(Exception):
    def __init__(self, path: str, message: str) -> None:
        self.path = path
        self.message = message
        super().__init__(f"{path}: {message}")


def validate(instance: Any, schema: dict, root: dict | None = None, path: str = "$") -> None:
    if root is None:
        root = schema

    if "$ref" in schema:
        schema = _resolve_ref(schema["$ref"], root)

    if "oneOf" in schema:
        errors = []
        matches = 0
        for branch in schema["oneOf"]:
            try:
                validate(instance, branch, root, path)
                matches += 1
            except SchemaValidationError as error:
                errors.append(str(error))
        if matches != 1:
            raise SchemaValidationError(
                path, f"expected exactly one oneOf branch to match, {matches} did: {errors}"
            )
        return

    if "const" in schema and instance != schema["const"]:
        raise SchemaValidationError(path, f"expected const {schema['const']!r}, got {instance!r}")

    if "enum" in schema and instance not in schema["enum"]:
        raise SchemaValidationError(path, f"{instance!r} not in enum {schema['enum']!r}")

    if "type" in schema:
        types = schema["type"] if isinstance(schema["type"], list) else [schema["type"]]
        if not any(_matches_type(instance, t) for t in types):
            raise SchemaValidationError(
                path, f"expected type {types}, got {type(instance).__name__} ({instance!r})"
            )

    if isinstance(instance, dict):
        for key in schema.get("required", []):
            if key not in instance:
                raise SchemaValidationError(path, f"missing required property {key!r}")
        properties = schema.get("properties", {})
        if schema.get("additionalProperties") is False:
            extra = sorted(set(instance.keys()) - set(properties.keys()))
            if extra:
                raise SchemaValidationError(path, f"unexpected propert(y/ies): {extra}")
        for key, subschema in properties.items():
            if key in instance:
                validate(instance[key], subschema, root, f"{path}.{key}")

    if isinstance(instance, list) and "items" in schema:
        for index, item in enumerate(instance):
            validate(item, schema["items"], root, f"{path}[{index}]")

    if isinstance(instance, (int, float)) and not isinstance(instance, bool):
        if "minimum" in schema and instance < schema["minimum"]:
            raise SchemaValidationError(path, f"{instance} < minimum {schema['minimum']}")
        if "maximum" in schema and instance > schema["maximum"]:
            raise SchemaValidationError(path, f"{instance} > maximum {schema['maximum']}")


def _matches_type(instance: Any, type_name: str) -> bool:
    if type_name == "null":
        return instance is None
    if type_name == "string":
        return isinstance(instance, str)
    if type_name == "boolean":
        return isinstance(instance, bool)
    if type_name == "integer":
        return isinstance(instance, int) and not isinstance(instance, bool)
    if type_name == "number":
        return isinstance(instance, (int, float)) and not isinstance(instance, bool)
    if type_name == "object":
        return isinstance(instance, dict)
    if type_name == "array":
        return isinstance(instance, list)
    raise ValueError(f"unsupported JSON Schema type: {type_name!r}")


def _resolve_ref(ref: str, root: dict) -> dict:
    if not ref.startswith("#/"):
        raise ValueError(f"only local refs are supported by this minimal validator, got {ref!r}")
    node: Any = root
    for part in ref[2:].split("/"):
        node = node[part]
    return node
