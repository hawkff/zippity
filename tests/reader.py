#!/usr/bin/env python3
import json
import os
import socket
from pathlib import Path
import subprocess
import tempfile
import zipfile

BINARY = Path(__file__).resolve().parents[1] / 'target/debug/zippity'

def call(request, good=True):
    result = subprocess.run([BINARY], input=json.dumps(request).encode(), capture_output=True, timeout=30)
    assert (result.returncode == 0) == good, result.stderr.decode()
    return result.stdout

with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    archive = root / 'large.zip'
    data = bytes(range(256)) * (160 * 1024)
    with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED) as out:
        out.writestr('large.bin', data)
    index = json.loads(call({'op':'list','archive':str(archive)}))
    service = subprocess.Popen([BINARY, '--serve'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        service.stdin.write(json.dumps({'archive':str(archive),'revision':index['source']['revision'],'password':None}).encode() + b'\n')
        service.stdin.flush()
        ready = json.loads(service.stdout.readline())
        for offset in range(0, len(data), 1024 * 1024):
            actual = call({'op':'read_service','socket':ready['socket'],'member':'large.bin',
                'revision':index['source']['revision'],'offset':offset,'length':1024*1024})
            assert actual == data[offset:offset+1024*1024]
        actual = call({'op':'read_service','socket':ready['socket'],'member':'large.bin',
            'revision':index['source']['revision'],'offset':15,'length':100})
        assert actual == data[15:115]
        previous_directory = os.getcwd()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stalled:
            stalled.settimeout(15)
            try:
                os.chdir(ready['root'])
                stalled.connect('reader.sock')
            finally:
                os.chdir(previous_directory)
            assert stalled.recv(1) == b'', 'Stalled client was not disconnected'
        assert service.poll() is None, 'One stalled client stopped the reader service'
        actual = call({'op':'read_service','socket':ready['socket'],'member':'large.bin',
            'revision':index['source']['revision'],'offset':115,'length':100})
        assert actual == data[115:215]
        archive.touch()
        call({'op':'read_service','socket':ready['socket'],'member':'large.bin',
            'revision':index['source']['revision'],'offset':115,'length':100}, good=False)
        service.stdin.close()
        service.wait(timeout=10)
        assert service.returncode == 0, service.stderr.read().decode()
        assert not Path(ready['root']).exists()
        print('Persistent reads, large members, seeking, revision conflicts, and lifecycle cleanup passed')
    except Exception:
        print('Reader process still running at the failure:', service.poll() is None)
        if service.poll() is None:
            service.terminate()
        service.wait(timeout=10)
        print('Reader process status:', service.returncode)
        print(service.stderr.read().decode())
        raise
    finally:
        if service.poll() is None:
            service.terminate()
            service.wait(timeout=10)
