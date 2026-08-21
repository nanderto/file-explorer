#!/usr/bin/env bash
# One-time GitHub branch-protection setup for main:
#   - require the "CI" status check (the aggregate job in .github/workflows/ci.yml)
#   - require branches to be up to date before merging (strict: true)
#   - block force pushes and deletions
#
# Prerequisites: repo pushed to GitHub, `gh auth login` done, and you have admin
# rights on the repo. Run again any time to re-apply.
#
# Usage: bash scripts/setup-branch-protection.sh [owner/repo] [branch]
set -euo pipefail

REPO="${1:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}"
BRANCH="${2:-main}"

echo "Applying branch protection to $REPO ($BRANCH)..."
gh api -X PUT "repos/$REPO/branches/$BRANCH/protection" --input - <<'JSON'
{
  "required_status_checks": {
    "strict": true,
    "contexts": ["CI"]
  },
  "enforce_admins": false,
  "required_pull_request_reviews": null,
  "restrictions": null,
  "allow_force_pushes": false,
  "allow_deletions": false,
  "required_conversation_resolution": true
}
JSON

echo "Done. Verify at: https://github.com/$REPO/settings/branches"
echo "Note: \"strict\": true is the API name for 'Require branches to be up to date before merging'."
