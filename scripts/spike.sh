#!/usr/bin/env bash
set -euo pipefail
root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
exec python3 - "$root/sidecar/dist/neutronsync-drive" "$@" <<'PY'
import argparse
import getpass
import json
import queue
import subprocess
import sys
import threading
import time

parser = argparse.ArgumentParser(description="Read-only Phase 0 measurements. Credentials use the terminal and anonymous pipes only.")
parser.add_argument("binary")
parser.add_argument("--path", default="/my-files/Documents")
parser.add_argument("--username", help="Proton username (prompted when omitted)")
parser.add_argument("--password-cmd", help="command whose stdout is the Proton password, e.g. pass-cli item view ... --field password")
parser.add_argument("--totp-cmd", help="command whose stdout is the current 6-digit TOTP code")
parser.add_argument("--duration", type=float, default=86400, help="session observation seconds (default: 24 hours)")
args = parser.parse_args()
tty_in = open('/dev/tty', 'r')
terminal = open('/dev/tty', 'w')
def prompt(message):
    terminal.write(message)
    terminal.flush()
    return tty_in.readline().rstrip('\n')

process = subprocess.Popen([args.binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=None, text=True, bufsize=1)
messages = queue.Queue()
notifications = []
signouts = 0
scope = None
sequence = 0
walk_progress = {'entries': 0, 'folders': 0, 'shown': 0.0, 'start': 0.0}
def reader():
    try:
        for line in process.stdout:
            messages.put((time.monotonic(), json.loads(line)))
    finally:
        messages.put((time.monotonic(), None))
threading.Thread(target=reader, daemon=True).start()

def receive(timeout):
    global signouts, scope
    received, value = messages.get(timeout=timeout)
    if value is None:
        raise RuntimeError('Sidecar exited')
    if value.get('method') == 'auth.signed_out':
        signouts += 1
    if value.get('method') == 'events.batch':
        scope = value['params']['scope_id']
    return received, value

def rpc(method, params=None, timeout=3600):
    global sequence
    sequence += 1
    request_id = sequence
    process.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': request_id, 'method': method, 'params': params or {}}) + '\n')
    process.stdin.flush()
    deadline = time.monotonic() + timeout
    while True:
        received, value = receive(max(0.001, deadline - time.monotonic()))
        if value.get('id') == request_id:
            if 'error' in value:
                raise RuntimeError('RPC failed: ' + value['error'].get('data', {}).get('code', 'fatal'))
            return value['result']
        if value.get('method') == 'walk.entry':
            walk_progress['entries'] += 1
            if value['params']['entry']['type'] == 'folder':
                walk_progress['folders'] += 1
            if received - walk_progress['shown'] >= 5:
                walk_progress['shown'] = received
                terminal.write(f"  walking: {walk_progress['folders']} folders, {walk_progress['entries']} entries, {received - walk_progress['start']:.0f} s\n")
                terminal.flush()
        if value.get('method') == 'events.batch':
            notifications.append((received, value))

try:
    if not rpc('auth.status')['signed_in']:
        username = args.username or prompt('Proton username: ')
        if args.password_cmd:
            fetched = subprocess.run(args.password_cmd, shell=True, stdout=subprocess.PIPE, stderr=None, text=True)
            if fetched.returncode != 0:
                raise RuntimeError('--password-cmd failed')
            password = fetched.stdout.rstrip('\n')
            if not password:
                raise RuntimeError('--password-cmd returned nothing')
        else:
            password = getpass.getpass('Proton password: ', stream=terminal)
        result = rpc('auth.login', {'username': username, 'password': password})
        if result.get('need_human_verification'):
            terminal.write('Proton wants human verification. Open this link in a browser and complete it:\n  ' + result['url'] + '\n')
            pasted = prompt('Press Enter when done (or paste a token if the page shows one): ').strip()
            result = rpc('auth.login', {'username': username, 'password': password, 'hv_token': pasted or result['token'], 'hv_type': 'captcha'})
            if result.get('need_human_verification'):
                raise RuntimeError('Human verification was not accepted')
        if result.get('need_2fa'):
            if args.totp_cmd:
                fetched = subprocess.run(args.totp_cmd, shell=True, stdout=subprocess.PIPE, stderr=None, text=True)
                code = fetched.stdout.strip() if fetched.returncode == 0 else ''
                if not code:
                    raise RuntimeError('--totp-cmd failed')
            else:
                code = getpass.getpass('TOTP code: ', stream=terminal)
            result = rpc('auth.submit_2fa', {'code': code})
        if result.get('need_mailbox_password'):
            result = rpc('auth.submit_mailbox_password', {'password': getpass.getpass('Mailbox password: ', stream=terminal)})
        if not result.get('ok'):
            raise RuntimeError('Authentication did not finish')
    uid = rpc('node.resolve', {'path': args.path})['uid']
    print('| Check | Target | Measured | Notes |', flush=True)
    print('| --- | --- | --- | --- |', flush=True)
    for label, target in [('Cold walk', 'under 120 s'), ('Warm walk', 'under 30 s')]:
        start = time.monotonic()
        walk_progress.update(entries=0, folders=0, shown=start, start=start)
        result = rpc('node.walk', {'uid': uid, 'exclude_globs': []})
        elapsed = time.monotonic() - start
        print(f"| {label} | {target} | {elapsed:.3f} s | folders={result['folders']}; failed={len(result['failed'])} |", flush=True)
        if result['failed']:
            for path, code in zip(result['failed'], result.get('failed_codes', [])):
                terminal.write(f'  failed folder: {path or "(root)"} [{code}]\n')
            raise RuntimeError('Walk is partial; do not use its timing as a successful reading')
    rpc('events.subscribe', {'scope_id': '/my-files'})
    print('My-files SDK tree-event scope ID: ' + str(scope), flush=True)
    path = prompt('Existing file to edit in the web client (absolute /my-files path): ')
    event_uid = rpc('node.resolve', {'path': path})['uid']
    prompt('Prepare the edit. Press Enter immediately before you save it in the web client: ')
    start = time.monotonic()
    notifications.clear()
    while True:
        received, value = receive(max(0.001, 180 - (time.monotonic() - start)))
        if value.get('method') == 'events.batch' and received >= start:
            if any(event['node_uid'] == event_uid and event['type'] == 'node_updated' for event in value['params']['events']):
                print(f'| Feed latency | under 60 s | {received - start:.3f} s | Upper bound from pre-save Enter to matching node_updated |', flush=True)
                break
    start = time.monotonic()
    end = start + args.duration
    next_status = start
    checks = 0
    while time.monotonic() < end:
        now = time.monotonic()
        if now >= next_status:
            status = rpc('auth.status')
            checks += 1
            if not status['signed_in']:
                raise RuntimeError('Session signed out during stability check')
            next_status += 300
        try:
            receive(min(max(0.001, next_status - time.monotonic()), max(0.001, end - time.monotonic())))
        except queue.Empty:
            pass
        if signouts:
            raise RuntimeError('Session signed out during stability check')
    status = rpc('auth.status')
    if not status['signed_in']:
        raise RuntimeError('Session signed out during stability check')
    print(f'| Session stability | zero sign-outs over 24 h | {signouts} sign-outs | elapsed={time.monotonic() - start:.1f} s; status checks={checks + 1} |', flush=True)
except (RuntimeError, queue.Empty) as error:
    print('Spike stopped: ' + (str(error) or 'event or RPC timeout'), file=sys.stderr)
    sys.exit(1)
except KeyboardInterrupt:
    print('Spike stopped before all measurements finished', file=sys.stderr)
    sys.exit(130)
finally:
    process.stdin.close()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.terminate()
        process.wait(timeout=5)
PY
