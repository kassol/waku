import { join } from "node:path";

/** Launch only the test bundle; never inherit a connection to another daemon. */
export function launchDevelopmentApp(
  appPath: string,
  daemonPath: string,
  cwd: string,
  environment: NodeJS.ProcessEnv = process.env,
  stdout: "pipe" | "inherit" = "inherit",
) {
  let executable = appPath;
  if (process.platform === "darwin") {
    const identity = Bun.spawnSync([
      "plutil", "-extract", "CFBundleIdentifier", "raw", "-o", "-",
      join(appPath, "Contents/Info.plist"),
    ]);
    if (identity.exitCode !== 0 || identity.stdout.toString().trim() !== "sh.waku.dev") {
      throw new Error("Development app must use the test identity sh.waku.dev");
    }
    executable = join(appPath, "Contents/MacOS/Waku Debug");
  }
  const env: NodeJS.ProcessEnv = { ...environment, WAKU_DAEMON_PATH: daemonPath };
  for (const name of ["WAKU_DAEMON_ADDRESS", "WAKU_DAEMON_TOKEN", "WAKU_APP_EXECUTABLE", "WAKU_FORCE_UPDATER", "WAKU_PREVIEW_UPDATE"]) {
    delete env[name];
  }
  return Bun.spawn([executable], { cwd, env, stdin: "ignore", stdout, stderr: "inherit" });
}

/** The watcher owns this handle; process names are never shutdown targets. */
export async function stopDevelopmentApp(app: ReturnType<typeof launchDevelopmentApp> | undefined) {
  if (app?.exitCode === null) {
    app.kill("SIGTERM");
    await app.exited;
  }
}
