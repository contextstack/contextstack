import re

TYPES = {"object": (dict,), "array": (list,), "integer": (int,), "string": (str,),
         "null": (type(None),), "number": (int, float), "boolean": (bool,)}


def matches_schema(schema, value):
    """Check the JSON Schema keywords used by this receipt contract."""
    kind = schema.get("type")
    if kind:
        assert any(type(value) in TYPES[name] for name in ([kind] if isinstance(kind, str) else kind)), kind
    if "const" in schema:
        assert value == schema["const"]
    if "enum" in schema:
        assert value in schema["enum"]
    if isinstance(value, str) and "pattern" in schema:
        assert re.search(schema["pattern"], value)
    if isinstance(value, dict):
        props = schema.get("properties", {})
        assert value.keys() >= set(schema.get("required", ()))
        additional = schema.get("additionalProperties")
        if additional is False:
            assert value.keys() <= props.keys()
        if isinstance(additional, dict):
            for key in value.keys() - props.keys():
                matches_schema(additional, value[key])
        for key in value.keys() & props.keys():
            matches_schema(props[key], value[key])
    if isinstance(value, list) and "items" in schema:
        for item in value:
            matches_schema(schema["items"], item)
