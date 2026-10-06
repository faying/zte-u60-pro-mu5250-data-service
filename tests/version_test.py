#!/usr/bin/env python3
"""Verify embedded version identity independently of firmware/runtime files."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
BIN = Path(sys.argv.pop(1)).resolve()
EXPECTED_VERSION = json.loads((ROOT / "version.json").read_text())["datad"]["version"]
EXPECTED = {"name": "zwrt-datad", "version": EXPECTED_VERSION}
FIRMWARE = "BD_TESTMODEMV1.0.0B99"


class VersionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="datad-version-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        # A stale or malicious runtime manifest must not override the binary.
        (self.root / "version.json").write_text('{"datad":{"version":"99.99.99"}}')
        self.calls = self.root / "calls"
        fake = self.root / "ubus"
        fake.write_text(r"""#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
import time
with Path(os.environ['VERSION_TEST_CALLS']).open('a') as out:
    out.write('called\n')
# while this file exists the "modem" does not answer (ubusd hung at boot)
while Path(os.environ['VERSION_TEST_HOLD']).exists():
    time.sleep(0.05)
if 'get_zwrt_common_info' in sys.argv:
    print(json.dumps({'wa_inner_version':'BD_TESTMODEMV1.0.0B99', 'hardware_version':'MU5250_HW1.0'}))
else:
    print('{}')
""")
        fake.chmod(0o755)
        self.env = dict(os.environ, ZWRT_DATAD_UBUS_BIN=str(fake),
                        ZWRT_DATAD_UCI_BIN='/usr/bin/false',
                        ZWRT_DATAD_DIR=str(self.root / 'cloud'),
                        VERSION_TEST_CALLS=str(self.calls),
                        VERSION_TEST_HOLD=str(self.root / 'hold'),
                        ZWRT_DATAD_NEIGHBOR_DIR=str(self.root / 'capture'),
                        ZWRT_DATAD_NEIGHBOR_CONFIG=str(self.root / 'neighbor.json'))

    def test_cli_needs_no_device_or_runtime_files(self):
        for args in (['--version'], ['-V'],
                     ['--neighbor', '--auth-token-file', str(self.root / 'missing'), '--version']):
            result = subprocess.run([str(BIN), *args], cwd=self.root, env=self.env,
                                    text=True, capture_output=True, check=True, timeout=3)
            self.assertEqual(result.stdout, 'zwrt-datad ' + EXPECTED_VERSION + '\n')
            self.assertEqual(result.stderr, '')
        self.assertFalse(self.calls.exists())
        self.assertFalse((self.root / 'capture').exists())

    def test_once_keeps_firmware_separate(self):
        result = subprocess.run([str(BIN), '--once'], cwd=self.root, env=self.env,
                                capture_output=True, check=True, timeout=10)
        state = json.loads(result.stdout)
        self.assertEqual(state['datad'], EXPECTED)
        self.assertEqual(state['system']['sw_version'], FIRMWARE)
        self.assertFalse(state['neighbor']['collector_running'])

    def test_refuses_unauthenticated_non_loopback_listener(self):
        result = subprocess.run([str(BIN), '--bind', '0.0.0.0', '--port', '0'],
                                cwd=self.root, env=self.env, text=True,
                                capture_output=True, timeout=3)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('refusing unauthenticated non-loopback listener', result.stderr)

    def test_listens_and_reports_starting_before_first_round(self):
        """P1-3: the port answers while the first round is stuck; /healthz is 503 until it ends."""
        hold = self.root / 'hold'
        hold.write_text('')
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        proc = subprocess.Popen([str(BIN), '-i', '200', '-p', str(port)],
                                cwd=self.root, env=self.env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        def get(path, timeout=4):
            try:
                with urllib.request.urlopen(f'http://127.0.0.1:{port}{path}', timeout=timeout) as r:
                    return r.status, json.load(r)
            except urllib.error.HTTPError as error:
                return error.code, json.load(error)
        try:
            deadline = time.monotonic() + 8
            while True:
                try:
                    status, body = get('/healthz')
                    break
                except (OSError, urllib.error.URLError):
                    if time.monotonic() >= deadline or proc.poll() is not None:
                        self.fail('datad did not listen while the first round was stuck')
                    time.sleep(.05)
            self.assertEqual((status, body['status'], body['ok']), (503, 'starting', False))
            self.assertEqual(get('/version'), (200, EXPECTED))
            # a read waits for the first round (up to 10 s), then gets its data
            started = time.monotonic()
            threading.Timer(1, hold.unlink).start()
            status, state = get('/v2/state', timeout=12)
            self.assertEqual(status, 200)
            self.assertIn('live', state['blocks'])
            self.assertGreaterEqual(time.monotonic() - started, 0.9)
            status, body = get('/healthz')
            self.assertEqual((status, body['status'], body['ok']), (200, 'ok', True))
            self.assertIsInstance(body['exec_age_ms'], int)
        finally:
            hold.unlink(missing_ok=True)
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()

    def test_http_sse_and_lan_auth_share_binary_version(self):
        sockets = [socket.socket(), socket.socket()]
        try:
            for sock in sockets:
                sock.bind(('127.0.0.1', 0))
            local_port, lan_port = [sock.getsockname()[1] for sock in sockets]
        finally:
            for sock in sockets:
                sock.close()
        token = self.root / 'auth.token'
        token.write_text('fixture-version-token')
        proc = subprocess.Popen([str(BIN), '-i', '200', '-p', str(local_port),
                                 '--lan-bind', '127.0.0.1', '--lan-port', str(lan_port),
                                 '--auth-token-file', str(token)],
                                cwd=self.root, env=self.env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        def request(port, path, method='GET', auth=False):
            headers = {'Authorization': 'Bearer fixture-version-token'} if auth else {}
            req = urllib.request.Request(f'http://127.0.0.1:{port}{path}', method=method, headers=headers)
            try:
                return urllib.request.urlopen(req, timeout=4)
            except urllib.error.HTTPError as error:
                return error
        try:
            deadline = time.monotonic() + 8
            while True:
                try:
                    with request(local_port, '/version') as response:
                        self.assertEqual(response.status, 200)
                        self.assertEqual(json.load(response), EXPECTED)
                    break
                except (OSError, urllib.error.URLError):
                    if time.monotonic() >= deadline or proc.poll() is not None:
                        self.fail('datad did not start')
                    time.sleep(.05)
            with request(lan_port, '/version') as response:
                self.assertEqual(response.status, 401)
            with request(lan_port, '/version', auth=True) as response:
                self.assertEqual(response.status, 200)
                self.assertEqual(json.load(response), EXPECTED)
            with request(local_port, '/version', method='POST') as response:
                self.assertEqual(response.status, 405)
            with request(lan_port, '/version', method='POST', auth=True) as response:
                self.assertEqual(response.status, 405)
            for port, auth in ((local_port, False), (lan_port, True)):
                with request(port, '/v2/state', auth=auth) as response:
                    state = json.load(response)
                    self.assertEqual(state['blocks']['live']['data']['system']['sw_version'], FIRMWARE)
                with request(port, '/v2/events', auth=auth) as response:
                    self.assertEqual(response.status, 200)
                    parts = []
                    for _ in range(300):
                        line = response.readline().decode()
                        if line.startswith('data:'):
                            parts.append(line[5:].lstrip())
                        elif not line.strip() and parts:
                            break
                    state = json.loads(''.join(parts))
                    self.assertEqual(state['blocks']['live']['data']['system']['sw_version'], FIRMWARE)
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()


if __name__ == '__main__':
    unittest.main(verbosity=2)
