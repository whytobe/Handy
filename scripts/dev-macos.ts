import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { resolveMacSigningIdentity } from "./run-tauri.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, "..");
const TAURI_DIR = path.join(REPO_ROOT, "src-tauri");
const DEBUG_BIN_PATH = path.join(TAURI_DIR, "target", "debug", "handy");
const DEV_APP_PATH = path.join(
  TAURI_DIR,
  "target",
  "debug",
  "bundle",
  "macos",
  "Handy.app",
);
const DEV_SERVER_URL = "http://localhost:1420";

interface TauriConfigSummary {
  productName?: string;
  version?: string;
  identifier?: string;
}

function escapeXml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&apos;");
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function runSyncOrExit(
  cmd: string,
  args: string[],
  options: { cwd?: string; env?: NodeJS.ProcessEnv; errorMessage: string },
): void {
  const result = spawnSync(cmd, args, {
    cwd: options.cwd ?? REPO_ROOT,
    env: options.env ?? process.env,
    stdio: "inherit",
  });
  if (result.status !== 0) {
    console.error(`[handy] ${options.errorMessage}`);
    process.exit(result.status ?? 1);
  }
}

function readCodesignSummary(appPath: string): {
  teamId: string | null;
  authority: string | null;
} {
  const result = spawnSync("codesign", ["-dv", "--verbose=2", appPath], {
    encoding: "utf8",
  });
  const output = `${result.stderr ?? ""}\n${result.stdout ?? ""}`;
  let teamId: string | null = null;
  let authority: string | null = null;

  for (const line of output.split(/\r?\n/)) {
    if (line.startsWith("TeamIdentifier=")) {
      teamId = line.slice("TeamIdentifier=".length).trim() || null;
    } else if (!authority && line.startsWith("Authority=")) {
      authority = line.slice("Authority=".length).trim() || null;
    }
  }

  return { teamId, authority };
}

