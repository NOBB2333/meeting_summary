#!/usr/bin/env bash
# Create disposable, synthetic media and a legacy-format meeting; no user data.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output="${1:-$root/target/upgrade-fixtures}"
if [[ -e "$output" || -L "$output" ]]; then
  echo "Fixture destination already exists; choose a new empty path: $output" >&2
  exit 1
fi
mkdir -p "$output/legacy"
ffmpeg -nostdin -hide_banner -loglevel error -n \
  -f lavfi -i 'sine=frequency=440:sample_rate=48000:duration=2' \
  -f lavfi -i 'anullsrc=r=48000:cl=mono:d=0.8' \
  -f lavfi -i 'sine=frequency=660:sample_rate=48000:duration=2' \
  -filter_complex '[0:a][1:a][2:a]concat=n=3:v=0:a=1[a]' -map '[a]' \
  -c:a pcm_s16le "$output/synthetic.wav"
ffmpeg -nostdin -hide_banner -loglevel error -n -i "$output/synthetic.wav" \
  -c:a aac "$output/synthetic.m4a"
ffmpeg -nostdin -hide_banner -loglevel error -n \
  -f lavfi -i 'testsrc2=size=320x180:rate=15' -i "$output/synthetic.wav" \
  -shortest -c:v mpeg4 -c:a aac "$output/synthetic.mp4"
cp "$output/synthetic.m4a" "$output/legacy/Legacy.m4a"
cat > "$output/legacy/Legacy.transcript.json" <<'JSON'
{
  "source": null,
  "model": "legacy-unrecorded-model",
  "complete": true,
  "cancelled": false,
  "gaps": [],
  "segments": [
    {"id": 1, "revision": 1, "start_ms": 0, "end_ms": 2000,
     "text": "合成旧会议片段，音频不是语音。", "finalized": true},
    {"id": 2, "revision": 1, "start_ms": 2800, "end_ms": 4800,
     "text": "静音间隔后的第二段。", "finalized": true}
  ],
  "legacy_extension": {"must_survive": true}
}
JSON
cat > "$output/legacy/Legacy.meeting.json" <<'JSON'
{"title":"旧会议副本","legacy_extension":{"must_survive":true}}
JSON
cat > "$output/legacy/Legacy.summary.history.json" <<'JSON'
{"versions":[{"number":1,"created_unix":0,"instruction":"旧版本","markdown":"# 旧纪要\n"}],"conversation":[]}
JSON
printf '# 旧纪要\n' > "$output/legacy/Legacy.summary.md"
printf '测试笔记\n' > "$output/legacy/Legacy.notes.txt"
printf 'Created disposable fixtures in %s\n' "$output"
