import { defineConfig, devices } from '@playwright/test';
import { execFileSync } from 'node:child_process';

// Ports: honor dev.sh's / the wrapper's exported env (SERVER_PORT / WEB_PORT) when
// present; otherwise pick a free random even/odd pair — the same scheme dev.sh uses
// — so a standalone `npm run test:e2e` never collides with other local services.
function freePortPair(): [string, string] {
  const script =
    "const net=require('net');" +
    "const free=p=>new Promise(r=>{const s=net.createServer();s.once('error',()=>r(false));s.once('listening',()=>s.close(()=>r(true)));s.listen(p,'0.0.0.0');});" +
    "(async()=>{for(let i=0;i<200;i++){const b=10000+2*Math.floor(Math.random()*4999);if(await free(b)&&await free(b+1)){process.stdout.write(b+' '+(b+1));return;}}process.exit(1);})();";
  const [s, w] = execFileSync(process.execPath, ['-e', script]).toString().trim().split(' ');
  return [s, w];
}

function resolvePorts(): [string, string] {
  if (process.env.SERVER_PORT && process.env.WEB_PORT) {
    return [process.env.SERVER_PORT, process.env.WEB_PORT];
  }
  const [s, w] = freePortPair();
  // Stabilize across re-imports (Playwright reloads this config per worker).
  process.env.SERVER_PORT = s;
  process.env.WEB_PORT = w;
  return [s, w];
}

const [SERVER_PORT, WEB_PORT] = resolvePorts();

export default defineConfig({
  testDir: '.',
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  workers: 1,
  reporter: 'list',
  use: {
    baseURL: `http://localhost:${WEB_PORT}`,
    trace: 'on-first-retry',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
  webServer: [
    {
      // Hermetic e2e: run the durable node on Sled so no Postgres container is
      // needed, even though the app defaults to Postgres for deployment.
      command: 'cargo run -p community-server --release --no-default-features --features sled',
      cwd: '..',
      port: parseInt(SERVER_PORT),
      env: { SERVER_PORT },
      reuseExistingServer: !process.env.CI,
      timeout: 180000,
    },
    {
      // trunk builds the Leptos app to wasm, serves it on $WEB_PORT, and proxies
      // /ws to the backend so the client uses one same-origin URL.
      command: `trunk serve --address 0.0.0.0 --port ${WEB_PORT} --proxy-backend ws://127.0.0.1:${SERVER_PORT}/ws --proxy-ws`,
      cwd: '../leptos-app',
      port: parseInt(WEB_PORT),
      reuseExistingServer: !process.env.CI,
      timeout: 180000,
    },
  ],
});
