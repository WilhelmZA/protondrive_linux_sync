#!/usr/bin/env bash
# Scripted container checks for AC-3..AC-10. Uses scratch volumes under /tmp,
# publishes ports on 127.0.0.1 only, and removes only its own containers/images.
set -euo pipefail

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
IMAGE="${NEUTRONSYNC_TEST_IMAGE:-neutronsync:dev}"
PREFIX="neutronsync-test-$$"
SCRATCH="$(mktemp -d /tmp/${PREFIX}-XXXXXX)"
CFG="${SCRATCH}/config"
DATA="${SCRATCH}/data"
mkdir -p "$CFG" "$DATA"
# Container user (PUID) must be able to write /data (Unraid shares are too).
chmod 777 "$DATA"
WEB_PASSWORD='test-password-12chars'
PUID=1234
PGID=1234
PORT_WEB=""
CID=""
CID_NOPW=""
CID_API=""
CID_FAKE=""
CID_USER=""
FAKE_IMAGE=""

cleanup() {
	set +e
	for id in "$CID" "$CID_NOPW" "$CID_API" "$CID_FAKE" "$CID_USER"; do
		[ -n "$id" ] || continue
		docker rm -f "$id" >/dev/null 2>&1
	done
	if [ -n "$FAKE_IMAGE" ]; then
		docker rmi "$FAKE_IMAGE" >/dev/null 2>&1
	fi
	# Files under scratch may be owned by PUID from the container.
	docker run --rm -v "${SCRATCH}:/scratch" alpine \
		chown -R "$(id -u):$(id -g)" /scratch >/dev/null 2>&1 || true
	rm -rf "$SCRATCH"
}
trap cleanup EXIT

log() { printf 'test: %s\n' "$*"; }
die() { printf 'test: FAIL: %s\n' "$*" >&2; exit 1; }

wait_healthy() {
	local id="$1" timeout="${2:-90}" status=""
	local deadline=$((SECONDS + timeout))
	while [ "$SECONDS" -lt "$deadline" ]; do
		status="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$id" 2>/dev/null || echo missing)"
		if [ "$status" = "healthy" ]; then
			echo "$status"
			return 0
		fi
		sleep 2
	done
	docker logs "$id" 2>&1 | tail -100 || true
	docker inspect "$id" || true
	die "container $id not healthy within ${timeout}s (last status=${status})"
}

host_port() {
	local id="$1" container_port="$2"
	docker inspect -f "{{(index (index .NetworkSettings.Ports \"${container_port}/tcp\") 0).HostPort}}" "$id"
}

api_in() {
	local id="$1" method="$2" path="$3" body="${4:-}"
	docker exec -i -e METHOD="$method" -e APATH="$path" -e BODY="$body" "$id" python3 - <<'PY'
import os, sys, urllib.error, urllib.request
method = os.environ["METHOD"]
path = os.environ["APATH"]
body = os.environ.get("BODY") or None
token = open("/config/secrets/api.token").read().strip()
data = None if not body else body.encode()
req = urllib.request.Request(
    "http://127.0.0.1:8090" + path,
    data=data,
    method=method,
    headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"},
)
try:
    with urllib.request.urlopen(req, timeout=60) as r:
        sys.stdout.write(r.read().decode())
except urllib.error.HTTPError as e:
    sys.stderr.write(e.read().decode() + "\n")
    sys.exit(1)
PY
}

# ----- AC-1 smoke -----
log "AC-1 version, binaries, size"
ver="$(docker run --rm --entrypoint neutronsync "$IMAGE" --version)"
printf '  version: %s\n' "$ver"
echo "$ver" | grep -qE '[0-9]+\.[0-9]+\.[0-9]+' || die "version missing crate version"
docker run --rm --entrypoint sh "$IMAGE" -c 'command -v neutronsync-drive >/dev/null && command -v neutronsync-web >/dev/null' \
	|| die "neutronsync-drive or neutronsync-web missing"
