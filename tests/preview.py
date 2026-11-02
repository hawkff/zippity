#!/usr/bin/env python3
"""Exercise automatic previews and ISO navigation through Yazi."""

import gzip
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import time
import zipfile

REPO = Path(__file__).resolve().parents[1]

with tempfile.TemporaryDirectory(prefix='zippity-preview-') as temporary:
    root = Path(temporary).resolve()
    config = root / 'config'
    for directory in (config, root / 'tmp', root / 'cache', root / 'state', root / 'out'):
        directory.mkdir()
    shutil.copytree(REPO / 'zippity.yazi', config / 'plugins/zippity.yazi')
    for name in ('keymap.toml', 'yazi.toml', 'vfs.toml'):
        shutil.copyfile(REPO / 'examples' / name, config / name)
    image = root / 'disc.ISO'
    image.write_bytes(gzip.decompress((REPO / 'tests/fixtures/iso9660.iso.gz').read_bytes()))
    archive = root / 'sample.zip'
    with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED) as output:
        output.writestr('a.txt', 'First automatic preview.\n')
        output.writestr('b.txt', 'Second automatic preview.\n')
        output.writestr('c.txt', 'Third automatic preview.\n')
        output.writestr('nested.iso', image.read_bytes())
        output.writestr('z-large.bin', b'Z' * (33 * 1024 * 1024))
    originals = {path:path.read_bytes() for path in (archive, image)}
    keymap = (config / 'keymap.toml').read_text()
    additions = '\n'.join(f'  {{ on = {json.dumps(key)}, run = {json.dumps(command)} }},' for key, command in (
        ('<F5>', 'reveal ' + shlex.quote(str(image))),
        ('<F6>', 'cd ' + shlex.quote(str(root / 'out'))),
    ))
    assert keymap.count('\n]\n') == 1
    (config / 'keymap.toml').write_text(keymap.replace('\n]\n', '\n' + additions + '\n]\n'))
    env = {**os.environ, 'PATH':str(REPO / 'target/debug') + os.pathsep + os.environ['PATH'],
        'YAZI_CONFIG_HOME':str(config), 'TMPDIR':str(root / 'tmp'),
        'XDG_CACHE_HOME':str(root / 'cache'), 'XDG_STATE_HOME':str(root / 'state')}
    socket = 'zippity-preview-' + str(os.getpid())

    def tmux(*args, check=True):
        return subprocess.run(['tmux','-L',socket,*args], env=env, text=True, capture_output=True, check=check, timeout=10)

    def screen():
        return tmux('capture-pane','-pt','smoke',check=False).stdout

    def keys(*values):
        tmux('send-keys','-t','smoke',*values)

    def wait(predicate, description):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(0.1)
        raise AssertionError(description + '\n' + screen())

    def cached_files():
        return [path for directory in (root / 'tmp').glob('zippity-*')
                for path in directory.iterdir() if path.is_file() and not path.name.startswith('.')]

    try:
        tmux('new-session','-d','-s','smoke','-x','140','-y','32',shlex.join(['yazi',str(archive)]))
        wait(lambda:'sample.zip' in screen(), 'Yazi did not start')
        keys('l')
        wait(lambda:'zippity://' in screen() and 'Press L' in screen(), 'Archive preview hint is missing')
        assert not cached_files()
        keys('L')
        wait(lambda:'First automatic preview.' in screen(), 'L did not preview the hovered member')
        assert len(cached_files()) == 1, 'Automatic mode unpacked unvisited members'
        keys('j')
        wait(lambda:'Second automatic preview.' in screen(), 'Moving the cursor did not preview the next member')
        assert len(cached_files()) == 2
        keys('G')
        wait(lambda:'Large member:' in screen(), 'Automatic mode did not enforce the size limit')
        assert 'Preview large archive member?' not in screen()
        assert len(cached_files()) == 2
        keys('L')
        wait(lambda:'Automatic previews disabled' in screen(), 'Toggle-off notification is missing')
        keys('g', 'g')
        wait(lambda:'a.txt' in screen().splitlines()[-1], 'First member was not selected')
        keys('j')
        wait(lambda:'b.txt' in screen().splitlines()[-1], 'Second member was not selected')
        keys('j')
        wait(lambda:'c.txt' in screen().splitlines()[-1] and 'Press L' in screen(), 'Disabling automatic previews failed')
        assert len(cached_files()) == 2
        keys('L')
        wait(lambda:'Third automatic preview.' in screen(), 'Re-enabling automatic previews failed')
        keys('j')
        wait(lambda:'nested.iso' in screen().splitlines()[-1], 'Nested image was not selected')
        keys('l')
        wait(lambda:'/nested.iso' in screen().splitlines()[0] and 'notes.txt' in screen(), 'Nested ISO did not open')
        keys('G')
        wait(lambda:'notes.txt' in screen().splitlines()[-1], 'Nested ISO member was not selected')
        keys('M-p')
        wait(lambda:'ISO archive preview.' in screen(), 'Nested ISO preview failed')
        keys('h')
        wait(lambda:'/nested.iso' not in screen().splitlines()[0], 'Leaving the nested ISO failed')
        keys('M-l')
        wait(lambda:'zippity://' not in screen().splitlines()[0], 'Lock did not leave the archive')
        assert not list((root / 'tmp').glob('zippity-*'))
        keys('l')
        wait(lambda:'zippity://' in screen() and 'Press L' in screen(), 'Lock did not reset automatic preview mode')
        assert not cached_files()
        keys('F5')
        wait(lambda:'disc.ISO' in screen().splitlines()[-1], 'Standalone ISO was not selected')
        keys('l')
        wait(lambda:'disc.ISO' in screen().splitlines()[0] and 'notes.txt' in screen(), 'Standalone ISO did not open')
        keys('G')
        wait(lambda:'notes.txt' in screen().splitlines()[-1], 'ISO member was not selected')
        keys('L')
        wait(lambda:'ISO archive preview.' in screen(), 'Standalone ISO automatic preview failed')
        keys('M-e')
        wait(lambda:'single-volume ZIP' in screen(), 'ISO editing was not refused')
        keys('y', 'F6')
        wait(lambda:screen().splitlines()[0].split()[0].endswith('/out'), 'Extraction destination did not open')
        keys('p')
        wait(lambda:(root / 'out/notes.txt').exists(), 'ISO extraction failed')
        assert (root / 'out/notes.txt').read_bytes() == b'ISO archive preview.\n'
        keys('q')
        wait(lambda:tmux('has-session','-t','smoke',check=False).returncode != 0, 'Quit failed')
        assert not list((root / 'tmp').glob('zippity-*'))
        assert all(path.read_bytes() == data for path, data in originals.items())
        print('Automatic previews, size guards, lock reset, nested ISO, and selected ISO extraction passed')
    finally:
        tmux('kill-server',check=False)
