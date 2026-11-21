import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, "..");

/**
 * Extracts the first `Developer ID Application` signing identity from
 * `security find-identity -v -p codesigning` output.
 */
export function parseSigningIdentity(securityOutput: string): string | null {
  for (const line of securityOutput.split(/\r?\n/)) {
    const match = line.match(/"(Developer ID Application:[^"]+)"/);
    if (match?.[1]) {
      return match[1];
    }
  }
  return null;
}

const HELP_OR_VERSION_FLAGS = new Set(["-h", "--help", "-V", "--version"]);
const DISABLE_UPDATER_ARTIFACTS_CONFIG =
  '{"bundle":{"createUpdaterArtifacts":false}}';

function argsBeforeSeparator(args: string[]): string[] {
  const separatorIndex = args.indexOf("--");
  return separatorIndex === -1 ? args : args.slice(0, separatorIndex);
}

/**
 * Returns the index of the `build` or `bundle` subcommand before `--`, or `-1`.
 */
export function findBundleSubcommandIndex(args: string[]): number {
  const cliArgs = argsBeforeSeparator(args);
  const subcommandIdx = cliArgs.findIndex((arg) => !arg.startsWith("-"));
  if (subcommandIdx === -1) {
    return -1;
  }
  const subcommand = cliArgs[subcommandIdx];
  return subcommand === "build" || subcommand === "bundle" ? subcommandIdx : -1;
}

/**
 * Returns true if any help or version flag appears before `--`.
 */
export function hasHelpOrVersionFlag(args: string[]): boolean {
  return argsBeforeSeparator(args).some((arg) =>
    HELP_OR_VERSION_FLAGS.has(arg),
  );
}

/**
 * Returns true when the Tauri CLI invocation is building/bundling an app.
 */
export function isBundleCommand(args: string[]): boolean {
  return findBundleSubcommandIndex(args) !== -1 && !hasHelpOrVersionFlag(args);
}

/**
 * Returns the CLI arguments to pass to Tauri, disabling updater artifact
 * signing for local non-Windows builds when no signing private key is set.
 */
export function buildTauriArgs(
  args: string[],
  env: NodeJS.ProcessEnv = process.env,
  platform: NodeJS.Platform = process.platform,
): string[] {
  const subcommandIdx = findBundleSubcommandIndex(args);
  if (
    subcommandIdx === -1 ||
    hasHelpOrVersionFlag(args) ||
    platform === "win32" ||
    env.CI ||
    env.TAURI_SIGNING_PRIVATE_KEY ||
    env.TAURI_SIGNING_PRIVATE_KEY_PATH
  ) {
    return [...args];
  }

  return [
    ...args.slice(0, subcommandIdx + 1),
    "--config",
    DISABLE_UPDATER_ARTIFACTS_CONFIG,
    ...args.slice(subcommandIdx + 1),
  ];
}

export function resolveMacSigningIdentity(): string | null {
  const result = spawnSync(
    "security",
    ["find-identity", "-v", "-p", "codesigning"],
    { encoding: "utf8" },
  );
  if (result.status !== 0) {
    return null;
  }
  return parseSigningIdentity(result.stdout ?? "");
}

function buildTauriEnv(
  args: string[],
  baseEnv: NodeJS.ProcessEnv = process.env,
): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...baseEnv };

  if (process.platform === "darwin") {
    if (!env.CMAKE_POLICY_VERSION_MINIMUM) {
      env.CMAKE_POLICY_VERSION_MINIMUM = "3.5";
    }

    if (!env.CI && !env.APPLE_SIGNING_IDENTITY && isBundleCommand(args)) {
      const override = env.HANDY_SIGNING_IDENTITY?.trim();
      const identity = override || resolveMacSigningIdentity();
      if (identity) {
        env.APPLE_SIGNING_IDENTITY = identity;
        console.log(`[handy] Using macOS signing identity: ${identity}`);
      }
    }
  }

  return env;
}

function main(): void {
  const args = process.argv.slice(2);
  const tauriBinName = process.platform === "win32" ? "tauri.cmd" : "tauri";
  const tauriBin = path.join(REPO_ROOT, "node_modules", ".bin", tauriBinName);

  if (!fs.existsSync(tauriBin)) {
    console.error(
      `[handy] Tauri CLI binary not found at ${tauriBin}. Run 'bun install' first.`,
    );
    process.exit(1);
  }

  const env = buildTauriEnv(args);
  const tauriArgs = buildTauriArgs(args, env);
  if (tauriArgs.length !== args.length) {
    console.log(
      "[handy] Skipping updater artifact signing (TAURI_SIGNING_PRIVATE_KEY not set)",
    );
  }

  const child = spawn(tauriBin, tauriArgs, {
    cwd: REPO_ROOT,
    env,
    stdio: "inherit",
    shell: process.platform === "win32",
  });

  child.on("error", (err) => {
    console.error(`[handy] Failed to start Tauri CLI: ${err.message}`);
    process.exit(1);
  });

  child.on("exit", (code, signal) => {
    if (signal) {
      process.kill(process.pid, signal);
    }
    process.exit(code ?? 1);
  });
}

if ((import.meta as ImportMeta & { main?: boolean }).main) {
  main();
}
