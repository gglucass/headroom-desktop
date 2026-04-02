set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

cd "${REPO_ROOT}"

echo "Running frontend coverage..."
npm run test:coverage

echo "Running desktop tests..."
npm run test:desktop

echo "Release checks passed."
