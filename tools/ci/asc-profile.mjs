// Ensure an IOS_APP_STORE provisioning profile exists for org.ankurah.community
// that includes the NEWEST Apple Distribution cert, and write its
// .mobileprovision. Get-or-create: reuses the named profile when it is ACTIVE
// and already carries the newest cert; otherwise deletes and re-mints it
// (profiles are cheap, re-mintable API objects — the cert's private key is the
// only real secret).
//
// Runs with the App Manager ASC key: that role can create/delete PROFILES via
// the API — it is cert MINTING it cannot do (that needs a portal CSR; see
// .github/workflows/ios-release.yml). Ported from synestheticsystems/anku's
// tools/ci/asc-profile.mjs, which proved this path on 2026-07-20.
//
// Because the profile is minted from the App ID's CURRENT capabilities, it
// carries whatever the App ID carries — so the Push Notifications capability we
// enabled on the App ID is included automatically, and an archive that entitles
// `aps-environment` signs cleanly with no hand-made profile to keep in step.
//
// Usage:
//   ASC_KEY_ID=.. ASC_ISSUER_ID=.. node tools/ci/asc-profile.mjs <out.mobileprovision> [name]
// Reads the .p8 from ~/.appstoreconnect/private_keys/AuthKey_$ASC_KEY_ID.p8.
// Prints "PROFILE_OK <uuid> <name> <reused|minted>" on success.
import { readFileSync, writeFileSync } from 'node:fs';
import { sign as cryptoSign } from 'node:crypto';

const BUNDLE_ID = 'org.ankurah.community';
const keyId = process.env.ASC_KEY_ID, issuer = process.env.ASC_ISSUER_ID;
const outPath = process.argv[2];
const profName = process.argv[3] || 'community-appstore';
if (!keyId || !issuer || !outPath) { console.error('missing env/args'); process.exit(1); }
const key = readFileSync(`${process.env.HOME}/.appstoreconnect/private_keys/AuthKey_${keyId}.p8`, 'utf8');

const b64u = (buf) => Buffer.from(buf).toString('base64url');
const now = Math.floor(Date.now() / 1000);
const header = b64u(JSON.stringify({ alg: 'ES256', kid: keyId, typ: 'JWT' }));
const payload = b64u(JSON.stringify({ iss: issuer, iat: now, exp: now + 900, aud: 'appstoreconnect-v1' }));
const sig = cryptoSign('sha256', Buffer.from(`${header}.${payload}`), { key, dsaEncoding: 'ieee-p1363' });
const jwt = `${header}.${payload}.${b64u(sig)}`;

const api = async (path, opts = {}) => {
  const res = await fetch(`https://api.appstoreconnect.apple.com${path}`, {
    ...opts,
    headers: { Authorization: `Bearer ${jwt}`, 'Content-Type': 'application/json' },
  });
  if (res.status === 204) return {};
  const body = await res.json().catch(() => ({}));
  if (!res.ok) {
    console.error('API_ERR', res.status, path, JSON.stringify(body.errors || body).slice(0, 600));
    process.exit(2);
  }
  return body;
};

// Newest Apple Distribution cert (the one whose p12 CI imports).
const certs = await api('/v1/certificates?filter[certificateType]=DISTRIBUTION&limit=200');
const cert = certs.data
  .sort((a, b) => new Date(b.attributes.expirationDate) - new Date(a.attributes.expirationDate))[0];
if (!cert) { console.error('NO_DIST_CERT — mint one via CSR in the portal first'); process.exit(3); }
console.error(`cert: ${cert.id} exp=${cert.attributes.expirationDate}`);

// Reuse the named profile only if ACTIVE and already carrying that cert.
const existing = await api(`/v1/profiles?filter[name]=${encodeURIComponent(profName)}&include=certificates`);
const prior = existing.data?.[0];
if (prior) {
  const certIds = (prior.relationships?.certificates?.data || []).map(c => c.id);
  if (prior.attributes.profileState === 'ACTIVE' && certIds.includes(cert.id)) {
    writeFileSync(outPath, Buffer.from(prior.attributes.profileContent, 'base64'));
    console.log('PROFILE_OK', prior.attributes.uuid, prior.attributes.name, 'reused');
    process.exit(0);
  }
  console.error(`stale profile ${prior.id} (state=${prior.attributes.profileState}) — deleting`);
  await api(`/v1/profiles/${prior.id}`, { method: 'DELETE' });
}

const bids = await api(`/v1/bundleIds?filter[identifier]=${BUNDLE_ID}`);
const bid = bids.data.find(d => d.attributes.identifier === BUNDLE_ID);
if (!bid) { console.error('NO_BUNDLE_ID'); process.exit(4); }

const prof = await api('/v1/profiles', {
  method: 'POST',
  body: JSON.stringify({
    data: {
      type: 'profiles',
      attributes: { name: profName, profileType: 'IOS_APP_STORE' },
      relationships: {
        bundleId: { data: { type: 'bundleIds', id: bid.id } },
        certificates: { data: [{ type: 'certificates', id: cert.id }] },
      },
    },
  }),
});
writeFileSync(outPath, Buffer.from(prof.data.attributes.profileContent, 'base64'));
console.log('PROFILE_OK', prof.data.attributes.uuid, prof.data.attributes.name, 'minted');
