#!/usr/bin/env python3
"""Validate the proposed Axo contract and its positive/negative schema fixtures."""
import argparse
import json
from pathlib import Path
import sys

import yaml
from jsonschema import Draft202012Validator, FormatChecker, ValidationError, validators
from openapi_spec_validator import validate
from referencing import Registry, Resource
from referencing.jsonschema import DRAFT202012

BASE = "urn:clever-csi:openapi-validation"

def utf8_bounds(validator, maximum, instance, schema):
    if isinstance(instance, str) and len(instance.encode("utf-8")) > maximum:
        yield ValidationError(f"UTF-8 length exceeds documented x-maxUtf8Bytes={maximum}")

def serialized_bounds(validator, maximum, instance, schema):
    serialized = json.dumps(instance, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode("utf-8")
    if len(serialized) > maximum:
        yield ValidationError(f"Serialized JSON exceeds documented x-maxSerializedBytes={maximum}")

ContractValidator = validators.extend(Draft202012Validator, {
    "x-maxUtf8Bytes": utf8_bounds,
    "x-maxSerializedBytes": serialized_bounds,
})


class UniqueLoader(yaml.SafeLoader):
    pass

def unique_mapping(loader, node, deep=False):
    result = {}
    for key_node, value_node in node.value:
        key = loader.construct_object(key_node, deep=deep)
        if key in result:
            raise yaml.constructor.ConstructorError("mapping", node.start_mark,
                f"duplicate YAML key {key!r}", key_node.start_mark)
        result[key] = loader.construct_object(value_node, deep=deep)
    return result

UniqueLoader.add_constructor(yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, unique_mapping)

def main():
    parser = argparse.ArgumentParser()
    root = Path(__file__).resolve().parents[1]
    parser.add_argument("document", type=Path, nargs="?", default=root / "docs/axo-openapi.yaml")
    parser.add_argument("--cases", type=Path, default=root / "deploy/fixtures/axo-contract.json",
        help="JSON array: {label, schema, valid, value}")
    args = parser.parse_args()
    document = yaml.load(args.document.read_text(), Loader=UniqueLoader)
    assert document.get("openapi", "").startswith("3.1."), "OpenAPI 3.1 required"
    validate(document)
    registry = Registry().with_resource(BASE, Resource(document, DRAFT202012))
    schemas = document.get("components", {}).get("schemas", {})
    for name, schema in schemas.items():
        Draft202012Validator.check_schema(schema)
    operations = []
    for path, item in document.get("paths", {}).items():
        for method, operation in item.items():
            if method in {"get", "post", "put", "patch", "delete", "head", "options", "trace"}:
                operations.append((method, path, operation.get("operationId")))
    ids = [operation_id for _, _, operation_id in operations]
    assert all(ids), "every operation must have operationId"
    assert len(ids) == len(set(ids)), "operationIds must be unique"
    catalogue = document["x-csi-error-codes"]
    for path, item in document["paths"].items():
        for method, operation in item.items():
            if method not in {"get", "post", "put", "patch", "delete", "head", "options", "trace"}:
                continue
            for code in operation.get("x-csi-error-codes", []):
                assert code in catalogue, f"{method} {path}: unknown code {code}"
                status = str(catalogue[code]["httpStatus"])
                assert status in operation["responses"], f"{method} {path}: no response for {code} ({status})"
    failures = []
    cases = json.loads(args.cases.read_text()) if args.cases else []
    for case in cases:
        name = case["schema"]
        if name not in schemas:
            failures.append(f"{case['label']}: schema {name} missing")
            continue
        validator = ContractValidator({"$ref": f"{BASE}#/components/schemas/{name}"},
            registry=registry, format_checker=FormatChecker())
        errors = list(validator.iter_errors(case["value"]))
        actual = not errors
        if actual != case["valid"]:
            reason = "; ".join(f"{'/'.join(map(str, error.path))}: {error.message}" for error in errors[:3])
            failures.append(f"{case['label']}: expected valid={case['valid']}, got {actual}; {reason}")
    print(f"OpenAPI 3.1 valid; {len(schemas)} schemas, {len(operations)} unique operations; {len(cases)-len(failures)}/{len(cases)} fixture cases passed")
    for failure in failures:
        print(f"FAIL: {failure}")
    if failures:
        return 1
    return 0

if __name__ == "__main__":
    sys.exit(main())