if docker run --rm --entrypoint sh "$IMAGE" -c 'command -v gcc >/dev/null || command -v cargo >/dev/null || command -v bun >/dev/null'; then
	die "gcc/cargo/bun must not be in the final image"
fi
size_bytes="$(docker image inspect -f '{{.Size}}' "$IMAGE")"
size_mb=$((size_bytes / 1024 / 1024))
printf '  image size: %s MB (limit 350)\n' "$size_mb"
[ "$size_mb" -lt 350 ] || die "image is ${size_mb} MB, over 350"

# ----- AC-3 first start -----
log "AC-3 first start with PUID/PGID"
CID="$(docker run -d --name "${PREFIX}-main" \
	-e "PUID=${PUID}" -e "PGID=${PGID}" \
	-e "NEUTRONSYNC_WEB_PASSWORD=${WEB_PASSWORD}" \
	-v "${CFG}:/config" -v "${DATA}:/data" \
	-p 127.0.0.1::8080 \
	"$IMAGE")"
wait_healthy "$CID" 90
PORT_WEB="$(host_port "$CID" 8080)"
log "  mapped web port ${PORT_WEB}"

[ -f "${CFG}/neutronsync.toml" ] || die "config missing"
diff -u "${ROOT}/docker/neutronsync.toml" "${CFG}/neutronsync.toml" || die "config differs from starter"
[ -f "${CFG}/secrets/api.token" ] || die "api.token missing"
[ -f "${CFG}/secrets/session.key" ] || die "session.key missing"
tok_meta="$(docker exec "$CID" stat -c '%a %u %g' /config/secrets/api.token)"
key_meta="$(docker exec "$CID" stat -c '%a %u %g' /config/secrets/session.key)"
echo "$tok_meta" | grep -q "^600 ${PUID} ${PGID}$" || die "api.token mode/owner wrong: ${tok_meta}"
echo "$key_meta" | grep -q "^600 ${PUID} ${PGID}$" || die "session.key mode/owner wrong: ${key_meta}"

# python:slim has no ps; read UIDs from /proc.
uids="$(docker exec -i "$CID" python3 - <<'PY'
import os
for name in os.listdir("/proc"):
    if not name.isdigit():
        continue
    try:
        uid = os.stat(f"/proc/{name}").st_uid
        cmd = open(f"/proc/{name}/cmdline", "rb").read().replace(b"\0", b" ").decode("utf-8", "replace")
    except OSError:
        continue
    if "neutronsync" in cmd:
        print(f"{uid} {cmd}")
PY
)"
printf '  procs:\n%s\n' "$uids"
echo "$uids" | grep -qE "^${PUID} " || die "expected serve/web running as uid ${PUID}"

TOKEN_SUM="$(docker exec "$CID" sha256sum /config/secrets/api.token | awk '{print $1}')"
KEY_SUM="$(docker exec "$CID" sha256sum /config/secrets/session.key | awk '{print $1}')"
CFG_SUM="$(docker exec "$CID" sha256sum /config/neutronsync.toml | awk '{print $1}')"

# ----- AC-10 -----
log "AC-10 full_walk_interval"
grep -q 'full_walk_interval = 21600' "${CFG}/neutronsync.toml" || die "full_walk_interval not 21600"
grep -qE 'six hours|6-hour|6 hours|21600' "${ROOT}/README.md" || die "README missing Unraid full-walk note"

# ----- AC-7 login page; API not on host -----
log "AC-7 login page and API isolation"
code="$(curl -sS -o /tmp/${PREFIX}-login.html -w '%{http_code}' "http://127.0.0.1:${PORT_WEB}/login")"
[ "$code" = "200" ] || die "GET /login returned ${code}"
grep -qiE 'password|sign in|login' /tmp/${PREFIX}-login.html || die "login page content unexpected"
if docker port "$CID" | grep -q 8090; then
	die "8090 is published on the host"
fi

