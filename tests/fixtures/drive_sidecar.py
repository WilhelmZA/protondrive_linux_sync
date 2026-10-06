#!/usr/bin/env python3
"""Scripted Phase 0 protocol. All evidence stays beside this temporary copy."""
import json
import os
from pathlib import Path
import sys
import hashlib
import threading
import time
import uuid

home = Path(__file__).parent
output_lock = threading.Lock()
subscription = None
feed_lock = threading.Lock()
with (home / "starts").open("a") as out:
    out.write(f"{os.getpid()}\n")


def entry(name, kind="file", uid=None):
    return dict(name=name, type=kind, uid=uid or name, size=42,
                mtime=1700000000, sha1="abc", parent_uid=None)


def children(uid):
    if uid.endswith("empty"):
        return []
    if uid in ("nested", "excluded"):
        return [entry("child.txt", uid=uid + "-file")]
    if uid == "folder":
        return [entry("nested", "folder", "nested")]
    return [entry("remote.txt"), entry("folder", "folder"),
            entry("excluded", "folder")]


def send(value):
    with output_lock:
        print(json.dumps(dict(jsonrpc="2.0", **value)), flush=True)


def read_json(name, default):
    try:
        return json.loads((home / name).read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        return default


def poll_feed():
    global subscription
    while True:
        time.sleep(0.7)
        with feed_lock:
            if subscription is None:
                continue
            script = read_json('feed.json', [])
            cursor = subscription['cursor']
            ids = [item.get('last_event_id') for item in script]
            start = ids.index(cursor) + 1 if cursor in ids else 0
            for item in script[start:start + 1]:
                if item.get('signed_out'):
                    send(dict(method='auth.signed_out', params={}))
                    subscription = None
                    break
                if item.get('refresh') and item['last_event_id'] not in subscription['notices']:
                    subscription['notices'].add(item['last_event_id'])
                    send(dict(method='events.refresh_required', params=dict(scope_id='scope', reason=item['refresh'])))
                send(dict(method='events.batch', params=dict(scope_id='scope', events=item.get('events', []), last_event_id=item['last_event_id'])))
                subscription['delivered'].add(item['last_event_id'])
                if not subscription['ack_required']:
                    subscription['cursor'] = item['last_event_id']


threading.Thread(target=poll_feed, daemon=True).start()


class Failure(Exception):
    pass


def mutable(request):
    method, p, rid = request['method'], request['params'], request['id']
    tree_path = home / 'tree.json'
    tree = json.loads(tree_path.read_text())
    controls = json.loads((home / 'controls.json').read_text()) if (home / 'controls.json').exists() else {}
    def node(uid):
        if uid not in tree:
            raise Failure('not_found')
        return tree[uid]
    def listing(uid):
        node(uid)
        error = controls.get('list_errors', {}).get(uid)
        if error:
            raise Failure(error)
        return [dict(n, uid=k) for k, n in tree.items() if n.get('parent_uid') == uid] + controls.get('extra_entries', {}).get(uid, [])
    def lookup(parent, name):
        return next((k for k, n in tree.items() if n.get('parent_uid') == parent and n['name'] == name), None)
    writes = ('node.create_folder', 'file.upload', 'file.download', 'node.rename', 'node.move', 'node.trash')
    try:
        if method in writes:
            with (home / 'writes').open('a') as out:
                out.write(json.dumps(request) + '\n')
            if controls.get('write_error'):
                raise Failure(controls['write_error'])
        if method == 'node.resolve':
            if p['path'].endswith('/die'):
                os._exit(0)
            parts = p['path'].strip('/').split('/')
            uid = 'root'
            if parts.pop(0) != 'my-files':
                raise Failure('not_found')
            for name in parts:
                uid = lookup(uid, name)
                if uid is None:
                    raise Failure('not_found')
            result = dict(uid=uid, type=node(uid)['type'])
        elif method == 'node.list':
            result = dict(entries=listing(p['uid']))
        elif method == 'node.path':
            uid = p['uid']
            parts = []
            for _ in range(256):
                n = node(uid)
                if uid == 'root':
                    break
                parts.append(n['name'])
                uid = n['parent_uid']
            else:
                raise Failure('not_found')
            result = dict(path='/'.join(['/my-files', *reversed(parts)]))
        elif method == 'node.walk':
            queue = [(p['uid'], '')]
            failed = []
            while queue:
                uid, path = queue.pop(0)
                try:
                    for n in listing(uid):
                        send(dict(method='transfer.progress', params=dict(id=999, bytes=1, total=None)))
                        send(dict(method='unknown.notification', params={}))
                        send(dict(method='walk.entry', params=dict(uid=n['uid'], parent_uid=uid, entry=n)))
                        if n['type'] == 'folder':
                            queue.append((n['uid'], '/'.join(filter(None, (path, n['name'])))))
                except Failure as e:
                    if uid == p['uid'] or str(e) == 'auth':
                        raise
                    failed.append(path)
            result = dict(folders=1, failed=failed, failed_codes=['transient'] * len(failed))
        elif method == 'node.create_folder':
            node(p['parent_uid'])
            uid = lookup(p['parent_uid'], p['name'])
            if uid and node(uid)['type'] != 'folder':
                raise Failure('conflict')
            uid = uid or str(uuid.uuid4())
            tree[uid] = dict(name=p['name'], type='folder', parent_uid=p['parent_uid'], size=0, mtime=None, sha1=None)
            result = dict(uid=uid)
        elif method == 'file.upload':
            node(p['parent_uid'])
            uid = p.get('replace_uid') or lookup(p['parent_uid'], p['name'])
            if uid and node(uid)['type'] != 'file':
                raise Failure('conflict')
            uid = uid or str(uuid.uuid4())
            path = Path(p['local_path'])
            data = path.read_bytes()
            tree[uid] = dict(name=p['name'], type='file', parent_uid=p['parent_uid'], data=data.hex(), size=len(data), mtime=int(path.stat().st_mtime), sha1=hashlib.sha1(data).hexdigest())
            result = dict(uid=uid, revision_uid=str(uuid.uuid4()), sha1=tree[uid]['sha1'])
        elif method == 'file.download':
            n = node(p['uid'])
            def transfer():
                target = Path(p['local_path'])
                temp = target.parent / (str(uuid.uuid4()) + '.tmp')
                with (home / 'transfers').open('a') as out:
                    out.write(json.dumps(dict(event='start', id=rid)) + '\n')
                try:
                    data = bytes.fromhex(n['data'])
                    temp.write_bytes(data[:1])
                    time.sleep(controls.get('download_delay', 0))
                    if controls.get('download_error'):
                        raise Failure(controls['download_error'])
                    temp.write_bytes(data)
                    os.utime(temp, (n['mtime'], n['mtime']))
                    temp.replace(target)
                    send(dict(method='transfer.progress', params=dict(id=rid, bytes=len(data), total=len(data))))
                    send(dict(id=rid, result=dict(size=len(data), sha1=n['sha1'])))
                except Failure as e:
                    send(dict(id=rid, error=dict(code=-32000, message='scripted', data=dict(code=str(e)))))
                finally:
                    temp.unlink(missing_ok=True)
                    with (home / 'transfers').open('a') as out:
                        out.write(json.dumps(dict(event='done', id=rid)) + '\n')
            threading.Thread(target=transfer).start()
            return
        elif method in ('node.rename', 'node.move'):
            n = node(p['uid'])
            parent = p.get('new_parent_uid', n['parent_uid'])
            name = p.get('new_name', n['name'])
            node(parent)
            if lookup(parent, name) not in (None, p['uid']):
                raise Failure('conflict')
            n.update(name=name, parent_uid=parent)
            result = dict(ok=True)
        elif method == 'node.trash':
            pending = list(p['uids'])
            while pending:
                uid = pending.pop()
                node(uid)
                pending += [k for k, n in tree.items() if n.get('parent_uid') == uid]
                del tree[uid]
            result = dict(ok=True)
        else:
            raise Failure('fatal')
        if method in writes:
            tree_path.write_text(json.dumps(tree))
        send(dict(id=rid, result=result))
    except Failure as e:
        send(dict(id=rid, error=dict(code=-32000, message='scripted', data=dict(code=str(e), retry_after=controls.get('retry_after', 0)))))


for line in sys.stdin:
    request = json.loads(line)
    method, params, rid = request["method"], request["params"], request["id"]
    with (home / "calls").open("a") as out:
        out.write(json.dumps(request) + "\n")
    if method == 'events.subscribe':
        controls = read_json('controls.json', {})
        if controls.get('refuse_cursor') and params.get('since_event_id'):
            send(dict(id=rid, error=dict(code=-32000, data=dict(code='not_found'))))
            continue
        script = read_json('feed.json', [])
        cursor = params.get('since_event_id', script[-1]['last_event_id'] if script else '0')
        with feed_lock:
            subscription = dict(cursor=cursor, ack_required=params.get('ack_required', False), delivered={cursor}, notices=set())
        send(dict(id=rid, result=dict(scope_id='scope', last_event_id=cursor)))
        continue
    if method == 'events.ack':
        with feed_lock:
            if subscription is None or params['scope_id'] != 'scope' or params['event_id'] not in subscription['delivered']:
                send(dict(id=rid, error=dict(code=-32000, data=dict(code='fatal'))))
            else:
                subscription['cursor'] = params['event_id']
                with (home / 'acks').open('a') as out:
                    out.write(json.dumps(params) + '\n')
                send(dict(id=rid, result=dict(ok=True)))
        continue
    if (home / 'tree.json').exists() and method != 'auth.status':
        mutable(request)
        continue
    result = None
    code = None
    if method == "auth.status":
        result = dict(signed_in=read_json('controls.json', {}).get('signed_in', True), account=str(os.getpid()))
    elif method == "node.resolve":
        path = params["path"]
        leaf = path.rsplit("/", 1)[-1]
        if leaf == "die":
            os._exit(0)
        if leaf == "hold":
            with (home / "held").open("a") as out:
                out.write("held\n")
            continue
        if leaf in ("missing", "auth", "transient", "fatal", "conflict", "rate_limited"):
            code = "not_found" if leaf == "missing" else leaf
        else:
            result = dict(uid=leaf, type="folder")
    elif method == "node.list":
        if params["uid"] == "list-missing":
            code = "not_found"
        elif params["uid"] == "list-auth":
            code = "auth"
        else:
            result = dict(entries=children(params["uid"]))
    elif method == "node.walk":
        assert params["exclude_globs"] == []
        if params["uid"] == "walk-missing":
            code = "not_found"
        elif params["uid"] == "walk-auth":
            code = "auth"
        else:
            queue = [params["uid"]]
            folders = 0
            while queue:
                uid = queue.pop(0)
                folders += 1
                for child in children(uid):
                    send(dict(method="walk.entry", params=dict(uid=child["uid"], parent_uid=uid, entry=child)))
                    if child["type"] == "folder":
                        queue.append(child["uid"])
            result = dict(folders=folders, failed=["unreadable"] if params["uid"] == "partial" else [], failed_codes=["transient"] if params["uid"] == "partial" else [])
    else:
        raise AssertionError("write or unknown RPC: " + method)
    if code:
        send(dict(id=rid, error=dict(code=-32000, message="scripted", data=dict(code=code, retry_after=0))))
    else:
        send(dict(id=rid, result=result))
