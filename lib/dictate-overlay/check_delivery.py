import json, os, subprocess, tempfile, time, statistics
from pathlib import Path
import sys
python=sys.executable
source=Path(__file__).resolve().parents[2]/'bin/dictate'
code=source.read_text()
shortcut=code[code.index('send_paste_key() {'):code.index('\ndeliver() {')]
clipboard=code[code.index('have_copyq() {'):code.index('# --- live preview')]
delivery_start=code.index('deliver() {')
delivery=code[delivery_start:code.index('\nhave_pw() {', delivery_start)]

with tempfile.TemporaryDirectory() as tmp:
 tmp=Path(tmp); processes=[]
 xvfb=subprocess.Popen(['Xvfb','-displayfd','1','-screen','0','1200x800x24'],stdout=subprocess.PIPE,stderr=subprocess.DEVNULL); processes.append(xvfb)
 env={**os.environ,'DISPLAY':':'+xvfb.stdout.readline().decode().strip(),'GDK_BACKEND':'x11','NO_AT_BRIDGE':'1'}
 def xd(*args): return subprocess.run(['xdotool',*map(str,args)],env=env,text=True,capture_output=True,check=True).stdout.strip()
 def until(fn, seconds=3):
  end=time.monotonic()+seconds
  while time.monotonic()<end:
   try:result=fn()
   except (OSError,ValueError):result=None
   if result:return result
   time.sleep(.005)
  raise RuntimeError('timeout')
 target=tmp/'target.py'
 target.write_text('''import gi,json,sys,time
from pathlib import Path
gi.require_version('Gtk','3.0')
from gi.repository import Gtk,Gdk,GLib
root=Path(sys.argv[1]); current=None
w=Gtk.Window(title='Paste latency sentinel'); entry=Gtk.Entry(); w.add(entry); w.show_all(); entry.grab_focus()
def changed(*_):
 (root/'result.json').write_text(json.dumps(dict(text=entry.get_text(),time=time.monotonic())))
entry.connect('changed',changed)
def key(_,event):
 if event.keyval in (Gdk.KEY_v,Gdk.KEY_V) and event.state&Gdk.ModifierType.CONTROL_MASK:
  (root/'key.json').write_text(json.dumps(dict(mask=int(event.state))))
  entry.paste_clipboard(); return True
 return False
entry.connect('key-press-event',key)
def tick():
 global current
 try:
  data=json.loads((root/'control.json').read_text())
  if data['id']!=current:
   current=data['id']; entry.set_text('')
   (root/'ack').write_text(str(current))
   time.sleep(data['busy']/1000)
 except (OSError,ValueError):pass
 return True
GLib.timeout_add(5,tick)
Gtk.main()
''')
 try:
  targetproc=subprocess.Popen([python,str(target),str(tmp)],env=env);processes.append(targetproc)
  wid=until(lambda: subprocess.run(['xdotool','search','--name','Paste latency sentinel'],env=env,capture_output=True,text=True).stdout.strip())
  xd('windowfocus',wid)
  trial=0
  for mode in ('0', '0.012'):
   durations=[]; failed=0; mismatches=0; owners=[]
   for busy in (0,150):
    for held in ('','shift','ctrl'):
     for combo in ('ctrl+v','ctrl+shift+v'):
      trial+=1; text=f'Example {trial}: Příliš žluťoučký kůň & punctuation.'
      owner=subprocess.Popen(['xclip','-selection','clipboard','-in','-quiet'],env=env,stdin=subprocess.PIPE,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL);owners.append(owner)
      owner.stdin.write(text.encode());owner.stdin.close()
      until(lambda: subprocess.run(['xclip','-selection','clipboard','-o'],env=env,capture_output=True,text=True).stdout==text)
      (tmp/'control.json').write_text(json.dumps(dict(id=trial,busy=busy)))
      until(lambda: (tmp/'ack').exists() and (tmp/'ack').read_text()==str(trial))
      if held:xd('keydown',held)
      start=time.monotonic()
      subprocess.run(['bash','-c', 'set -euo pipefail\nrelease_mark() { :; }\n'+shortcut+'\nsend_paste_key "$TEST_COMBO"'],
                     env={**env,'PASTE_KEY_DELAY':mode,'TEST_COMBO':combo},check=True)
      try:
       result=until(lambda: json.loads((tmp/'result.json').read_text()) if (tmp/'result.json').exists() and json.loads((tmp/'result.json').read_text())['text']==text else None,.8)
       durations.append((result['time']-start)*1000)
       mask=json.loads((tmp/'key.json').read_text())['mask']
       mismatches+=bool(mask&1) != ('shift' in combo)
      except RuntimeError:failed+=1
      finally:
       xd('keyup','ctrl','shift','alt')
   for owner in owners:
    if owner.poll() is None:owner.terminate();owner.wait()
   assert failed == 0 and mismatches == 0
   print(mode, 'success',len(durations),'failures',failed,'wrong_shift_state',mismatches,'median_ms',round(statistics.median(durations),1) if durations else None,'max_ms',round(max(durations),1) if durations else None,flush=True)
  # Full delivery with the actual xclip fallback, isolated from the real CopyQ.
  for replace_clipboard, receiver_busy in ((False, 0), (True, 0), (False, 350)):
   trial+=1
   owner=subprocess.Popen(['xclip','-selection','clipboard','-in','-quiet'],env=env,stdin=subprocess.PIPE,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
   processes.append(owner);owner.stdin.write(b'previous clipboard');owner.stdin.close()
   until(lambda: subprocess.run(['xclip','-selection','clipboard','-o'],env=env,capture_output=True,text=True).stdout=='previous clipboard')
   (tmp/'control.json').write_text(json.dumps(dict(id=trial,busy=receiver_busy)))
   until(lambda:(tmp/'ack').read_text()==str(trial))
   text='Delivery example: česky & literal punctuation.'
   fixture = 'set -euo pipefail\nrelease_mark() { :; }\n'+clipboard+shortcut+delivery+\
     '\nhave_copyq() { echo probe >>"$TEST_PROBES"; return 1; }\ndeliver "$TEST_TEXT"\n'
   probes=tmp/f'probes-{trial}'
   sender=subprocess.Popen(['bash','-c',fixture],env={**env,'TEST_PROBES':str(probes),'TEST_TEXT':text,
       'INPUT_MODE':'paste','CLIP_MODE':'restore','PASTE_SETTLE':'0.4','PASTE_KEY_DELAY':'0.012','LOG':str(tmp/'log')},
       stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
   processes.append(sender)
   until(lambda:(tmp/'result.json').exists() and json.loads((tmp/'result.json').read_text())['text']==text)
   sender.wait(timeout=3); assert sender.returncode==0
   expected='previous clipboard'
   if replace_clipboard:
    expected='new clipboard copied by user'
    owner=subprocess.Popen(['xclip','-selection','clipboard','-in','-quiet'],env=env,stdin=subprocess.PIPE,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    processes.append(owner);owner.stdin.write(expected.encode());owner.stdin.close()
   time.sleep(.5)
   actual=subprocess.check_output(['xclip','-selection','clipboard','-o'],env=env,text=True)
   assert actual==expected, 'Clipboard restoration clobbered newer text or failed to restore'
   assert len(probes.read_text().splitlines())==1, 'Backend should be probed once'
  print('PASS: full clipboard delivery, one backend probe, restoration, newer clipboard preserved',flush=True)

  for spacing in ('0', '0.012'):
   result_path=tmp/f'terminal-{spacing}'
   terminal=subprocess.Popen(['xterm','-u8','-xrm','XTerm*VT100.translations: #override Ctrl Shift <Key>V: insert-selection(CLIPBOARD)','-title','Terminal paste sentinel','-e','bash','-c',
       'printf ready >"$TEST_READY"; IFS= read -r value; printf "%s" "$value" >"$TEST_RESULT"'],env={**env,'TEST_RESULT':str(result_path),'TEST_READY':str(result_path)+'.ready'},
       stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
   processes.append(terminal)
   terminal_id=until(lambda:subprocess.run(['xdotool','search','--name','Terminal paste sentinel'],env=env,capture_output=True,text=True).stdout.strip())
   until(lambda:Path(str(result_path)+'.ready').exists())
   xd('windowfocus',terminal_id)
   payload='Terminal example: žluťoučký kůň & literal symbols.'
   owner=subprocess.Popen(['xclip','-selection','clipboard','-in','-quiet'],env=env,stdin=subprocess.PIPE,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
   processes.append(owner);owner.stdin.write(payload.encode());owner.stdin.close()
   until(lambda:subprocess.run(['xclip','-selection','clipboard','-o'],env=env,capture_output=True,text=True).stdout==payload)
   subprocess.run(['bash','-c','set -euo pipefail\nrelease_mark() { :; }\n'+shortcut+'\nsend_paste_key ctrl+shift+v'],
                  env={**env,'PASTE_KEY_DELAY':spacing},check=True)
   time.sleep(.1)
   xd('key','Return')
   until(result_path.exists)
   assert result_path.read_text()==payload, 'Terminal paste mismatch'
   terminal.wait(timeout=2)
  print('PASS: xterm with configured Ctrl+Shift+V binding, 0 and 12 ms spacing',flush=True)

 finally:
  for process in reversed(processes):
   if process.poll() is None:process.terminate();process.wait()