docker rm -f "$CID" >/dev/null
CID=""

# Expose API: unauthenticated -> 401
CFG_API="${SCRATCH}/config-api"
DATA_API="${SCRATCH}/data-api"
mkdir -p "$CFG_API" "$DATA_API"
chmod 777 "$DATA_API"
CID_API="$(docker run -d --name "${PREFIX}-api" \
	-e "PUID=${PUID}" -e "PGID=${PGID}" \
	-e "NEUTRONSYNC_WEB_PASSWORD=${WEB_PASSWORD}" \
	-e "NEUTRONSYNC_API_LISTEN=0.0.0.0:8090" \
	-v "${CFG_API}:/config" -v "${DATA_API}:/data" \
	-p 127.0.0.1::8080 -p 127.0.0.1::8090 \
	"$IMAGE")"
wait_healthy "$CID_API" 90
PORT_API="$(host_port "$CID_API" 8090)"
unauth="$(curl -sS -o /tmp/${PREFIX}-unauth.json -w '%{http_code}' "http://127.0.0.1:${PORT_API}/api/v1/state" || true)"
[ "$unauth" = "401" ] || die "expected 401 without token, got ${unauth}"
docker rm -f "$CID_API" >/dev/null
CID_API=""

# Missing token file + API_LISTEN: serve refuses (point token file at a missing path)
CFG_NOTOKEN="${SCRATCH}/config-notoken"
mkdir -p "${CFG_NOTOKEN}"
CID_API="$(docker run -d --name "${PREFIX}-notoken" \
	-e "PUID=${PUID}" -e "PGID=${PGID}" \
	-e "NEUTRONSYNC_WEB_PASSWORD=${WEB_PASSWORD}" \
	-e "NEUTRONSYNC_API_LISTEN=0.0.0.0:8090" \
	-e "NEUTRONSYNC_API_TOKEN_FILE=/config/secrets/missing.token" \
	-v "${CFG_NOTOKEN}:/config" -v "${DATA_API}:/data" \
	"$IMAGE")"
sleep 12
logs="$(docker logs "$CID_API" 2>&1 || true)"
echo "$logs" | grep -qiE 'token|refuse|missing' || die "serve did not refuse missing token with API_LISTEN"
docker rm -f "$CID_API" >/dev/null
CID_API=""

# ----- AC-4 second start -----
log "AC-4 second start preserves secrets"
CID="$(docker run -d --name "${PREFIX}-main2" \
	-e "PUID=${PUID}" -e "PGID=${PGID}" \
	-e "NEUTRONSYNC_WEB_PASSWORD=${WEB_PASSWORD}" \
	-v "${CFG}:/config" -v "${DATA}:/data" \
	-p 127.0.0.1::8080 \
	"$IMAGE")"
wait_healthy "$CID" 90
PORT_WEB="$(host_port "$CID" 8080)"
[ "$(docker exec "$CID" sha256sum /config/secrets/api.token | awk '{print $1}')" = "$TOKEN_SUM" ] || die "api.token changed on second start"
[ "$(docker exec "$CID" sha256sum /config/secrets/session.key | awk '{print $1}')" = "$KEY_SUM" ] || die "session.key changed on second start"
[ "$(docker exec "$CID" sha256sum /config/neutronsync.toml | awk '{print $1}')" = "$CFG_SUM" ] || die "config changed on second start"

# ----- AC-6 clean stop -----
log "AC-6 docker stop within 20s, serve.lock gone"
t0="$SECONDS"
docker stop -t 20 "$CID" >/dev/null
elapsed=$((SECONDS - t0))
[ "$elapsed" -le 20 ] || die "docker stop took ${elapsed}s"
docker rm -f "$CID" >/dev/null
CID=""
if [ -e "${CFG}/state/neutronsync/serve.lock" ]; then
	die "serve.lock still present after stop"
fi

