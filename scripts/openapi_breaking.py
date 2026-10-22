#!/usr/bin/env python3
"""Fail when a new OpenAPI document breaks the previous /api/v1 contract.

A break is a removed path or method, a removed parameter, a removed property,
a property whose type, format, enum, or ref shape changed, or a property or
parameter that became required. Added optional fields and added paths are
allowed. Descriptions are ignored.

Usage: openapi_breaking.py OLD.json NEW.json
       openapi_breaking.py --self-test
"""

from __future__ import annotations

import json
import sys
from typing import Any


METHODS = {"get", "post", "put", "delete", "patch", "head", "options", "trace"}


def load(path: str) -> dict:
    with open(path, encoding="utf-8") as fh:
        doc = json.load(fh)
    if not isinstance(doc, dict):
        raise SystemExit(f"{path} is not a JSON object")
    return doc


def lookup(spec: dict, ref: str) -> dict:
    if not ref.startswith("#/"):
        return {}
    node: Any = spec
    for part in ref[2:].split("/"):
        part = part.replace("~1", "/").replace("~0", "~")
        if not isinstance(node, dict) or part not in node:
            return {}
        node = node[part]
    return node if isinstance(node, dict) else {}


def resolve(spec: dict, schema: Any, depth: int = 0) -> Any:
    if not isinstance(schema, dict) or depth > 40:
        return schema
    if "$ref" not in schema:
        return schema
    target = resolve(spec, lookup(spec, schema["$ref"]), depth + 1)
    if not isinstance(target, dict):
        return target
    if len(schema) == 1:
        return target
    merged = dict(target)
    for key, value in schema.items():
        if key != "$ref":
            merged[key] = value
    return merged


def merge(spec: dict, schema: Any, depth: int = 0) -> Any:
    schema = resolve(spec, schema, depth)
    if not isinstance(schema, dict) or "allOf" not in schema or depth > 40:
        return schema
    props: dict = {}
    required: list = []
    out: dict = {}
    for part in schema["allOf"]:
        part = merge(spec, part, depth + 1)
        if not isinstance(part, dict):
            continue
        props.update(part.get("properties") or {})
        required.extend(part.get("required") or [])
        for key, value in part.items():
            if key not in ("properties", "required", "allOf"):
                out.setdefault(key, value)
    props.update(schema.get("properties") or {})
    required.extend(schema.get("required") or [])
    for key, value in schema.items():
        if key not in ("allOf", "properties", "required"):
            out[key] = value
    out["properties"] = props
    if required:
        out["required"] = list(dict.fromkeys(required))
    return out


def type_sig(spec: dict, schema: Any) -> str:
    schema = merge(spec, schema)
    if isinstance(schema, bool):
        return "true" if schema else "false"
    if not isinstance(schema, dict):
        return json.dumps(schema, sort_keys=True)
    for key in ("oneOf", "anyOf"):
        if key in schema:
            parts = sorted(type_sig(spec, item) for item in schema[key])
            return json.dumps([key, parts], sort_keys=True)
    kind = schema.get("type")
    if isinstance(kind, list):
        kind = sorted(str(item) for item in kind)
    enum = schema.get("enum")
    if isinstance(enum, list):
        enum = sorted(json.dumps(item, sort_keys=True) for item in enum)
    items = type_sig(spec, schema["items"]) if "items" in schema else None
    extra = schema.get("additionalProperties")
    if isinstance(extra, dict):
        extra = type_sig(spec, extra)
    const = schema.get("const")
    return json.dumps(
        {
            "type": kind,
            "format": schema.get("format"),
            "enum": enum,
            "const": const,
            "items": items,
            "additionalProperties": extra,
            "minimum": schema.get("minimum"),
            "maximum": schema.get("maximum"),
        },
        sort_keys=True,
    )


def collect(spec: dict, schema: Any, prefix: str, props: dict, required: set) -> None:
    schema = merge(spec, schema)
    if not isinstance(schema, dict):
        return
    declared = schema.get("properties")
    if isinstance(declared, dict):
        req = set(schema.get("required") or [])
        for name in sorted(declared):
            path = f"{prefix}.{name}" if prefix else name
            props[path] = type_sig(spec, declared[name])
            if name in req:
                required.add(path)
            collect(spec, declared[name], path, props, required)
    if "items" in schema:
        collect(spec, schema["items"], prefix + "[]", props, required)
    for key in ("oneOf", "anyOf"):
        if key in schema and isinstance(schema[key], list):
            for index, item in enumerate(schema[key]):
                collect(spec, item, f"{prefix}|{key}{index}", props, required)


def operation_anchors(spec: dict) -> dict:
    found = {}
    for path, item in sorted((spec.get("paths") or {}).items()):
        if not isinstance(item, dict):
            continue
        for method, op in sorted(item.items()):
            if method not in METHODS or not isinstance(op, dict):
                continue
            label = f"{method.upper()} {path}"
            body = (op.get("requestBody") or {}).get("content") or {}
            if isinstance(body, dict):
                for ctype, media in sorted(body.items()):
                    if isinstance(media, dict) and "schema" in media:
                        found[f"{label} request {ctype}"] = media["schema"]
            responses = op.get("responses") or {}
            if isinstance(responses, dict):
                for status, resp in sorted(responses.items()):
                    if not isinstance(resp, dict):
                        continue
                    # Error statuses may be documented more tightly later.
                    # A missing 2xx body is a break; a missing 4xx write-up is not.
                    if not str(status).startswith("2"):
                        continue
                    content = resp.get("content") or {}
                    if isinstance(content, dict):
                        for ctype, media in sorted(content.items()):
                            if isinstance(media, dict) and "schema" in media:
                                found[f"{label} response {status} {ctype}"] = media["schema"]
    return found


