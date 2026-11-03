"""Relay real kubectl exec, preserving guest bytes and removing its owned exit diagnostic."""

import os
import re
import signal
import subprocess
import sys

ACK = b"by-sandbox-started\n"
TAIL = 128
process = subprocess.Popen(sys.argv[1:], stderr=subprocess.PIPE)
started = False
previous = b""
tail = b""
while chunk := process.stderr.read1(65536):
    if not started:
        combined = previous + chunk
        acknowledgement = re.search(rb"(?m)^by-sandbox-started\n", combined)
        if acknowledgement is None:
            sys.stderr.buffer.write(chunk)
            sys.stderr.buffer.flush()
            previous = combined[-len(ACK) - 1 :]
            continue
        end = acknowledgement.end() - len(previous)
        sys.stderr.buffer.write(chunk[:end])
        sys.stderr.buffer.flush()
        chunk = chunk[end:]
        started = True
    tail += chunk
    if len(tail) > TAIL:
        sys.stderr.buffer.write(tail[:-TAIL])
        sys.stderr.buffer.flush()
        tail = tail[-TAIL:]
code = process.wait()
trailer = f"command terminated with exit code {code}\n".encode()
if started and code > 0 and tail.endswith(trailer):
    tail = tail[: -len(trailer)]
sys.stderr.buffer.write(tail)
sys.stderr.buffer.flush()
if code < 0:
    if -code not in (signal.SIGKILL, signal.SIGSTOP):
        signal.signal(-code, signal.SIG_DFL)
    os.kill(os.getpid(), -code)
sys.exit(code)
