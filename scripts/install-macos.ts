import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, "..");

const BUILT_APP_PATH = path.join(
  REPO_ROOT,
  "src-tauri",
  "target",
  "release",
  "bundle",
  "macos",
  "Handy.app",
);
const TARGET_APP_PATH = "/Applications/Handy.app";
const BACKUP_APP_PATH = "/Applications/Handy-backup.app";

interface CodesignInfo {
  teamId: string | null;
  authorities: string[];
}

function sleepMs(ms: number): void {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function readCodesignInfo(appPath: string): CodesignInfo {
  const result = spawnSync("codesign", ["-dv", "--verbose=2", appPath], {
    encoding: "utf8",
  });
  const output = `${result.stderr ?? ""}\n${result.stdout ?? ""}`;
  let teamId: string | null = null;
  const authorities: string[] = [];

  for (const line of output.split(/\r?\n/)) {
    if (line.startsWith("TeamIdentifier=")) {
      const value = line.slice("TeamIdentifier=".length).trim();
      teamId = value || null;
    } else if (line.startsWith("Authority=")) {
      const value = line.slice("Authority=".length).trim();
      if (value) {
        authorities.push(value);
      }
    }
  }

  return { teamId, authorities };
}

function isHandyRunning(): boolean {
  return spawnSync("pgrep", ["-x", "handy"], { stdio: "ignore" }).status === 0;
}

function quitRunningHandy(): void {
  if (!isHandyRunning()) {
    return;
  }

  console.log("[handy] Quitting running Handy instance...");
  spawnSync("osascript", ["-e", 'tell application "Handy" to quit'], {
    stdio: "ignore",
  });

  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    if (!isHandyRunning()) {
      return;
    }
    sleepMs(250);
  }

  console.log("[handy] Handy did not exit gracefully; terminating process...");
  spawnSync("pkill", ["-x", "handy"], { stdio: "ignore" });
  sleepMs(500);
}

function runOrExit(cmd: string, args: string[], errorMessage: string): void {
  const result = spawnSync(cmd, args, {
    cwd: REPO_ROOT,
    stdio: "inherit",
  });
  if (result.status !== 0) {
    console.error(`[handy] ${errorMessage}`);
    process.exit(result.status ?? 1);
  }
}

function main(): void {
  if (process.platform !== "darwin") {
    console.error("[handy] install:macos is only supported on macOS.");
    process.exit(1);
  }

  console.log("[handy] Building signed macOS .app bundle...");
  runOrExit(
    "bun",
    ["run", "tauri", "build", "--bundles", "app"],
    "Failed to build Handy.app.",
  );

  if (!fs.existsSync(BUILT_APP_PATH)) {
    console.error(`[handy] Built app bundle not found at ${BUILT_APP_PATH}`);
    process.exit(1);
  }

  const builtInfo = readCodesignInfo(BUILT_APP_PATH);
  console.log(
    `[handy] Built app signature — TeamIdentifier=${builtInfo.teamId ?? "none"}, Authority=${builtInfo.authorities[0] ?? "none"}`,
  );

  quitRunningHandy();

  if (fs.existsSync(TARGET_APP_PATH) && !fs.existsSync(BACKUP_APP_PATH)) {
    const existingInfo = readCodesignInfo(TARGET_APP_PATH);
    if (existingInfo.teamId !== builtInfo.teamId) {
      console.log(
        `[handy] Backing up existing Handy.app (TeamIdentifier=${existingInfo.teamId ?? "none"}) to ${BACKUP_APP_PATH}...`,
      );
      fs.renameSync(TARGET_APP_PATH, BACKUP_APP_PATH);
    }
  }

  if (fs.existsSync(TARGET_APP_PATH)) {
    fs.rmSync(TARGET_APP_PATH, { recursive: true, force: true });
  }

  console.log(`[handy] Installing ${BUILT_APP_PATH} -> ${TARGET_APP_PATH}...`);
  runOrExit(
    "ditto",
    [BUILT_APP_PATH, TARGET_APP_PATH],
    `Failed to copy ${BUILT_APP_PATH} to ${TARGET_APP_PATH}.`,
  );

  console.log("[handy] Launching Handy...");
  runOrExit(
    "open",
    [TARGET_APP_PATH],
    "Failed to launch /Applications/Handy.app.",
  );
}

if ((import.meta as ImportMeta & { main?: boolean }).main) {
  main();
}
