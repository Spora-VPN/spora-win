#!/bin/sh
# Runs ON the peer node, piped over ssh by `wt peer-share`: (re)starts a
# `spora-cli share --json` as a transient systemd unit and prints its share
# URL once the sharer reports share_ready. Extra args go to `spora-cli share`.
set -eu
D=/root/wintest
U=spora-wintest-share
mkdir -p "$D"
[ -x "$D/spora-cli" ] || { echo "no $D/spora-cli on this node; run 'wt peer-push'" >&2; exit 1; }
systemctl stop "$U" 2>/dev/null || true
systemctl reset-failed "$U" 2>/dev/null || true
rm -f "$D/share.jsonl" "$D/share.err"
systemd-run --quiet --unit "$U" --collect \
  -p WorkingDirectory="$D" -p StandardOutput=append:"$D/share.jsonl" -p StandardError=append:"$D/share.err" \
  "$D/spora-cli" share --json --identity-file "$D/identity.bin" --record-label wintest-peer --record-id wintest "$@"
i=0
while [ $i -lt 30 ]; do
  url=$(grep -o '"url":"[^"]*"' "$D/share.jsonl" 2>/dev/null | head -1 | cut -d'"' -f4 || true)
  if [ -n "$url" ]; then echo "$url"; exit 0; fi
  if ! systemctl is-active --quiet "$U"; then
    echo "share unit exited:" >&2; tail -20 "$D/share.err" >&2; exit 1
  fi
  sleep 1; i=$((i + 1))
done
echo "no share_ready within 30s; stderr so far:" >&2; tail -20 "$D/share.err" >&2; exit 1
