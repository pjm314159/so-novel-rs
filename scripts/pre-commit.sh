#!/usr/bin/env bash
# 本地提交前快速拦截（与 CI 保持一致，本地通过 = CI 通过）
# 用法：bash scripts/pre-commit.sh（或经 .pre-commit-config.yaml 安装）
set -euo pipefail

echo "[1/4] cargo fmt --check"
cargo fmt --all -- --check

echo "[2/4] cargo clippy"
cargo clippy --all-targets --all-features -- -D warnings

echo "[3/4] cargo test"
cargo test

echo "[4/4] 大文件检查"
# 禁止提交 >500KB 的非文本文件（tests/fixtures 除外）
git diff --cached --name-only 2>/dev/null | while read -r f; do
  if [ -f "$f" ]; then
    size=$(stat -c%s "$f" 2>/dev/null || stat -f%z "$f")
    if [ "$size" -gt 512000 ] && [[ "$f" != tests/fixtures/* ]]; then
      echo "ERROR: $f (${size}B) 超过 500KB，如确需提交请放行" >&2
      exit 1
    fi
  fi
done

echo "pre-commit: all checks passed"
