import { describe, expect, test } from 'bun:test';
import { appVersion } from '../src/auth';
import { neutronsyncDriveVersion } from '../src/build-version';

describe('drive client version header', () => {
  test('appVersion matches the generated build label', () => {
    expect(appVersion).toBe(`external-drive-neutronsync@${neutronsyncDriveVersion}`);
  });
});
