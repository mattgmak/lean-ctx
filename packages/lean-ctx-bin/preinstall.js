#!/usr/bin/env node
"use strict";

// Stop running lean-ctx processes before npm replaces the binary.
// On Windows, a running .exe cannot be deleted — this prevents EBUSY / orphan files.

const { execFileSync, execSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const IS_WIN = process.platform === "win32";
const BIN_DIR = path.join(__dirname, "bin");
const BINARY_NAME = IS_WIN ? "lean-ctx.exe" : "lean-ctx";
const BINARY_PATH = path.join(BIN_DIR, BINARY_NAME);

function cleanOrphans() {
  if (!fs.existsSync(BIN_DIR)) return;
  try {
    const entries = fs.readdirSync(BIN_DIR);
    for (const entry of entries) {
      if (entry.endsWith(".old") || entry.endsWith(".old.exe") || entry.endsWith(".tmp")) {
        try {
          fs.unlinkSync(path.join(BIN_DIR, entry));
        } catch {}
      }
    }
  } catch {}
}

function stopViaLeanCtx() {
  if (!fs.existsSync(BINARY_PATH)) return false;
  try {
    execFileSync(BINARY_PATH, ["stop"], { stdio: "ignore", timeout: 10000 });
    return true;
  } catch {
    return false;
  }
}

function stopViaTaskkill() {
  try {
    execSync('taskkill /F /IM "lean-ctx.exe" /T', { stdio: "ignore", timeout: 5000 });
    return true;
  } catch {
    return false;
  }
}

function stopViaPkill() {
  try {
    execSync("pkill -f lean-ctx", { stdio: "ignore", timeout: 5000 });
    return true;
  } catch {
    return false;
  }
}

if (IS_WIN) {
  stopViaLeanCtx();
  // taskkill fallback catches processes missed by `lean-ctx stop`
  stopViaTaskkill();
  // Brief pause so Windows releases file handles
  try { execSync("timeout /T 1 /NOBREAK >NUL 2>&1", { stdio: "ignore" }); } catch {}
} else {
  stopViaLeanCtx();
  stopViaPkill();
}

cleanOrphans();
