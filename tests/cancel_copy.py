#!/usr/bin/env python3
"""Check cancellation cleanup with a deliberately stalled archive process."""

import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time


repo = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    archive = root / "example.zip"
    archive.write_bytes(b"PK\x03\x04fixture")
    backend = root / "7zz"
    backend.write_text(
        "#!/usr/bin/env python3\n"
        "import sys, time\n"
        "sys.stdin.buffer.read()\n"
        "if sys.argv[1] == 'l':\n"
        "    print('Path = payload.bin\\nSize = 1073741824\\nFolder = -\\n')\n"
        "else:\n"
        "    sys.stdout.buffer.write(b'partial')\n"
        "    sys.stdout.buffer.flush()\n"
        "    time.sleep(30)\n"
    )
    backend.chmod(0o700)
    env = {**os.environ, "PATH": str(root) + os.pathsep + os.environ["PATH"]}
    binary = repo / "target/debug/zippity"
    listing = subprocess.run(
        [binary], input=json.dumps({"op": "list", "archive": str(archive)}).encode(),
        capture_output=True, env=env, check=True,
    )
    revision = json.loads(listing.stdout)["source"]["revision"]
    destination = root / "out.bin"
    destination.write_bytes(b"keep this")
    process = subprocess.Popen(
        [binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        env=env, start_new_session=True,
    )
    try:
        request = {
            "op": "copy", "archive": str(archive), "member": "payload.bin",
            "revision": revision, "destination": str(destination), "overwrite": True,
        }
        process.stdin.write(json.dumps(request).encode())
        process.stdin.close()
        process.stdin = None
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if any(path.stat().st_size > 0 for path in root.glob(".zippity-*")):
                break
            time.sleep(0.05)
        else:
            raise AssertionError("Copy did not start")
        process.send_signal(signal.SIGTERM)
        _, error = process.communicate(timeout=5)
        assert process.returncode != 0
        assert json.loads(error)["kind"] == "cancelled"
        assert destination.read_bytes() == b"keep this"
        assert not list(root.glob(".zippity-*")), "Cancellation left a partial file"
        print("Cancellation preserved the destination and removed partial output")
    finally:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=5)
