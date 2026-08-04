#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target="$repo/src-tauri/src/main.rs"
backup=$(mktemp)
cp "$target" "$backup"
restore() {
  cp "$backup" "$target"
  rm -f "$backup"
}
trap restore EXIT INT TERM

python3 - "$target" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text()
needle = '"edit_file" => execute_edit_file_tool'
count = text.count(needle)
if count != 1:
    raise SystemExit(f"expected one local dispatch arm, found {count}")
path.write_text(text.replace(needle, '"edit_file_mutated" => execute_edit_file_tool', 1))
PY

set +e
cargo test --manifest-path "$repo/src-tauri/Cargo.toml" local_executor_ask_user_is_not_unavailable
status=$?
set -e
if [ "$status" -eq 0 ]; then
  echo "mutation_result=unexpected_pass"
  exit 1
fi
echo "mutation_result=expected_failure status=$status"
