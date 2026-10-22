"""Read engine state and start one sync.

Point this at a neutronsync serve you already started. It does not launch the server.

    uv run python examples/sync_once.py --socket "$NEUTRONSYNC_SOCKET"
    uv run python examples/sync_once.py --base-url http://127.0.0.1:8090
"""

from __future__ import annotations

import argparse
import asyncio
import os

from neutronsync_client import ApiClient


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--base-url",
        default=os.environ.get("NEUTRONSYNC_API_URL"),
        help="API base URL. On a unix socket this is only the Host header.",
    )
    parser.add_argument(
        "--socket",
        default=os.environ.get("NEUTRONSYNC_SOCKET"),
        help="Unix socket path. Omit to use TCP at --base-url.",
    )
    parser.add_argument(
        "--token",
        default=os.environ.get("NEUTRONSYNC_API_TOKEN"),
        help="Bearer token. Defaults to NEUTRONSYNC_API_TOKEN.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="POST /api/v1/sync with dry_run set.",
    )
    args = parser.parse_args()
    if args.socket:
        base_url = args.base_url or "http://localhost"
    else:
        base_url = args.base_url or "http://127.0.0.1:8090"
    asyncio.run(_run(base_url, args.socket, args.token, dry_run=args.dry_run))


async def _run(base_url: str, socket_path: str | None, token: str | None, *, dry_run: bool) -> None:
    client = ApiClient(base_url, token=token, socket_path=socket_path)
    try:
        envelope = await client.state()
        print(f"rev {envelope.rev} pairs {len(envelope.state.pairs)} busy {envelope.state.busy}")
        await client.sync(dry_run=dry_run)
        print("sync started" if not dry_run else "dry run posted")
    finally:
        await client.aclose()


if __name__ == "__main__":
    main()
