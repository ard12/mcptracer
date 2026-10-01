"""Independent Python reference for the frozen Rust identity vectors."""
import hashlib
import json
from pathlib import Path
import unittest

GOLDEN = Path(__file__).parent / "golden"

def strict_load(path):
    def reject_constant(value):
        raise ValueError(f"unsupported JSON numeric constant: {value}")
    return json.loads(path.read_text(encoding="utf-8"), parse_constant=reject_constant)

def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False)

class IdentityGoldenVectors(unittest.TestCase):
    def check_vector(self, identity, expected, digest):
        encoded = canonical(identity)
        self.assertEqual(encoded, expected)
        self.assertEqual(hashlib.sha256(encoded.encode("utf-8")).hexdigest(), digest)

    def test_tool_vectors(self):
        for vector in strict_load(GOLDEN / "tool-identity-vectors.json"):
            identity = {key: vector[key] for key in (
                "name", "title", "description", "inputSchema", "outputSchema", "annotations"
            )}
            self.check_vector(identity, vector["canonical"], vector["sha256"])

    def test_artifact_vector(self):
        identity = strict_load(GOLDEN / "artifact-identity-input.json")
        expected = (GOLDEN / "artifact-identity.canonical.json").read_text(encoding="utf-8").rstrip("\n")
        digest = (GOLDEN / "artifact-identity.sha256").read_text(encoding="ascii").strip()
        self.check_vector(identity, expected, digest)

    def test_non_json_numeric_constants_are_rejected(self):
        with self.assertRaises(ValueError):
            json.loads('{"value": NaN}', parse_constant=lambda value: (_ for _ in ()).throw(ValueError(value)))

if __name__ == "__main__":
    unittest.main()
