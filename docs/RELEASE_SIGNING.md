# Release signing

Every release from 0.7.0 on ships `SHA256SUMS`, a checksum list over the four release files, and `SHA256SUMS.asc`, a detached signature over that list made with NeutronSync's release signing key. The in-app updater verifies the signature against the public keys compiled into it (`packaging/release-signing-key.asc`, which holds one key normally and two during a rotation) before it installs anything, the AUR package verifies it before it builds, and a user can verify it by hand. The release workflow refuses to publish when it cannot sign, refuses when the secret's key is not one of the committed keys, and `scripts/release.sh` refuses to tag while the committed public key is still the placeholder.

This page is the operator's manual: creating the key, wiring it into GitHub, verifying a release by hand, rotating the key, and publishing to the AUR.

## What the key is

- A dedicated key, not the key that signs commits. If a CI secret ever leaks, it exposes release signing only, and the commit identity stays clean.
- ed25519, sign-only, three-year expiry, user ID `NeutronSync release signing <82225237+WilhelmZA@users.noreply.github.com>`.
- The private half lives in exactly two places: the GitHub secret `RELEASE_GPG_KEY` and an offline backup. The passphrase lives in the secret `RELEASE_GPG_PASSPHRASE` and in the same backup.
- The public half is committed as `packaging/release-signing-key.asc` and embedded in every build. The file may hold more than one armored key block; a signature by any key in it is accepted, which is what makes rotation possible.

## Creating the key

Work in a scratch keyring so nothing touches the everyday one, and keep the terminal history out of it (`set +o history` in bash, or a leading space with `HISTCONTROL=ignorespace`).

1. Generate the key. `gpg` asks for the passphrase through the pinentry; choose a long one and keep it for step 4.

   ```sh
   export GNUPGHOME="$(mktemp -d)"
   chmod 700 "$GNUPGHOME"
   gpg --quick-gen-key 'NeutronSync release signing <82225237+WilhelmZA@users.noreply.github.com>' ed25519 sign 3y
   gpg --list-keys --with-colons | awk -F: '$1 == "fpr" { print $10; exit }'
   ```

   The last line prints the fingerprint. Keep it: the AUR `PKGBUILD` and the README both quote it.

2. Export the public key into the repository and commit it. This replaces the placeholder, and from this commit on `scripts/release.sh` will tag.

   ```sh
   gpg --armor --export '82225237+WilhelmZA@users.noreply.github.com' > packaging/release-signing-key.asc
   sed -i "s/^validpgpkeys=.*/validpgpkeys=('<fingerprint>')/" packaging/aur/PKGBUILD
   git add packaging/release-signing-key.asc packaging/aur/PKGBUILD
   git commit -m "Add the release signing public key"
   ```

   Also replace the fingerprint placeholder in `README.md` under "Verifying a release".

3. Export the private key and create the two GitHub secrets. `gh secret set` reads the value from standard input, so the key never appears on a command line.

   ```sh
   gpg --armor --export-secret-keys '82225237+WilhelmZA@users.noreply.github.com' | gh secret set RELEASE_GPG_KEY
   gh secret set RELEASE_GPG_PASSPHRASE
   ```

   The second command prompts for the passphrase.

4. Store an offline backup: the same armored secret export and the passphrase, on an encrypted drive that is not this machine. The GitHub secret cannot be read back, so this backup is the only other copy.

5. Delete the scratch keyring: `rm -rf "$GNUPGHOME"; unset GNUPGHOME`.

The first tagged release after this proves the whole chain: the workflow imports the secret, signs `SHA256SUMS`, then verifies that signature against the committed public key in a fresh keyring and fails if they do not match.

## Verifying a release by hand

A user who wants to check a download before installing it:

```sh
curl -LO https://raw.githubusercontent.com/WilhelmZA/protondrive_linux_sync/rust/packaging/release-signing-key.asc
gpg --import release-signing-key.asc
gpg --fingerprint '82225237+WilhelmZA@users.noreply.github.com'   # compare with the fingerprint in README.md
gpg --verify SHA256SUMS.asc SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
```

`gpg --verify` must say `Good signature`; the warning that the key is not certified only means the user has not signed the key themselves. `sha256sum` prints `OK` for each downloaded file it finds in the list.

## Rotating the key

A copy only trusts the keys it was built with, and the release workflow only signs with a key that is committed. Rotation therefore goes through one release that trusts both keys and is still signed by the old one.

1. Generate the new key as above, in a scratch keyring, and note its fingerprint.
2. Append the new public key to `packaging/release-signing-key.asc`, after the old one, so the file holds two armored blocks:

   ```sh
   gpg --armor --export '<new fingerprint>' >> packaging/release-signing-key.asc
   ```

   Add the new fingerprint to `validpgpkeys` in `packaging/aur/PKGBUILD` and to the README next to the old one, and commit. Do not change the secrets yet.
3. Ship one release. The workflow still signs with the old secret, which it accepts because the old key is still committed. Installed copies verify that release with the old key, and once installed they trust both keys.
4. Switch the secrets to the new key (`gh secret set RELEASE_GPG_KEY` from the new armored secret export, `gh secret set RELEASE_GPG_PASSPHRASE`), update the offline backup, and ship the next release. The workflow now signs with the new key, which copies from step 3 verify.
5. In a later release, remove the old key's block from `packaging/release-signing-key.asc` and its fingerprint from the PKGBUILD and README. Revoke it if it was compromised; otherwise let it expire.

Anyone who skipped the bridging release of step 3 has to download the next one by hand and verify it as above. If the secret is switched before the new public key is committed, the workflow stops with "is not one of the public keys committed" and nothing is published.

## Publishing to the AUR

The `neutronsync-bin` package is pushed by `.github/workflows/aur.yml` when a release is promoted to stable with `scripts/promote.sh`. Pre-release versions never go to the AUR. Without the `AUR_SSH_KEY` secret the workflow logs a notice and does nothing, so the AUR is opt-in.

1. Create an account at <https://aur.archlinux.org> with the same name and e-mail as the PKGBUILD maintainer line.
2. Generate an SSH key just for the AUR and add the public half under "My Account" on the AUR site:

   ```sh
   ssh-keygen -t ed25519 -f aur-key -C 'neutronsync aur publishing'
   ```

3. Create the secret from the private half, then delete the local copy:

   ```sh
   gh secret set AUR_SSH_KEY < aur-key
   shred -u aur-key
   ```

4. Promote a release (`scripts/promote.sh X.Y.Z`). The first push creates the package; there is nothing to create on the AUR beforehand.

The workflow pins the AUR host key by fingerprint. Confirm the fingerprint in `.github/workflows/aur.yml` against the one published at <https://aur.archlinux.org> before the first run.
