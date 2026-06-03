#!/usr/bin/env bash
# Pre-commit guard: block newly-added AACS VUK values.
#
# A VUK (Volume Unique Key) is a 16-byte AES key, written as 32 hex
# characters (optionally 0x-prefixed). VUKs are decryption keys and must
# never be committed.
#
# Scans only newly added/changed lines. Allowed:
#   - the all-zero placeholder (00000...0)
#   - any added line containing the marker:  vuk-allow
set -euo pipefail

# 32 hex chars as a standalone token (not part of a 40- or 64-char hash),
# optionally 0x-prefixed.
pattern='(?<![0-9a-fA-F])(?:0x)?[0-9a-fA-F]{32}(?![0-9a-fA-F])'
zero='^(?:0x)?0{32}$'
rc=0

while IFS= read -r f; do
    [ -z "$f" ] && continue
    [ "$f" = "test_discs.toml" ] && continue
    added=$(git diff --cached --unified=0 --no-color -- "$f" \
        | grep '^+' | grep -v '^+++' | sed 's/^+//' \
        | grep -v 'vuk-allow' || true)
    [ -z "$added" ] && continue
    tokens=$(printf '%s\n' "$added" | grep -aoP "$pattern" \
        | grep -vP "$zero" | sort -u || true)
    if [ -n "$tokens" ]; then
        echo "✗ $f — newly added value(s) look like AACS VUKs:"
        printf '%s\n' "$tokens" | sed 's/^/      /'
        rc=1
    fi
done < <(git diff --cached --name-only --diff-filter=ACM)

if [ "$rc" -ne 0 ]; then
    cat >&2 <<'MSG'

VUKs are AACS decryption keys and must not be committed.
  - Pass real keys at runtime (--vuk) or via a gitignored file; never
    hardcode them in source.
  - For a synthetic test vector, add the marker  vuk-allow  to the line.
MSG
fi
exit $rc
