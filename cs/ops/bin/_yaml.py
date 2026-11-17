"""A YAML file as Ruby's `JSON.generate(YAML.safe_load(File.read(path)))` read it, through
`cs-tools yaml-json` (byte for byte, refusals included), so no ops helper needs Ruby. Helper, not an op."""
import json
import os
from pathlib import Path
import subprocess

LAB = Path(__file__).resolve().parents[2]


def load(path, timeout=None):
    """The parsed value. ValueError carries the refusal (`<message> (<Ruby class>)`) on a nonzero exit;
    OSError if the binary is missing; TimeoutExpired past timeout."""
    binary = os.environ.get('CS_TOOLS_BIN') or str(LAB.parent / 'target/release/cs-tools')
    result = subprocess.run([binary, 'yaml-json', str(path)], capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        lines = result.stderr.strip().splitlines() or ['yaml-json exit %d' % result.returncode]
        raise ValueError(lines[-1].removeprefix('yaml-json: '))
    return json.loads(result.stdout)
