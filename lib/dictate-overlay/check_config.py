"""Configuration precedence and parsing checks; no capture or credentials needed."""
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[2]
source = (root/'bin/dictate').read_text()
loader = source[source.index('config_explicit='):source.index('# Two scratch locations')]
loader = re.sub(r'  config_reader=.*', '  config_reader="$TEST_READER"', loader)
with tempfile.TemporaryDirectory() as tmp:
    config = Path(tmp, 'config.toml')
    base = {k:v for k,v in os.environ.items() if not k.startswith('DICTATE_')}
    base.update(TEST_READER=str(root/'lib/dictate-overlay/config.py'), XDG_CONFIG_HOME=tmp)
    def run(extra=None):
        return subprocess.run(['bash', '-c', 'set -euo pipefail\n'+loader+'''
python3 - <<'PYOUT'
import json, os
print(json.dumps({k:v for k,v in os.environ.items() if k.startswith('DICTATE_')}))
PYOUT
'''], env={**base, **(extra or {})}, text=True, capture_output=True)
    assert run().returncode == 0, 'Absent default config should be optional'
    config.write_text('DICTATE_CHANNELS = 10\nDICTATE_STYLE = true\nDICTATE_SERVER = ""\nDICTATE_PROMPT = "$(touch SHOULD_NOT_EXIST) `literal` česky"\n')
    result = run({'DICTATE_CONFIG':str(config), 'DICTATE_CHANNELS':'2', 'DICTATE_PROMPT':''})
    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data['DICTATE_CHANNELS']=='2' and data['DICTATE_STYLE']=='1'
    assert data['DICTATE_PROMPT']=='' and data['DICTATE_SERVER']==''
    result = run({'DICTATE_CONFIG':str(config)})
    assert json.loads(result.stdout)['DICTATE_PROMPT']=='$(touch SHOULD_NOT_EXIST) `literal` česky'
    assert not Path('SHOULD_NOT_EXIST').exists()
    assert run({'DICTATE_CONFIG':''}).returncode == 0
    assert run({'DICTATE_CONFIG':str(config)+'-missing'}).returncode != 0
    for invalid in ('DICTATE_TYPO = 1', 'DICTATE_STYLE = [1]', 'DICTATE_PROMPT = "secret-unclosed', 'DICTATE_PROMPT = "line\\nline"'):
        config.write_text(invalid)
        result = run({'DICTATE_CONFIG':str(config)})
        assert result.returncode != 0
        assert 'secret-unclosed' not in result.stderr
print('PASS: optional/default/explicit paths; environment precedence; empty overrides; boolean values; literal text; validation')
