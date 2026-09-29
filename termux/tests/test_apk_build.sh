#!/usr/bin/env bash
# Builds the connector APK and checks the manifest facts that Android 14+ enforces.
# Skips (exit 0) if the toolchain is absent. Needs ANDROID_JAR or network for the pinned jar.
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
for t in javac aapt apksigner keytool; do command -v $t >/dev/null || { echo "SKIP: $t missing"; exit 0; }; done
T="$(mktemp -d)"; export WEWALLA_HOME="$T/home" OUT="$T/out"
"$HERE/connector-apk/build.sh" >/dev/null
APK="$OUT/wewalla-connector.apk"
X="$(aapt dump xmltree "$APK" AndroidManifest.xml)"
B="$(aapt dump badging "$APK")"
chk() { [[ "$2" == *"$3"* ]] || { echo "FAIL: $1 (missing: $3)"; exit 1; }; echo "ok: $1"; }
chk "package name"      "$B" "name='com.sectrollz.wewalla.connector'"
chk "targetSdk 34"      "$B" "targetSdkVersion:'34'"
chk "fgs type=location" "$X" "foregroundServiceType(0x01010599)=(type 0x11)0x8"
chk "service not exported" "$(sed -n '/E: service/,$p' <<<"$X")" "exported(0x01010010)=(type 0x12)0x0"
chk "INTERNET"          "$B" "android.permission.INTERNET"
chk "NEARBY_WIFI"       "$B" "android.permission.NEARBY_WIFI_DEVICES"
chk "dex in apk"        "$(unzip -l "$APK")" "classes.dex"
apksigner verify "$APK" >/dev/null && echo "ok: signature verifies"
