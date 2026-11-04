"""Optional isolated development validation; not a runtime dependency."""
import hashlib
import importlib.metadata
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
local = ROOT/'.tools/schema-validator'
if local.is_dir():
    sys.path.insert(0, str(local))
try:
    from jsonschema import Draft202012Validator
except ImportError:
    raise SystemExit('Install optional jsonschema==4.26.0 into .tools/schema-validator for schema QA')


def main():
    schema_path = ROOT/'contracts/retrieval-v1.schema.json'
    cases_path = ROOT/'tests/fixtures/retrieval-protocol-cases.json'
    schema = json.loads(schema_path.read_text('utf-8'))
    response_path = ROOT/'contracts/retrieval-response-v1.schema.json'
    response_schema = json.loads(response_path.read_text('utf-8'))
    cases = json.loads(cases_path.read_text('utf-8'))
    Draft202012Validator.check_schema(schema)
    Draft202012Validator.check_schema(response_schema)
    validator = Draft202012Validator(schema)
    assert all(validator.is_valid(case) for case in cases['valid_shape'])
    assert all(not validator.is_valid(case) for case in cases['invalid_shape'])
    receipt = {'schema_version': 1, 'validator': 'python-jsonschema',
        'validator_version': importlib.metadata.version('jsonschema'),
        'draft': '2020-12', 'metaschema_valid': True,
        'valid_cases_accepted': len(cases['valid_shape']),
        'invalid_cases_rejected': len(cases['invalid_shape']),
        'schema_sha256': hashlib.sha256(schema_path.read_bytes()).hexdigest(),
        'response_schema_sha256': hashlib.sha256(response_path.read_bytes()).hexdigest(),
        'cases_sha256': hashlib.sha256(cases_path.read_bytes()).hexdigest(),
        'scope': 'request contract; actual Rust parser and worker have separate integration tests'}
    print(json.dumps(receipt, indent=2))


if __name__ == '__main__':
    main()