function buildInfoPlist(
  productName: string,
  version: string,
  identifier: string,
): string {
  const escapedName = escapeXml(productName);
  const escapedVersion = escapeXml(version);
  const escapedIdentifier = escapeXml(identifier);

  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
\t<key>CFBundleDevelopmentRegion</key>
\t<string>English</string>
\t<key>CFBundleDisplayName</key>
\t<string>${escapedName}</string>
\t<key>CFBundleExecutable</key>
\t<string>handy</string>
\t<key>CFBundleIdentifier</key>
\t<string>${escapedIdentifier}</string>
\t<key>CFBundleInfoDictionaryVersion</key>
\t<string>6.0</string>
\t<key>CFBundleName</key>
\t<string>${escapedName}</string>
\t<key>CFBundlePackageType</key>
\t<string>APPL</string>
\t<key>CFBundleShortVersionString</key>
\t<string>${escapedVersion}</string>
\t<key>CFBundleVersion</key>
\t<string>${escapedVersion}</string>
\t<key>CSResourcesFileMapped</key>
\t<true/>
\t<key>LSMinimumSystemVersion</key>
\t<string>10.15</string>
\t<key>CFBundleIconFile</key>
\t<string>icon.icns</string>
\t<key>LSRequiresCarbon</key>
\t<true/>
\t<key>NSHighResolutionCapable</key>
\t<true/>
\t<key>NSMicrophoneUsageDescription</key>
\t<string>Request microphone access to transcribe audio locally</string>
\t<key>NSAppTransportSecurity</key>
\t<dict>
\t\t<key>NSAllowsLocalNetworking</key>
\t\t<true/>
\t</dict>
</dict>
</plist>
`;
}

function assembleAndSignDevBundle(): void {
  if (!fs.existsSync(DEBUG_BIN_PATH)) {
    console.error(`[handy] Debug binary not found at ${DEBUG_BIN_PATH}`);
    process.exit(1);
  }

  const confPath = path.join(TAURI_DIR, "tauri.conf.json");
  const tauriConfig = JSON.parse(
    fs.readFileSync(confPath, "utf8"),
  ) as TauriConfigSummary;
  const productName = tauriConfig.productName || "Handy";
  const version = tauriConfig.version || "0.10.1";
  const identifier = tauriConfig.identifier || "com.pais.handy";

  const contentsDir = path.join(DEV_APP_PATH, "Contents");
  const macosDir = path.join(contentsDir, "MacOS");
  const resourcesDir = path.join(contentsDir, "Resources");
  const appResourcesDir = path.join(resourcesDir, "resources");

  fs.mkdirSync(macosDir, { recursive: true });
  fs.mkdirSync(resourcesDir, { recursive: true });

  const destBinPath = path.join(macosDir, "handy");
  fs.copyFileSync(DEBUG_BIN_PATH, destBinPath);
  fs.chmodSync(destBinPath, 0o755);

  fs.copyFileSync(
    path.join(TAURI_DIR, "icons", "icon.icns"),
    path.join(resourcesDir, "icon.icns"),
  );

  fs.rmSync(appResourcesDir, { recursive: true, force: true });
  runSyncOrExit("ditto", [path.join(TAURI_DIR, "resources"), appResourcesDir], {
    errorMessage: "Failed to copy src-tauri/resources into dev app bundle.",
  });

  fs.writeFileSync(
    path.join(contentsDir, "Info.plist"),
    buildInfoPlist(productName, version, identifier),
    "utf8",
  );

  const envIdentity =
    process.env.APPLE_SIGNING_IDENTITY?.trim() ||
    process.env.HANDY_SIGNING_IDENTITY?.trim();
  const detectedIdentity = envIdentity || resolveMacSigningIdentity();
  if (!detectedIdentity) {
    console.warn(
      "[handy] Warning: no Developer ID Application certificate found; falling back to ad-hoc signing ('-'). macOS TCC permissions may re-prompt.",
    );
  }
  const signingIdentity = detectedIdentity || "-";
  const entitlementsPath = path.join(TAURI_DIR, "Entitlements.plist");

  const signArgs = (targetPath: string) => [
    "--force",
    "--options",
    "runtime",
    "--timestamp=none",
    "--entitlements",
    entitlementsPath,
    "--sign",
    signingIdentity,
    targetPath,
  ];

  console.log(`[handy] Signing dev bundle with identity: ${signingIdentity}`);
  runSyncOrExit("codesign", signArgs(destBinPath), {
    errorMessage: "Failed to codesign debug binary inside dev bundle.",
  });
  runSyncOrExit("codesign", signArgs(DEV_APP_PATH), {
    errorMessage: "Failed to codesign dev Handy.app bundle.",
  });

  const summary = readCodesignSummary(DEV_APP_PATH);
  console.log(
    `[handy] Dev bundle ready at ${DEV_APP_PATH} — TeamIdentifier=${summary.teamId ?? "none"}, Authority=${summary.authority ?? "none"}`,
  );
}

async function isViteReachable(): Promise<boolean> {
  try {
    const response = await fetch(DEV_SERVER_URL, {
      signal: AbortSignal.timeout(1000),
    });
    return response.status >= 100;
  } catch {
    return false;
  }
}

async function ensureViteDevServer(): Promise<ChildProcess | null> {
  if (await isViteReachable()) {
    console.log(`[handy] Using existing Vite dev server at ${DEV_SERVER_URL}`);
    return null;
  }

  console.log(`[handy] Starting Vite dev server at ${DEV_SERVER_URL}...`);
  const viteChild = spawn("bun", ["run", "dev"], {
    cwd: REPO_ROOT,
    stdio: "inherit",
  });

  let exitedEarly = false;
  let earlyCode: number | null = null;
  viteChild.once("exit", (code) => {
    exitedEarly = true;
    earlyCode = code;
  });

  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    if (exitedEarly) {
      console.error(
        `[handy] Vite dev server exited early with code ${earlyCode ?? 1}.`,
      );
      process.exit(earlyCode ?? 1);
    }
    if (await isViteReachable()) {
      return viteChild;
    }
    await sleep(200);
  }

  viteChild.kill("SIGTERM");
  console.error(
    `[handy] Timed out waiting for Vite dev server at ${DEV_SERVER_URL}.`,
  );
  process.exit(1);
}

function isHandyRunning(): boolean {
  return spawnSync("pgrep", ["-x", "handy"], { stdio: "ignore" }).status === 0;
}

async function quitRunningHandy(): Promise<void> {
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
    await sleep(250);
  }

  console.log("[handy] Handy did not exit gracefully; terminating process...");
  spawnSync("pkill", ["-x", "handy"], { stdio: "ignore" });
  await sleep(500);
}

async function main(): Promise<void> {
  if (process.platform !== "darwin") {
    console.error("[handy] dev:macos is only supported on macOS.");
    process.exit(1);
  }

  const args = process.argv.slice(2);
  const prepareOnly = args.includes("--prepare-only");
  const noBuild = args.includes("--no-build");

  if (!(noBuild && fs.existsSync(DEBUG_BIN_PATH))) {
    console.log("[handy] Building debug Rust binary...");
    runSyncOrExit("cargo", ["build"], {
      cwd: TAURI_DIR,
      env: {
        ...process.env,
        CMAKE_POLICY_VERSION_MINIMUM:
          process.env.CMAKE_POLICY_VERSION_MINIMUM || "3.5",
      },
      errorMessage: "Failed to build debug Rust binary.",
    });
  } else {
    console.log(`[handy] Reusing existing debug binary at ${DEBUG_BIN_PATH}`);
  }

  assembleAndSignDevBundle();

  if (prepareOnly) {
    return;
  }

  const spawnedVite = await ensureViteDevServer();
  await quitRunningHandy();

  console.log("[handy] Launching signed dev Handy.app...");
  const openChild = spawn("open", ["-n", "-W", DEV_APP_PATH], {
    cwd: REPO_ROOT,
    stdio: "inherit",
  });

  const devBinaryPattern = `${DEV_APP_PATH}/Contents/MacOS/handy`;
  let cleaningUp = false;
  const cleanup = (signal?: NodeJS.Signals) => {
    if (cleaningUp) return;
    cleaningUp = true;

    if (signal) {
      spawnSync("pkill", ["-f", devBinaryPattern], { stdio: "ignore" });
      if (!openChild.killed) {
        openChild.kill("SIGTERM");
      }
    }

    if (spawnedVite && !spawnedVite.killed) {
      spawnedVite.kill("SIGTERM");
    }
  };

  process.once("SIGINT", () => cleanup("SIGINT"));
  process.once("SIGTERM", () => cleanup("SIGTERM"));

  const exitCode = await new Promise<number>((resolve) => {
    openChild.once("error", (err) => {
      console.error(`[handy] Failed to launch dev Handy.app: ${err.message}`);
      resolve(1);
    });
    openChild.once("exit", (code) => {
      resolve(code ?? 0);
    });
  });

  cleanup();
  process.exit(exitCode);
}

if ((import.meta as ImportMeta & { main?: boolean }).main) {
  void main();
}
