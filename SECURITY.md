# Security

## Session ownership and credentials

The `neutronsync-drive` sidecar owns the Proton API session. It stores the account identifier, session UID, access token, refresh token, derived key passphrase and expiry in Secret Service. The item label is `NeutronSync Drive session`; its attributes are `application=neutronsync-drive` and `purpose=session-v1`. The old phase-labelled item migrates only after a successful write and verified readback. The CLI backend uses its own separate session.

One sidecar serves the user session and performs token refresh. A shared in-flight refresh prevents parallel API requests from rotating the same refresh token independently. The session-wide socket and advisory lock extend that ownership across GUI, tray, CLI and watch processes. This prevents competing refreshers from invalidating each other's session.

Credentials cross the local Unix socket during sign-in. The GUI sign-in form sends username, password, two-factor codes and mailbox passwords only to the sidecar over that user-only socket. It never writes them to disk or logs, and it clears the password and code fields after each attempt. Password fields are masked. Credentials never enter logs, activity records, configuration or process arguments. Error messages use typed codes instead of server text that could echo secrets. Processes running as the same user are inside the trust boundary: they can connect to the socket and access that user's keyring. Root and a compromised desktop session are also outside this protection.

## Local transport

The socket lives at `$XDG_RUNTIME_DIR/neutronsync-drive/session.sock`, or `$HOME/.cache/neutronsync-drive/session.sock` when `XDG_RUNTIME_DIR` is unset. Its directory must belong to the user and have mode `0700`. The owner holds a `0600` lock file beside the socket. It checks every peer's `SO_PEERCRED` UID before reading requests and refuses another UID. Stale sockets are removed only by the process holding the lock.

Each connection receives its own replies, walk entries, transfer progress and event batches. Each connection owns independent event cursors and acknowledgements. Sign-out broadcasts to every connection. Killing a client does not kill the owner; after an owner crash, a new owner takes the released lock and watchers resume saved cursors. Stdio transport remains available for isolated fixtures and the diagnostic spike; do not run a real-session stdio owner beside the session owner.

## Remote operations

The sidecar sends recoverable trash operations. It has no `deleteNodes`, `emptyTrash` or `deleteRevision` call. Failed listings never count as empty folders. Downloads verify size and SHA-1 before an atomic rename. Proton's SDK performs encryption and decryption.

Requests identify NeutronSync with `x-pm-appversion: external-drive-neutronsync@<version>-dev`. NeutronSync never spoofs a first-party client identifier.

## Reporting a vulnerability

Use the repository's private vulnerability reporting page: <https://github.com/WilhelmZA/protondrive_linux_sync/security/advisories/new>. Include the affected version, reproduction steps and expected behavior. Exclude passwords, session tokens and keyring contents. If private reporting is unavailable, open an issue requesting a private contact without publishing exploit details or credentials.
