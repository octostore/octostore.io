#!/usr/bin/env bash
set -euo pipefail

cargo fmt --check
cargo fmt --manifest-path fuzz/Cargo.toml -- --check
cargo check --all-targets --all-features --locked
cargo check --manifest-path fuzz/Cargo.toml --all-targets --locked
RUST_TEST_THREADS=1 cargo test --all-targets --all-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
scripts/check-package.sh
# ShellCheck's informational diagnostics have changed between runner images.
# Keep this gate strict for actionable warnings and errors on every platform.
shellcheck -S warning install.sh deploy/deploy.sh scripts/*.sh tests/fixtures/*.sh

python3 -m py_compile scripts/uptime_monitor.py scripts/test-uptime-monitor.py
python3 scripts/test-uptime-monitor.py
go run github.com/rhysd/actionlint/cmd/actionlint@v1.7.12
scripts/check-release-contract.sh
scripts/smoke-downgrade-compatibility.sh
npm ci --ignore-scripts --no-audit --no-fund
./node_modules/.bin/playwright install chromium
npm run lint:openapi
npm run lint:html
npm run test:site:static
npm run test:site:browser
scripts/smoke-skill-install.sh
scripts/smoke-release-fixture.sh
