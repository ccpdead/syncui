#!/usr/bin/env python3
"""Exercise static native and emulated agents without SSH or external data."""
import argparse, json, subprocess, tempfile
from pathlib import Path
p = argparse.ArgumentParser()
p.add_argument('agent')
p.add_argument('--emulator')
a = p.parse_args()
command = ([a.emulator] if a.emulator else []) + [a.agent, 'serve']
with tempfile.TemporaryDirectory(prefix='syncui-agent-test-') as directory:
    root = Path(directory)
    (root / '中文\nname').write_text('hello')
    def request(action, version=1):
        proc = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        proc.stdin.write(json.dumps(dict(version=version,id='smoke',root=directory,action=action))+'\n'); proc.stdin.flush()
        frames=[]
        while True:
            line=proc.stdout.readline()
            if not line: break
            frame=json.loads(line); frames.append(frame)
            assert frame['id']=='smoke' and frame['version']==1
            if frame['event'] in ('done','error'): break
        proc.stdin.close(); proc.wait(timeout=10)
        assert frames, proc.stderr.read()
        return frames
    frames=request({'op':'scan','ignore':[]})
    entries=dict(pair for f in frames if f['event']=='entries' for pair in f['data'])
    assert list(entries)==['中文\nname'] and frames[-1]['event']=='done'
    digest=request({'op':'hashes','files':entries})[-1]['data']['中文\nname']
    assert len(digest)==64
    read=request({'op':'read','path':'中文\nname'})[-1]['data']
    assert read['side']['content']=='hello'
    write=request({'op':'write','path':'中文\nname','content':'changed','expected':entries['中文\nname']})[-1]
    assert write['event']=='done'
    assert request({'op':'write','path':'中文\nname','content':'stale','expected':entries['中文\nname']})[-1]['event']=='error'
    assert request({'op':'delete','path':'中文\nname','expected':write['data']})[-1]['event']=='done'
    assert not (root/'中文\nname').exists()
    assert request({'op':'scan','ignore':[]},version=999)[-1]['event']=='error'
    print('PASS: scan/hash/read/write/stale-write/delete/version', a.agent, 'emulator='+str(a.emulator))
