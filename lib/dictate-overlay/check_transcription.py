"""Verify request language and vocabulary are forwarded as literal form values."""
import json
import os
from pathlib import Path
import subprocess
import tempfile

source = (Path(__file__).resolve().parents[2]/'bin/dictate').read_text()
start = source.index('transcribe_whisper() {')
function = source[start:source.index('\ntranscribe() {', start)]
with tempfile.TemporaryDirectory() as tmp:
    for language, prompt in (('auto', 'example-project, sub-agent'), ('en', ''),
                             ('', '@not-a-file;type=text/plain, $(literal) café')):
        env = {**os.environ, 'PROMPT':prompt, 'LANG_':language, 'SERVER':'unused',
               'LOG':str(Path(tmp,'log')), 'ARGS_FILE':str(Path(tmp,'args.json'))}
        harness = '''set -euo pipefail
curl() {
  python3 - "$@" <<'PYARGS'
import json,os,sys
from pathlib import Path
Path(os.environ['ARGS_FILE']).write_text(json.dumps(sys.argv[1:]))
print('{"text":"recognized text"}')
PYARGS
}
'''+function+'\ntranscribe_whisper fixture.wav\n'
        result = subprocess.check_output(['bash','-c',harness], env=env, text=True)
        assert result == 'recognized text'
        args = json.loads(Path(tmp,'args.json').read_text())
        form_values = [args[i + 1] for i, arg in enumerate(args[:-1]) if arg == '--form-string']
        assert 'prompt=' + prompt in form_values
        assert 'language=' + language in form_values
print('PASS: language and vocabulary forwarding, explicit empty overrides, literal multipart values')
