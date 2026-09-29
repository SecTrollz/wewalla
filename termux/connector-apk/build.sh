#!/usr/bin/env bash
# Build the Wewalla Connector APK with plain command-line tools: no Gradle, no Android Studio.
# Works inside Termux (pkg install openjdk-17 aapt apksigner dx zip curl) and on any Linux box.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WHOME="${WEWALLA_HOME:-$HOME/.wewalla}"
OUT="${OUT:-$HERE/build}"
API_JAR_URL="${API_JAR_URL:-https://raw.githubusercontent.com/Sable/android-platforms/master/android-34/android.jar}"
API_JAR_SHA256="6cea1df3efb77103ac3e2beb9bf4718964b0e0869ab16d39d29d5cbae1c147ad"
MIN_API=26
KS="$WHOME/debug.keystore"
KS_PASS="wewalla-debug"     # local sideload key only; the file never leaves your device

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

mkdir -p "$WHOME" "$OUT"

# ---- 0. tools -------------------------------------------------------------
missing=()
have keytool   || missing+=("openjdk-17 (keytool)")
have apksigner || missing+=("apksigner")
have python3   || missing+=("python")
have javac || have ecj || missing+=("openjdk-17 (javac) or ecj")
have d8 || have dx || have dalvik-exchange || missing+=("dx (or d8)")
have aapt || have aapt2 || missing+=("aapt (or aapt2)")
if ((${#missing[@]})); then
  die "missing tools: ${missing[*]}
  Termux:  pkg install openjdk-17 aapt apksigner dx ecj python
  Debian:  apt install default-jdk-headless aapt apksigner dalvik-exchange python3"
fi

# ---- 1. android.jar (compile-time stubs only; nothing from it ships in the APK) ----
find_jar() {
  local c
  for c in "${ANDROID_JAR:-}" "$WHOME/android.jar" \
           "${PREFIX:-/nonexistent}/share/aapt/android.jar" \
           $(ls -d "${ANDROID_HOME:-/nonexistent}"/platforms/android-*/android.jar 2>/dev/null | sort -V | tail -n1); do
    [[ -n "$c" && -f "$c" ]] && { echo "$c"; return 0; }
  done
  return 1
}
if ! JAR="$(find_jar)"; then
  say "downloading android.jar (pinned SHA-256)"
  have curl || die "need curl to fetch android.jar (pkg install curl) or set ANDROID_JAR=/path/to/android.jar"
  curl -fL --retry 3 -o "$WHOME/android.jar.part" "$API_JAR_URL"
  got="$(sha256sum "$WHOME/android.jar.part" | cut -d' ' -f1)"
  [[ "$got" == "$API_JAR_SHA256" ]] || { rm -f "$WHOME/android.jar.part"; die "android.jar checksum mismatch ($got)"; }
  mv "$WHOME/android.jar.part" "$WHOME/android.jar"
  JAR="$WHOME/android.jar"
fi
# Wi-Fi RTT needs API 28+ symbols at compile time
# (no `unzip | grep -q` here: with pipefail, grep exiting early SIGPIPEs unzip and fails the pipeline)
JAR_LIST="$(unzip -l "$JAR" 2>/dev/null || true)"
[[ "$JAR_LIST" == *android/net/wifi/rtt/WifiRttManager.class* ]] \
  || die "$JAR is too old (needs API 28+). Set ANDROID_JAR to a newer android.jar or delete it to re-download."
say "android.jar: $JAR"

# ---- 2. compile -----------------------------------------------------------
rm -rf "$OUT/classes" "$OUT/dex" "$OUT/apk"; mkdir -p "$OUT/classes" "$OUT/dex" "$OUT/apk"
mapfile -t SRC < <(find "$HERE/src" -name '*.java')
say "compiling ${#SRC[@]} Java files"
if have javac; then
  javac --release 8 -Xlint:-options -cp "$JAR" -d "$OUT/classes" "${SRC[@]}"
else
  ecj -8 -nowarn -cp "$JAR" -d "$OUT/classes" "${SRC[@]}"
fi

# ---- 3. dex ---------------------------------------------------------------
say "dexing"
if have d8; then
  d8 --release --min-api "$MIN_API" --lib "$JAR" --output "$OUT/dex" $(find "$OUT/classes" -name '*.class')
else
  DX=dx; have dx || DX=dalvik-exchange
  "$DX" --dex --output="$OUT/dex/classes.dex" "$OUT/classes"
fi
[[ -f "$OUT/dex/classes.dex" ]] || die "dexing produced no classes.dex"

# ---- 4. package manifest (no resources -> no res compile step) -----------------
say "packaging manifest"
if have aapt; then
  aapt package -f -M "$HERE/AndroidManifest.xml" -I "$JAR" -F "$OUT/apk/base.apk" \
       --min-sdk-version "$MIN_API" --target-sdk-version 34 --version-code 1 --version-name 0.1.0
else
  aapt2 link -o "$OUT/apk/base.apk" -I "$JAR" --manifest "$HERE/AndroidManifest.xml" \
        --min-sdk-version "$MIN_API" --target-sdk-version 34 --version-code 1 --version-name 0.1.0
fi

python3 - "$OUT/apk/base.apk" "$OUT/dex/classes.dex" "$OUT/apk/unsigned.apk" <<'PY'
import sys, zipfile
base, dex, out = sys.argv[1:4]
with zipfile.ZipFile(base) as zin, zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zout:
    for i in zin.infolist():
        zout.writestr(i, zin.read(i.filename))
    zout.write(dex, "classes.dex")
PY

# ---- 5. align + sign -------------------------------------------------------
APK_IN="$OUT/apk/unsigned.apk"
if have zipalign; then
  zipalign -f -p 4 "$APK_IN" "$OUT/apk/aligned.apk" && APK_IN="$OUT/apk/aligned.apk"
else
  say "warning: zipalign not found, APK left unaligned (Debian: apt install zipalign; in Termux it may ship with the aapt package)"
fi
if [[ ! -f "$KS" ]]; then
  say "creating local signing key $KS"
  keytool -genkeypair -keystore "$KS" -storepass "$KS_PASS" -keypass "$KS_PASS" -alias wewalla \
          -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Wewalla local sideload" >/dev/null 2>&1
fi
FINAL="$OUT/wewalla-connector.apk"
say "signing"
apksigner sign --ks "$KS" --ks-pass "pass:$KS_PASS" --key-pass "pass:$KS_PASS" --ks-key-alias wewalla \
          --min-sdk-version "$MIN_API" --out "$FINAL" "$APK_IN"
apksigner verify "$FINAL" >/dev/null && say "signature verified"

cp "$FINAL" "$WHOME/wewalla-connector.apk"
say "built: $FINAL ($(wc -c <"$FINAL") bytes); copy also at $WHOME/wewalla-connector.apk"
echo "next: wewalla connector install"