# ----- AC-5 missing password -----
log "AC-5 missing NEUTRONSYNC_WEB_PASSWORD"
CFG_NOPW="${SCRATCH}/config-nopw"
DATA_NOPW="${SCRATCH}/data-nopw"
mkdir -p "$CFG_NOPW" "$DATA_NOPW"
chmod 777 "$DATA_NOPW"
CID_NOPW="$(docker run -d --name "${PREFIX}-nopw" \
	-e "PUID=${PUID}" -e "PGID=${PGID}" \
	-v "${CFG_NOPW}:/config" -v "${DATA_NOPW}:/data" \
	-p 127.0.0.1::8080 \
	"$IMAGE")"
sleep 15
logs="$(docker logs "$CID_NOPW" 2>&1 || true)"
echo "$logs" | grep -q 'Set NEUTRONSYNC_WEB_PASSWORD' || die "missing password instruction not in logs"
status="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$CID_NOPW")"
[ "$status" != "healthy" ] || die "container became healthy without password"
printf '  health=%s\n' "$status"
docker rm -f "$CID_NOPW" >/dev/null
CID_NOPW=""

# ----- D3 --user (non-root) -----
log "D3 --user mode starts healthy"
CFG_USER="${SCRATCH}/config-user"
DATA_USER="${SCRATCH}/data-user"
mkdir -p "$CFG_USER" "$DATA_USER"
chmod 777 "$CFG_USER" "$DATA_USER"
HOST_UID="$(id -u)"
HOST_GID="$(id -g)"
CID_USER="$(docker run -d --name "${PREFIX}-user" \
	--user "${HOST_UID}:${HOST_GID}" \
	-e "NEUTRONSYNC_WEB_PASSWORD=${WEB_PASSWORD}" \
	-v "${CFG_USER}:/config" -v "${DATA_USER}:/data" \
	-p 127.0.0.1::8080 \
	"$IMAGE")"
wait_healthy "$CID_USER" 120
docker rm -f "$CID_USER" >/dev/null
CID_USER=""

# ----- AC-8 / AC-9 through the web UI + fake remote delete -----
log "AC-8/AC-9 web UI + fake sidecar remote delete"
FAKE_IMAGE="${PREFIX}-fakeimg"
CFG_FAKE="${SCRATCH}/config-fake"
DATA_FAKE="${SCRATCH}/data-fake"
FAKE_HOME="${SCRATCH}/fake-sidecar"
mkdir -p "$CFG_FAKE" "$DATA_FAKE/photos" "$FAKE_HOME"
chmod 777 "$DATA_FAKE" "$DATA_FAKE/photos" "$FAKE_HOME"
cp "${ROOT}/tests/fixtures/drive_sidecar.py" "${FAKE_HOME}/drive_sidecar.py"
chmod 755 "${FAKE_HOME}/drive_sidecar.py"
printf '%s\n' '{"signed_in": false}' >"${FAKE_HOME}/controls.json"
# Mutable tree so folder browse + remote delete work.
python3 - <<PY
import json
from pathlib import Path
home = Path("${FAKE_HOME}")
home.joinpath("tree.json").write_text(json.dumps({
    "root": {"name": "my-files", "type": "folder", "parent_uid": None},
    "photos": {"name": "photos", "type": "folder", "parent_uid": "root"},
}))
PY

docker build -t "$FAKE_IMAGE" -f - "$ROOT" <<'DOCKER'
FROM neutronsync:dev
RUN mkdir -p /opt/fake-sidecar \
	&& printf '%s\n' '#!/bin/sh' 'cd /opt/fake-sidecar && exec python3 drive_sidecar.py "$@"' \
		>/usr/local/bin/neutronsync-drive \
	&& chmod 755 /usr/local/bin/neutronsync-drive
DOCKER

