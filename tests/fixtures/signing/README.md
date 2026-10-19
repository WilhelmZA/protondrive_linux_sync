# Signing test fixtures

Throwaway material for the updater's signature tests. The keys were generated in a scratch `GNUPGHOME` that was deleted afterwards; their private halves no longer exist anywhere, and none of them is the real release key (that one lives in `packaging/release-signing-key.asc`).

- `SHA256SUMS` lists four fake 0.7.0 assets whose contents are the lines `deb-bytes`, `rpm-bytes`, `tar-bytes` and `src-bytes`.
- `test-release-key.asc` is the first test key; `SHA256SUMS.asc` is its detached signature over the list.
- `test-release-key-2.asc` is the second test key; `SHA256SUMS.second-key.asc` is its signature. `test-release-keys-both.asc` is the two keys as two armored blocks (the rotation layout of `packaging/release-signing-key.asc`), and `test-release-keys-one-block.asc` is the same two keys in one block, as `gpg --armor --export k1 k2` writes them.
- `SHA256SUMS.other-key.asc` is a signature by a third throwaway key that is in none of the files, so a test can show that a signature from an unknown key is refused.

To regenerate, create three ed25519 sign-only keys with `gpg --quick-gen-key` under a fresh `GNUPGHOME`, rewrite the four files, run `sha256sum` over them in that order, sign the list with each key (`gpg --armor --detach-sign`), export the first two public keys separately, concatenated, and together in one export, and delete the `GNUPGHOME`.
