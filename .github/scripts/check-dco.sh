#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Checks the Developer Certificate of Origin rules from CONTRIBUTING.md for every commit in
# a range (argument: "BASE..HEAD"):
#
#   - each commit has a "Signed-off-by:" line with the commit author's e-mail address,
#   - no "Signed-off-by:" line belongs to an AI model (only humans can certify the DCO).
#
# Merge commits are skipped. If BASE is all zeros (a newly pushed branch), only HEAD is checked.

set -euo pipefail

range="${1:?usage: check-dco.sh BASE..HEAD}"
base="${range%%..*}"
head="${range##*..}"
if [[ "$base" =~ ^0+$ ]]; then
    commits=$(git rev-list --no-merges --max-count=1 "$head")
else
    commits=$(git rev-list --no-merges "$base..$head")
fi

failed=0
for commit in $commits; do
    subject=$(git log -1 --format='%h %s' "$commit")
    author_email=$(git log -1 --format='%ae' "$commit")
    signoffs=$(git log -1 --format='%(trailers:key=Signed-off-by,valueonly)' "$commit")

    if [[ -z "$signoffs" ]]; then
        echo "::error::$subject: missing Signed-off-by (commit with 'git commit -s')"
        failed=1
        continue
    fi
    if ! grep -qiF "<$author_email>" <<<"$signoffs"; then
        echo "::error::$subject: no Signed-off-by for the author <$author_email>"
        failed=1
    fi
    if grep -qiE 'claude|anthropic\.com' <<<"$signoffs"; then
        echo "::error::$subject: an AI model must not sign off; use an Assisted-by: trailer"
        failed=1
    fi
done

if [[ "$failed" -ne 0 ]]; then
    exit 1
fi
echo "DCO check passed for $(wc -w <<<"$commits") commit(s)"
