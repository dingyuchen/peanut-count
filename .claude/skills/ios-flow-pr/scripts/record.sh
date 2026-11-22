#!/bin/zsh
# Records the Peanut Count flow clips on a dedicated simulator.
# Usage: record.sh [output-dir] [test]   (test defaults to testFlow; see Flow.swift for resume points)
# Env:   FLOW_SIM   simulator name (default "PeanutCount Flows"); created if missing, erased on each full run.
#        SKIP_BUILD=1 reuses the last Release build.
set -euo pipefail
skill=${0:A:h:h}
repo=$(git -C "$skill" rev-parse --show-toplevel)
branch=$(git -C "$repo" branch --show-current)
out=${1:-$repo/.flows/${branch//\//-}}
test=${2:-testFlow}
work=$repo/.flows/.harness
sim_name=${FLOW_SIM:-PeanutCount Flows}
app=$repo/apps/ios/DerivedData/Build/Products/Release-iphonesimulator/PeanutCount.app
mkdir -p "$out" "$work"

# Only ever touch the dedicated simulator: never the one the developer is using.
udid=$(xcrun simctl list devices -j | python3 -c "import json,sys; n=sys.argv[1]; print(next((d['udid'] for r in json.load(sys.stdin)['devices'].values() for d in r if d['name']==n and d['isAvailable']), ''))" "$sim_name")
if [[ -z $udid ]]; then
  runtime=$(xcrun simctl list runtimes -j | python3 -c "import json,sys; print([r['identifier'] for r in json.load(sys.stdin)['runtimes'] if r['platform']=='iOS' and r['isAvailable']][-1])")
  udid=$(xcrun simctl create "$sim_name" "iPhone 17" "$runtime")
fi
echo "simulator: $sim_name ($udid)"

if [[ ${SKIP_BUILD:-0} != 1 ]]; then
  [[ -d $repo/apps/ios/ios ]] || (cd "$repo/apps/ios" && npx expo prebuild --platform ios --no-install && cd ios && pod install)
  (cd "$repo/apps/ios/ios" && xcodebuild -workspace PeanutCount.xcworkspace -scheme PeanutCount -configuration Release \
    -sdk iphonesimulator -destination 'generic/platform=iOS Simulator' -derivedDataPath ../DerivedData build \
    > "$work/app-build.log" 2>&1) || { tail -40 "$work/app-build.log"; exit 1; }
fi

if [[ $test == testFlow ]]; then
  rm -rf "$out"/*.mp4(N) "$out/stills" "$out/previews" "$out/contact-sheet.png"
  xcrun simctl shutdown "$udid" 2>/dev/null || true
  xcrun simctl erase "$udid"
fi
xcrun simctl boot "$udid" 2>/dev/null || true
xcrun simctl bootstatus "$udid" -b > /dev/null
xcrun simctl ui "$udid" appearance light
xcrun simctl status_bar "$udid" override --time 9:41 --batteryState charged --batteryLevel 100 --cellularBars 4 --wifiBars 3
xcrun simctl install "$udid" "$app"

# Seed the fixtures into Files > On My iPhone.
data=~/Library/Developer/CoreSimulator/Devices/$udid/data
find_storage() {
  for meta in $data/Containers/Shared/AppGroup/*/.com.apple.mobile_container_manager.metadata.plist(N); do
    if plutil -extract MCMMetadataIdentifier raw "$meta" 2>/dev/null | grep -qx group.com.apple.FileProvider.LocalStorage; then
      echo "${meta:h}/File Provider Storage"; return
    fi
  done
}
# Copy only while the Files provider is running: files that land in its storage before it
# starts are listed by the picker but never handed to the app.
xcrun simctl launch "$udid" com.apple.DocumentsApp > /dev/null
sleep 5
storage=$(find_storage)
[[ -n $storage ]] || { echo "Could not find the Files local storage container"; exit 1; }
mkdir -p "$storage"
rm -f "$storage/sample-statement.pdf" "$storage/z-ambiguous-dates.pdf"
sleep 1
cp "$skill/fixtures/sample-statement.pdf" "$storage/sample-statement.pdf"
cp "$skill/fixtures/ambiguous-dates.pdf" "$storage/z-ambiguous-dates.pdf"
sleep 3
xcrun simctl terminate "$udid" com.apple.DocumentsApp 2>/dev/null || true

# Generate the UI-test runner outside the skill folder and run it with the capture server.
rsync -a --delete "$skill/harness/" "$work/runner/"
(cd "$work/runner" && xcodegen generate --quiet)
run_test() {
  (cd "$work/runner" && xcodebuild test -project FlowUITests.xcodeproj -scheme FlowUITests \
    -destination "platform=iOS Simulator,id=$udid" -parallel-testing-enabled NO -derivedDataPath "$work/Derived" \
    -only-testing:"FlowUITests/Flow/$1" > "$work/$1.log" 2>&1)
}
echo "warming up the document picker"
run_test testWarmUp || { grep -E "error:" "$work/testWarmUp.log" | tail -5; exit 1; }
python3 -u "$skill/scripts/capture.py" "$udid" "$out" > "$work/capture.log" 2>&1 &
capture_pid=$!
trap 'curl -s http://127.0.0.1:18745/stop > /dev/null || true; kill $capture_pid 2>/dev/null || true' EXIT
sleep 1
rc=0
run_test "$test" || rc=$?
curl -s http://127.0.0.1:18745/stop > /dev/null || true
if [[ $rc != 0 ]]; then
  grep -E "error:|failed|Missing" "$work/$test.log" | tail -20
  echo "Flow failed. Full log: $work/$test.log. Resume with: $0 $out testResumeChart|testResumeAmbiguous|testResumeDark"
  exit $rc
fi

swift "$skill/scripts/join-video.swift" "$out"
swift "$skill/scripts/previews.swift" "$out"
echo "clips: $out"
