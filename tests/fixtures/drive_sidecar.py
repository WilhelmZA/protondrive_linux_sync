#!/usr/bin/env python3
"""Scripted Phase 0 protocol. All evidence stays beside this temporary copy."""
import json
import os
from pathlib import Path
import sys

home = Path(__file__).parent
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
    print(json.dumps(dict(jsonrpc="2.0", **value)), flush=True)


for line in sys.stdin:
    request = json.loads(line)
    method, params, rid = request["method"], request["params"], request["id"]
    with (home / "calls").open("a") as out:
        out.write(json.dumps(request) + "\n")
    result = None
    code = None
    if method == "auth.status":
        result = dict(signed_in=True, account=str(os.getpid()))
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
