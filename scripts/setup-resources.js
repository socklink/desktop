import fs from 'fs';
import crypto from 'crypto';
import path from 'path';
import { execSync } from 'child_process';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

const targetDir = path.resolve(__dirname, '../src-tauri/resources');
const targetFile = path.join(targetDir, 'obs-virtualcam-module64.dll');

// Ensure target directory exists
if (!fs.existsSync(targetDir)) {
  fs.mkdirSync(targetDir, { recursive: true });
}

// On non-Windows platforms, the Windows DLL is not needed at runtime (Linux uses v4l2loopback).
// A dummy placeholder is sufficient to satisfy Tauri's bundle resource check.
if (process.platform !== 'win32') {
  if (!fs.existsSync(targetFile)) {
    fs.writeFileSync(targetFile, '');
    console.log(`[setup-resources] Non-Windows platform detected (${process.platform}). Created placeholder at ${targetFile}`);
  }
  process.exit(0);
}

if (fs.existsSync(targetFile) && fs.statSync(targetFile).size > 1000) {
  console.log(`[setup-resources] obs-virtualcam-module64.dll already exists at ${targetFile}`);
  process.exit(0);
}

console.log('[setup-resources] obs-virtualcam-module64.dll is missing.');
console.log('[setup-resources] Downloading OBS Studio ZIP to extract the virtual camera DLL...');

const tempZip = path.join(__dirname, 'obs-temp.zip');
const url = 'https://github.com/obsproject/obs-studio/releases/download/30.1.2/OBS-Studio-30.1.2.zip';
// Pinned SHA-256 of the release asset (size 147393388). Update both together when
// bumping the OBS version.
const expectedSha256 = '35be787de3169413eb5e6b6dc8b493c3f1973dd3f8366144b54575f897906dc0';
const dllZipPath = 'data/obs-plugins/win-dshow/obs-virtualcam-module64.dll';

try {
  // Download the ZIP file using curl
  console.log(`[setup-resources] Downloading from: ${url}`);
  execSync(`curl -fL "${url}" -o "${tempZip}"`, { stdio: 'inherit' });

  // Refuse to use anything that is not the exact artifact we pinned.
  const actualSha256 = crypto.createHash('sha256').update(fs.readFileSync(tempZip)).digest('hex');
  if (actualSha256 !== expectedSha256) {
    throw new Error(`Checksum mismatch for OBS download: expected ${expectedSha256}, got ${actualSha256}`);
  }
  console.log('[setup-resources] Checksum verified.');

  // Extract the specific DLL using tar
  console.log(`[setup-resources] Extracting ${dllZipPath}...`);
  execSync(`tar -xf "${tempZip}" "${dllZipPath}"`, { cwd: __dirname });

  // Move the DLL to the destination
  const extractedDll = path.join(__dirname, 'data/obs-plugins/win-dshow/obs-virtualcam-module64.dll');
  if (fs.existsSync(extractedDll)) {
    fs.renameSync(extractedDll, targetFile);
    console.log(`[setup-resources] Successfully installed DLL to ${targetFile}`);
  } else {
    throw new Error('Extracted DLL file not found. Extraction might have failed.');
  }
} catch (error) {
  console.error('[setup-resources] Error occurred during setup:', error.message);
  process.exit(1);
} finally {
  // Clean up files
  if (fs.existsSync(tempZip)) {
    try {
      fs.unlinkSync(tempZip);
    } catch (e) {
      console.warn('[setup-resources] Failed to delete temporary ZIP:', e.message);
    }
  }
  const extractedDataDir = path.join(__dirname, 'data');
  if (fs.existsSync(extractedDataDir)) {
    try {
      fs.rmSync(extractedDataDir, { recursive: true, force: true });
    } catch (e) {
      console.warn('[setup-resources] Failed to delete extracted data directory:', e.message);
    }
  }
}
