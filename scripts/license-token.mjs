#!/usr/bin/env node
// Issues a DataQuery Pro token by hand (offline customers, refunds re-issued, gifts).
// The online path is DataQueryWebsite/functions/api/license/[action].js; both sign the
// same claims with the same Ed25519 key, which lives outside the repository.
//
//   node scripts/license-token.mjs --name "Ada Lovelace" --email ada@example.com --order 1001
//   (key: $DATAQUERY_LICENSE_KEY, default ~/.dataquery/license-signing.pem)

import { createPrivateKey, sign } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { parseArgs } from 'node:util';

const { values } = parseArgs({
  options: { name: { type: 'string' }, email: { type: 'string' }, order: { type: 'string' }, instance: { type: 'string' } },
});
if (!values.name || !values.email || !values.order) {
  console.error('usage: license-token.mjs --name <name> --email <email> --order <order id> [--instance <id>]');
  process.exit(2);
}

const keyPath = process.env.DATAQUERY_LICENSE_KEY ?? join(homedir(), '.dataquery', 'license-signing.pem');
const key = createPrivateKey(readFileSync(keyPath));
const claims = { v: 1, plan: 'pro', name: values.name, email: values.email, order: values.order, instance: values.instance ?? null, issued: new Date().toISOString().slice(0, 10) };
const payload = Buffer.from(JSON.stringify(claims)).toString('base64url');
const signature = sign(null, Buffer.from(payload), key).toString('base64url');
console.log(`DQ1.${payload}.${signature}`);