def parameters(spec: dict) -> dict:
    found = {}
    for path, item in sorted((spec.get("paths") or {}).items()):
        if not isinstance(item, dict):
            continue
        shared = item.get("parameters") or []
        for method, op in sorted(item.items()):
            if method not in METHODS or not isinstance(op, dict):
                continue
            label = f"{method.upper()} {path}"
            for param in list(shared) + list(op.get("parameters") or []):
                param = resolve(spec, param)
                if not isinstance(param, dict) or "name" not in param:
                    continue
                where = param.get("in", "")
                key = f"{label} param {where} {param['name']}"
                found[key] = (
                    bool(param.get("required")),
                    type_sig(spec, param.get("schema") or {}),
                )
    return found


def paths_and_methods(spec: dict) -> dict:
    out = {}
    for path, item in sorted((spec.get("paths") or {}).items()):
        if not isinstance(item, dict):
            continue
        out[path] = sorted(method for method in item if method in METHODS)
    return out


def compare(old: dict, new: dict) -> list[str]:
    problems = []
    old_paths = paths_and_methods(old)
    new_paths = paths_and_methods(new)
    for path, methods in old_paths.items():
        if path not in new_paths:
            problems.append(f"removed path {path}")
            continue
        for method in methods:
            if method not in new_paths[path]:
                problems.append(f"removed {method.upper()} {path}")

    old_params = parameters(old)
    new_params = parameters(new)
    for key, (was_required, old_type) in old_params.items():
        if key not in new_params:
            problems.append(f"removed {key}")
            continue
        now_required, new_type = new_params[key]
        if old_type != new_type:
            problems.append(f"changed type of {key}")
        if now_required and not was_required:
            problems.append(f"made required: {key}")
    for key, (now_required, _) in new_params.items():
        if key not in old_params and now_required:
            problems.append(f"added required {key}")

    old_anchors = operation_anchors(old)
    new_anchors = operation_anchors(new)
    for key, schema in old_anchors.items():
        if key not in new_anchors:
            problems.append(f"removed {key}")
            continue
        old_props: dict = {}
        old_req: set = set()
        new_props: dict = {}
        new_req: set = set()
        collect(old, schema, "", old_props, old_req)
        collect(new, new_anchors[key], "", new_props, new_req)
        for prop, old_type in sorted(old_props.items()):
            if prop not in new_props:
                problems.append(f"removed {key} field {prop}")
            elif old_type != new_props[prop]:
                problems.append(f"changed type of {key} field {prop}")
        for prop in sorted(new_req - old_req):
            problems.append(f"made required: {key} field {prop}")
    return problems


def self_test() -> None:
    base = {
        "openapi": "3.1.0",
        "paths": {
            "/api/v1/state": {
                "get": {
                    "parameters": [
                        {
                            "name": "activity",
                            "in": "query",
                            "required": False,
                            "schema": {"type": "integer"},
                        }
                    ],
                    "responses": {
                        "200": {
                            "content": {
                                "application/json": {
                                    "schema": {"$ref": "#/components/schemas/Envelope"}
                                }
                            }
                        }
                    },
                }
            }
        },
        "components": {
            "schemas": {
                "Envelope": {
                    "type": "object",
                    "required": ["rev"],
                    "properties": {
                        "rev": {"type": "integer"},
                        "phase": {"type": "string", "enum": ["Idle", "Error"]},
                    },
                }
            }
        },
    }

    def check(mutated: dict, needle: str) -> None:
        found = compare(base, mutated)
        if not any(needle in item for item in found):
            raise SystemExit(f"self-test missed {needle!r}: {found}")

    added = json.loads(json.dumps(base))
    added["components"]["schemas"]["Envelope"]["properties"]["extra"] = {"type": "boolean"}
    if compare(base, added):
        raise SystemExit(f"optional field should pass: {compare(base, added)}")

    removed_path = json.loads(json.dumps(base))
    del removed_path["paths"]["/api/v1/state"]
    check(removed_path, "removed path")

    removed_field = json.loads(json.dumps(base))
    del removed_field["components"]["schemas"]["Envelope"]["properties"]["rev"]
    check(removed_field, "removed")

    changed = json.loads(json.dumps(base))
    changed["components"]["schemas"]["Envelope"]["properties"]["rev"] = {"type": "string"}
    check(changed, "changed type")

    enum_lost = json.loads(json.dumps(base))
    enum_lost["components"]["schemas"]["Envelope"]["properties"]["phase"]["enum"] = ["Idle"]
    check(enum_lost, "changed type")

    required = json.loads(json.dumps(base))
    required["components"]["schemas"]["Envelope"]["required"].append("phase")
    check(required, "made required")

    print("openapi breaking-change self-test passed")


def main() -> None:
    if len(sys.argv) == 2 and sys.argv[1] == "--self-test":
        self_test()
        return
    if len(sys.argv) != 3:
        raise SystemExit("usage: openapi_breaking.py OLD.json NEW.json")
    problems = compare(load(sys.argv[1]), load(sys.argv[2]))
    if problems:
        print("openapi breaking changes:", file=sys.stderr)
        for item in problems:
            print(f"  {item}", file=sys.stderr)
        raise SystemExit(1)
    print("openapi breaking-change check passed")


if __name__ == "__main__":
    main()
