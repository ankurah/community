#!/usr/bin/env node
// Pick a free even/odd port pair ONCE (same scheme as dev.sh), export it, then run
// Playwright. Because the ports are chosen up front and passed via env, the config
// and every worker process see the SAME ports — so a standalone `npm run test:e2e`
// never collides with other local services, and the webServer + baseURL never
// diverge. Honors SERVER_PORT/WEB_PORT if already set (dev.sh or CI can pin them).
import { spawnSync } from 'node:child_process';
import net from 'node:net';

const free = (p) =>
  new Promise((r) => {
    const s = net.createServer();
    s.once('error', () => r(false));
    s.once('listening', () => s.close(() => r(true)));
    s.listen(p, '0.0.0.0');
  });

async function pickPair() {
  if (process.env.SERVER_PORT && process.env.WEB_PORT) {
    return [process.env.SERVER_PORT, process.env.WEB_PORT];
  }
  for (let i = 0; i < 200; i++) {
    const b = 10000 + 2 * Math.floor(Math.random() * 4999);
    if ((await free(b)) && (await free(b + 1))) return [String(b), String(b + 1)];
  }
  throw new Error('e2e: could not find a free port pair in 10000-19998');
}

const [SERVER_PORT, WEB_PORT] = await pickPair();
console.log(`[e2e] ports: server=${SERVER_PORT}  web=${WEB_PORT}`);

const res = spawnSync('npx', ['--no-install', 'playwright', 'test', ...process.argv.slice(2)], {
  stdio: 'inherit',
  env: { ...process.env, SERVER_PORT, WEB_PORT },
});
process.exit(res.status ?? 1);
