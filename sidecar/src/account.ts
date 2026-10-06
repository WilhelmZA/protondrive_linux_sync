import { CryptoProxy, VERIFICATION_STATUS, type PrivateKeyReference, type PublicKeyReference } from '@protontech/crypto';
import { Api } from '@protontech/crypto/proxy/endpoint/api.ts';
import type { ProtonDriveAccount, ProtonDriveAccountAddress } from '@protontech/drive-sdk';
import type { Auth } from './auth';
import { Fault } from './errors';

Api.init({});
CryptoProxy.setEndpoint(new Api());

type Key = { ID: string; PrivateKey: string; Primary: number; Active?: number; Token?: string; Signature?: string };

export class Account implements ProtonDriveAccount {
  private addresses: ProtonDriveAccountAddress[] = [];
  constructor(private auth: Auth) {}
  async unlock(passphrase: string) {
    const [{ User }, { Addresses }] = await Promise.all([this.auth.json('/core/v4/users'), this.auth.json('/core/v4/addresses')]);
    const userKeys: PrivateKeyReference[] = [];
    for (const key of User.Keys as Key[]) {
      try { userKeys.push(await CryptoProxy.importPrivateKey({ armoredKey: key.PrivateKey, passphrase })); }
      catch { /* Old inactive user keys can require a previous passphrase. */ }
    }
    if (!userKeys.length) throw new Fault('auth');
    const addresses: ProtonDriveAccountAddress[] = [];
    for (const address of [...Addresses].sort((a, b) => (a.Order ?? 0) - (b.Order ?? 0))) {
      if (address.Status !== undefined && address.Status !== 1) continue;
      const keys: ProtonDriveAccountAddress['keys'] = [];
      let primaryKeyIndex = 0;
      for (const key of address.Keys as Key[]) {
        if (key.Active === 0) continue;
        let password = passphrase;
        if (key.Token) {
          if (!key.Signature) throw new Fault('auth');
          const token = await CryptoProxy.decryptMessage({ armoredMessage: key.Token, armoredSignature: key.Signature, decryptionKeys: userKeys, verificationKeys: userKeys });
          if (token.verificationStatus !== VERIFICATION_STATUS.SIGNED_AND_VALID) throw new Fault('auth');
          password = token.data;
        }
        const unlocked = await CryptoProxy.importPrivateKey({ armoredKey: key.PrivateKey, passphrase: password });
        if (key.Primary === 1) primaryKeyIndex = keys.length;
        keys.push({ id: key.ID, key: unlocked });
      }
      if (keys.length) addresses.push({ email: address.Email, addressId: address.ID, primaryKeyIndex, keys });
    }
    if (!addresses.length) throw new Fault('auth');
    this.addresses = addresses;
  }
  async getOwnAddresses() { return this.addresses; }
  async getOwnPrimaryAddress() {
    if (!this.addresses[0]) throw new Fault('auth');
    return this.addresses[0];
  }
  async getOwnAddress(id: string) {
    const address = this.addresses.find(a => a.addressId === id || a.email.toLowerCase() === id.toLowerCase());
    if (!address) throw new Fault('auth');
    return address;
  }
  // The SDK asks for the signer's keys once per node it verifies; without this cache a walk
  // sends one keys request per file, almost all for the same few addresses, and gets rate limited.
  private publicKeys = new Map<string, Promise<PublicKeyReference[]>>();
  getPublicKeys(email: string) {
    const key = email.toLowerCase();
    let keys = this.publicKeys.get(key);
    if (!keys) {
      keys = this.fetchPublicKeys(email);
      keys.catch(() => this.publicKeys.delete(key));
      this.publicKeys.set(key, keys);
    }
    return keys;
  }
  private async fetchPublicKeys(email: string): Promise<PublicKeyReference[]> {
    for (let attempt = 0; ; attempt++) {
      try {
        const data = await this.auth.json(`/core/v4/keys?Email=${encodeURIComponent(email)}`);
        return Promise.all((data.Keys ?? []).map((key: { PublicKey: string }) => CryptoProxy.importPublicKey({ armoredKey: key.PublicKey })));
      } catch (error) {
        if (!(error instanceof Fault) || error.code !== 'rate_limited' || attempt >= 4) throw error;
        await Bun.sleep(Math.max(1, error.retry_after ?? 10) * 1000);
      }
    }
  }
  async hasProtonAccount(email: string) { return (await this.getPublicKeys(email)).length > 0; }
}
