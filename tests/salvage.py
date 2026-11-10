#!/usr/bin/env python3
"""Check recovery from damaged streams against a scripted 7zz with 7-Zip's error behavior."""

import json
import os
from pathlib import Path
import subprocess
import tempfile

REPO = Path(__file__).resolve().parents[1]
BINARY = REPO / 'target/debug/zippity'

# Emits selected members in listing order, or every member when none is named. "crc"
# damage writes the full size with wrong bytes; "short" writes half and, in a 7z block, nothing more for the rest of the block
# (7zExtract.cpp, CFolderOutStream::FlushCorrupted), while RAR keeps decoding later
# members; "empty" writes nothing for that member alone; "memory" stops the whole run the
# way 7zz does when it cannot allocate a decoder.
FAKE = r'''#!/usr/bin/env python3
import json, os, sys, zlib
script = json.load(open(os.environ['FAKE_SCRIPT']))
with open(os.environ['FAKE_LOG'], 'a') as log:
    log.write(sys.argv[1] + '\n')
sys.stdin.buffer.read()
if sys.argv[1] == 'l':
    for member in script['members']:
        data = member['content'].encode()
        print(f"Path = {member['name']}\nSize = {len(data)}\nCRC = {zlib.crc32(data):08X}")
        for key, value in member.get('fields', {}).items():
            print(f'{key} = {value}')
        print()
    sys.exit(0)
selected = {argument[3:] for argument in sys.argv if argument.startswith('-i!')}
dead, failed = set(), []
for member in script['members']:
    data, group, damage = member['content'].encode(), member.get('group'), member.get('damage')
    chosen = not selected or member['name'] in selected
    if group is not None and group in dead:
        if chosen:
            failed.append(member['name'])
        continue
    if damage == 'memory':
        sys.stdout.buffer.flush()
        sys.stderr.write("\nERROR: Can't allocate required memory!\n")
        sys.exit(8)
    if damage == 'crc':
        data = data[:-1] + bytes([data[-1] ^ 1])
    elif damage == 'short':
        data = data[:len(data) // 2]
        if script['mode'] == '7z' and group is not None:
            dead.add(group)
    elif damage == 'empty':
        data = b''
    if chosen:
        sys.stdout.buffer.write(data)
        if damage is not None:
            failed.append(member['name'])
sys.stdout.buffer.flush()
for name in failed:
    sys.stderr.write(f'ERROR: Data Error : {name}\n')
if failed:
    sys.exit(2)
'''

MAGIC = {'7z': b'7z\xbc\xaf\x27\x1c', 'rar': b'Rar!\x1a\x07\x01\x00', 'zip': b'PK\x03\x04'}


def member(name, group=None, damage=None, fields=None):
    content = f'{name} member content, written for recovery checks.\n'
    return {'name': name, 'content': content, 'group': group, 'damage': damage, 'fields': fields or {}}


with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    fake = root / 'bin'
    fake.mkdir()
    (fake / '7zz').write_text(FAKE)
    (fake / '7zz').chmod(0o700)
    log = root / 'calls.log'

    def recover(mode, members, keep_damaged=False, failure=False):
        work = root / f'{mode}-{len(list(root.iterdir()))}'
        work.mkdir()
        archive = work / 'archive'
        archive.write_bytes(MAGIC[mode] + b'\0' * 32)
        script = work / 'script.json'
        script.write_text(json.dumps({'mode': mode, 'members': members}))
        env = {**os.environ, 'PATH': str(fake) + os.pathsep + os.environ['PATH'],
            'FAKE_SCRIPT': str(script), 'FAKE_LOG': str(log)}

        def call(request, failure=False):
            result = subprocess.run([BINARY], input=json.dumps(request).encode(), capture_output=True, timeout=60, env=env)
            assert (result.returncode != 0) == failure, result.stderr.decode()
            return json.loads((result.stderr if failure else result.stdout).splitlines()[-1])

        revision = call({'op':'list','archive':str(archive)})['source']['revision']
        log.write_text('')
        report = call({'op':'copy_many','archive':str(archive),'revision':revision,'keep_damaged':keep_damaged,
            'files':[{'member':m['name'],'destination':str(work / m['name'])} for m in reversed(members)]}, failure)
        passes = log.read_text().split().count('x')
        if failure:
            assert sorted(path.name for path in work.iterdir()) == ['archive', 'script.json']
            return report, passes, work
        for item in members:
            copied = work / item['name']
            if item['name'] in report['damaged'] or item['name'] in report['lost']:
                assert not copied.exists(), item['name']
            else:
                assert copied.read_text() == item['content'], item['name']
        return report, passes, work

    def sizes(members, names):
        return sum(len(m['content']) for m in members if m['name'] in names)

    # 7z: a decoder failure inside a block writes nothing more for that block, so with nothing
    # after the damage the rest of the block is lost without another pass.
    block = [member('a', 1, fields={'Block': 1}), member('b', 1, 'short', {'Block': 1}),
        member('c', 1, fields={'Block': 1})]
    report, passes, work = recover('7z', block)
    assert report == {'done': sizes(block, 'a'), 'damaged': ['b'], 'lost': ['c'], 'partial': []}, report
    assert passes == 1, passes
    report, passes, work = recover('7z', block, keep_damaged=True)
    assert report['partial'] == [str(work / 'b.damaged')] and passes == 2, (report, passes)
    partial = work / 'b.damaged'
    assert partial.read_text() == block[1]['content'][:len(block[1]['content']) // 2]
    assert partial.stat().st_mode & 0o777 == 0o600

    # With a later block in the same pass, its bytes fill the gap, so each affected block
    # gets one pass of its own: block 1 confirms the loss and block 2 decodes cleanly.
    blocks = block + [member('d', 2, fields={'Block': 2}), member('e', 2, fields={'Block': 2})]
    report, passes, _ = recover('7z', blocks)
    assert report == {'done': sizes(blocks, 'ade'), 'damaged': ['b'], 'lost': ['c'], 'partial': []}, report
    assert passes == 3, passes

    # RAR keeps decoding after damage in a solid run, so one more pass recovers what follows.
    rar = [member('a', 1, fields={'Solid': '-'}), member('b', 1, 'short', {'Solid': '+'}),
        member('c', 1, fields={'Solid': '+'}), member('d', 1, fields={'Solid': '+'})]
    report, passes, _ = recover('rar', rar)
    assert report == {'done': sizes(rar, 'acd'), 'damaged': ['b'], 'lost': [], 'partial': []}, report
    assert passes == 2, passes

    # Independent members: a member with no output shifts the rest, which one more pass reframes.
    zip_members = [member('a'), member('b', damage='empty'), member('c', damage='crc'), member('d')]
    report, passes, _ = recover('zip', zip_members)
    assert report == {'done': sizes(zip_members, 'ad'), 'damaged': ['b', 'c'], 'lost': [], 'partial': []}, report
    assert passes == 2, passes
    # Running out of memory fails the run, not a member, so the request fails after one pass.
    error, passes, _ = recover('zip', [member('a'), member('b', damage='memory'), member('c')], failure=True)
    assert error['kind'] == 'archive' and passes == 1, (error, passes)
    print('Damaged members are found by CRC, retried per stream, and reported without repeated decoding')