CID_FAKE="$(docker run -d --name "${PREFIX}-fake" \
	-e "PUID=${PUID}" -e "PGID=${PGID}" \
	-e "NEUTRONSYNC_WEB_PASSWORD=${WEB_PASSWORD}" \
	-e "NEUTRONSYNC_DRIVE_STDIO=1" \
	-e "NEUTRONSYNC_FAKE_AUTH=totp" \
	-v "${CFG_FAKE}:/config" \
	-v "${DATA_FAKE}:/data" \
	-v "${FAKE_HOME}:/opt/fake-sidecar" \
	-p 127.0.0.1::8080 \
	"$FAKE_IMAGE")"
wait_healthy "$CID_FAKE" 120
PORT_WEB="$(host_port "$CID_FAKE" 8080)"
# Disable auto sync so Playwright drives dry-run / sync now explicitly.
docker exec -u root "$CID_FAKE" sh -c \
	"sed -i 's/auto_sync = true/auto_sync = false/' /config/neutronsync.toml; chown -R ${PUID}:${PGID} /opt/fake-sidecar /config"
# Restart serve so it reloads config (s6 finish+start).
docker restart "$CID_FAKE" >/dev/null
wait_healthy "$CID_FAKE" 120
PORT_WEB="$(host_port "$CID_FAKE" 8080)"

PYWEB="${ROOT}/web/.venv/bin/python"
[ -x "$PYWEB" ] || die "web/.venv missing; need Playwright for AC-8/AC-9"
export WEB_URL="http://127.0.0.1:${PORT_WEB}"
export WEB_PASSWORD
export DATA_PHOTOS="${DATA_FAKE}/photos"

log "  AC-8 Playwright: web login, Proton totp, add pair, dry run"
PHASE=ac8 "$PYWEB" "${ROOT}/docker/test_web_e2e.py" || die "AC-8 web e2e failed"

if [ -e "${CFG_FAKE}/state/neutronsync/session.enc" ]; then
	die "session.enc exists with fake sidecar"
fi
log "  AC-8 ok (no session.enc)"

# AC-9: local remote.txt → Sync now (upload) → drop it from the fake tree →
# Sync now again → delete-local lands in /data/.Trash-<uid>/files.
# docker exec needs -i so the heredoc reaches python's stdin.
printf 'local-body' >"${DATA_FAKE}/photos/remote.txt"
docker exec -i -u root "$CID_FAKE" python3 -c \
	'import json; from pathlib import Path; Path("/opt/fake-sidecar/controls.json").write_text(json.dumps({"signed_in": True}))'

log "  AC-9 prepare: baseline sync"
PHASE=ac9-prepare "$PYWEB" "${ROOT}/docker/test_web_e2e.py" || die "AC-9 prepare failed"

# Proton-side delete: drop every remote.txt under photos (uid may be a UUID).
docker exec -i -u root "$CID_FAKE" python3 - <<'PY'
import json
from pathlib import Path
home = Path("/opt/fake-sidecar")
tree = json.loads(home.joinpath("tree.json").read_text())
removed = [
    k
    for k, n in list(tree.items())
    if n.get("name") == "remote.txt" and n.get("parent_uid") == "photos"
]
for k in removed:
    del tree[k]
if not removed:
    raise SystemExit("AC-9: no remote.txt in fake tree after prepare")
home.joinpath("tree.json").write_text(json.dumps(tree))
print("removed", removed)
PY

log "  AC-9 assert: sync after remote delete → Activity + /data/.Trash-${PUID}/files"
EXPECT_TRASH="${DATA_FAKE}/.Trash-${PUID}/files"
export EXPECT_TRASH
PHASE=ac9-assert "$PYWEB" "${ROOT}/docker/test_web_e2e.py" || die "AC-9 web/trash e2e failed"
log "  AC-9 ok"

docker rm -f "$CID_FAKE" >/dev/null
CID_FAKE=""
docker rmi "$FAKE_IMAGE" >/dev/null 2>&1 || true
FAKE_IMAGE=""

log "AC-8 sidecar file-store tests ran in image build (sidecar stage)"
log "ALL CHECKS PASSED"
