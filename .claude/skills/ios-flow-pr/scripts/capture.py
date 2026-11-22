"""Loopback server the XCUITest calls to record clips and stills.

GET /start-NN-name  start recording NN-name.mp4 (stops any recording in progress)
GET /stop           stop and finalize the current recording
GET /dark           switch the simulator to dark appearance
GET /NN-name        save stills/NN-name.png
Usage: python3 capture.py <device-udid> <output-dir>
"""
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import re, signal, subprocess, sys, time

device, out = sys.argv[1], Path(sys.argv[2])
(out / 'stills').mkdir(parents=True, exist_ok=True)
io = ['xcrun', 'simctl', 'io', device]
recording = None

def stop():
    global recording
    if recording is not None:
        recording.send_signal(signal.SIGINT)
        recording.wait(timeout=15)
        recording = None

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        print(self.path, flush=True)

    def do_GET(self):
        global recording
        name = self.path[1:]
        try:
            if name.startswith('start-'):
                stop()
                recording = subprocess.Popen(io + ['recordVideo', '--codec=h264', '--force', str(out / (name[6:] + '.mp4'))],
                                             start_new_session=True)
                time.sleep(.65)
            elif name == 'stop':
                stop()
            elif name == 'dark':
                subprocess.run(['xcrun', 'simctl', 'ui', device, 'appearance', 'dark'], check=True)
            elif re.fullmatch(r'[0-9]{2}-[a-z0-9-]+', name):
                subprocess.run(io + ['screenshot', str(out / 'stills' / (name + '.png'))], check=True)
            else:
                self.send_error(404)
                return
            self.send_response(200)
            self.end_headers()
        except Exception as exc:
            self.send_error(500, str(exc))

try:
    HTTPServer(('127.0.0.1', 18745), Handler).serve_forever()
finally:
    stop()
