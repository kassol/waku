import { expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { launchDevelopmentApp, stopDevelopmentApp } from "./dev-process";

test.skipIf(process.platform !== "darwin")("test launch and stop preserve the installed app sentinel and reject release identity", async () => {
  const root = mkdtempSync(join(tmpdir(), "waku-isolation-"));
  const sentinel = Bun.spawn([process.execPath, "-e", "setInterval(() => {}, 1000)"], { stdout: "ignore" });
  let app: ReturnType<typeof launchDevelopmentApp> | undefined;
  try {
    const bundle = join(root, "Waku Debug.app");
    mkdirSync(join(bundle, "Contents/MacOS"), { recursive: true });
    const plist = join(bundle, "Contents/Info.plist");
    const identity = (id: string) => writeFileSync(plist, `<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>${id}</string></dict></plist>`);
    identity("sh.waku.dev");
    writeFileSync(join(bundle, "Contents/MacOS/Waku Debug"), `#!${process.execPath}\nconsole.log(JSON.stringify(process.env)); setInterval(() => {}, 1000);`, { mode: 0o755 });
    const original = join(root, "original-settings.json");
    writeFileSync(original, "original sentinel");
    app = launchDevelopmentApp(bundle, join(root, "waku-debug-daemon"), root, {
      ...process.env,
      WAKU_DAEMON_ADDRESS: "127.0.0.1:1",
      WAKU_DAEMON_TOKEN: "original-token",
      WAKU_FORCE_UPDATER: "1",
      WAKU_APP_EXECUTABLE: "/Applications/Waku.app/Contents/MacOS/Waku",
    }, "pipe");
    const reader = app.stdout!.getReader();
    const output = await reader.read();
    const env = JSON.parse(new TextDecoder().decode(output.value));
    expect(env.WAKU_DAEMON_ADDRESS).toBeUndefined();
    expect(env.WAKU_DAEMON_TOKEN).toBeUndefined();
    expect(env.WAKU_APP_EXECUTABLE).toBeUndefined();
    expect(env.WAKU_FORCE_UPDATER).toBeUndefined();
    await stopDevelopmentApp(app);
    expect(sentinel.exitCode).toBeNull();
    expect(await Bun.file(original).text()).toBe("original sentinel");
    identity("sh.waku");
    expect(() => launchDevelopmentApp(bundle, "daemon", root)).toThrow("test identity");
  } finally {
    await stopDevelopmentApp(app);
    sentinel.kill();
    await sentinel.exited;
    rmSync(root, { recursive: true, force: true });
  }
});
