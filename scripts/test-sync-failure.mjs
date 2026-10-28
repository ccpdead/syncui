// Start Vite with: npm run dev -- --port 1421
// Run: NODE_PATH=<playwright modules> node scripts/test-sync-failure.mjs
// Every Tauri command is mocked; no filesystem sync or SSH connection is performed.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
const { chromium } = createRequire(import.meta.url)("playwright");
const url = process.env.SYNCUI_TEST_URL ?? "http://127.0.0.1:1421";
const browser = await chromium.launch({
  executablePath: process.env.CHROME_PATH ?? "/usr/bin/google-chrome",
  headless: true,
  args: ["--no-sandbox"],
});

try {
  for (const scenario of ["partial", "rejected", "cancelled", "refresh-failed", "skipped", "success", "disconnected"]) {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", e => errors.push(String(e)));
    await page.route("**/*", async route => {
      if (new URL(route.request().url()).origin !== new URL(url).origin) return route.abort();
      return route.continue();
    });
    await page.route("**/src/main.tsx", async route => {
      const response = await route.fetch();
      await route.fulfill({ response, body: `
        import { mockIPC, mockWindows } from "/node_modules/@tauri-apps/api/mocks.js";
        import { emit } from "/node_modules/@tauri-apps/api/event.js";
        window.isTauri = true;
        mockWindows("main");
        const scenario = ${JSON.stringify(scenario)};
        const profile = { id:"mock", name:"mock", host:"mock.invalid", user:"mock", identityFile:"", auth:"auto", rememberPassword:false };
        const entry = { relPath:"keep.txt", action:"upload", localSize:10, remoteSize:null, localMtime:100, remoteMtime:null, newer:"local" };
        const diff = { comparisonId:"plan", complete:true, entries:[entry], uploadCount:1, downloadCount:0, deleteLocalCount:0, deleteRemoteCount:0, conflictCount:0, sameCount:0, skippedCount:0 };
        window.syncTestCalls = [];
        mockIPC(async (cmd, args) => {
          window.syncTestCalls.push({cmd,args});
          switch (cmd) {
            case "load_settings": return { localPath:"/mock/local", remotePath:"/mock/remote", mode:"mirror", conflictPolicy:"newer", useHash:false, concurrency:4, ignoreText:"",
              remoteEndpoint:scenario === "disconnected" ? {kind:"ssh",profileId:"mock",path:"/mock/remote"} : null };
            case "save_settings": case "save_ssh_profiles": return;
            case "load_ssh_profiles": return [profile];
            case "connect_ssh": return {id:"mock", user:"mock", home:"/mock", arch:"x86_64", hostKey:"mock", rsyncReady:true, agentReady:true, message:"mock only"};
            case "compare_dirs": return diff;
            case "sync_entries": {
              if (scenario === "rejected") throw new Error("mock transport failure");
              if (scenario === "disconnected") {
                await emit("ssh-connection-state", {connectionId:"mock",connected:false,reason:"mock disconnected"});
                throw new Error("mock disconnected");
              }
              const success = scenario === "success";
              return {items:[], donePaths:[], uploaded:success ? 1 : 0, downloaded:0, deletedLocal:0, deletedRemote:0,
                skipped:scenario === "skipped" ? 1 : 0, failed:scenario === "partial" ? 1 : 0,
                errors:scenario === "partial" ? ["rsync exit=12 mock failure"] : [],
                cancelled:scenario === "cancelled", needsRecompare:!success,
                bytesCopied:10, logicalBytes:success ? 10 : 0, elapsedSeconds:1,
                refreshed:success ? {...diff, comparisonId:"fresh", entries:[], uploadCount:0, sameCount:1} : scenario === "skipped" ? diff : null};
            }
            default: throw new Error("Unexpected mock command: " + cmd);
          }
        }, { shouldMockEvents:true });
      ` + await response.text() });
    });
    await page.goto(url);
    if (scenario === "disconnected") await page.getByRole("button", {name:"连接并检测", exact:true}).click();
    const compare = page.getByRole("button", {name:"对比 Compare", exact:true});
    await compare.click();
    await page.getByRole("button", {name:"全选", exact:true}).click();
    await page.getByRole("button", {name:"同步选中 (1)", exact:true}).click();
    if (scenario === "success") {
      await page.getByRole("button", {name:"同步选中 (0)", exact:true}).waitFor();
      assert.equal(await page.getByText("keep.txt", {exact:true}).count(), 0);
      assert.equal(await page.getByText(/需要重新对比：当前差异列表/).count(), 0);
    } else {
      await page.getByText(/需要重新对比：当前差异列表/).waitFor();
      assert.equal(await page.getByText("keep.txt", {exact:true}).count(), 1);
      assert.equal(await page.getByRole("button", {name:"同步选中 (1)", exact:true}).isDisabled(), true);
      if (scenario === "partial") assert.match(await page.locator(".error").first().innerText(), /rsync exit=12/);
      if (scenario === "disconnected") await page.getByRole("button", {name:"连接并检测", exact:true}).click();
      await compare.click();
      await page.getByRole("button", {name:"全选", exact:true}).click();
      assert.equal(await page.getByRole("button", {name:"同步选中 (1)", exact:true}).isEnabled(), true);
      assert.equal(await page.getByText(/需要重新对比：当前差异列表/).count(), 0);
    }
    const syncCalls = await page.evaluate(() => window.syncTestCalls.filter(x => x.cmd === "sync_entries"));
    assert.equal(syncCalls.length, 1);
    assert.equal(syncCalls[0].args.comparisonId, "plan");
    assert.deepEqual(errors, []);
    console.log(`PASS ${scenario}: diff state, plan invalidation, and recompare recovery`);
    await page.close();
  }
} finally {
  await browser.close();
}
