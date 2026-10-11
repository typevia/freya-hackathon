# Freya Builder

Rust CLI that reads a case folder, generates a Freya workflow, tests it, and repairs failures. `mock/` contains the Cloudflare Worker used by the generated agent.

## Setup

Requires Rust, Python 3, and Node.js 22+ for the mock.

```sh
cp .env.example .env
# Fill in your API keys, then:
set -a; . ./.env; set +a
export CARGO_TARGET_DIR=target-builder
cargo build --locked
```

## Build an agent

```sh
cargo run --locked -- build-folder \
  --case-folder /path/to/case \
  --mock-url https://your-mock.your-subdomain.workers.dev \
  --output output/final --max-iterations 5 --budget-seconds 1800 --tests-limit 2
```

The case folder is input—not embedded policy. Generated workflows, test results, and traces go under `output/final/`. Text PDFs are handled automatically. Model-generated code is validated before deployment.

To serve the legacy JSON build API locally: `cargo run -- serve`. It exposes `POST /builds` and `GET /status` on `127.0.0.1:8787`.

## Mock deployment

Set the Cloudflare variables in `.env`, then:

```sh
cd mock
npm ci
npm test
npx wrangler deploy
```

Keep the deployed Worker running through judging. The current mock URL is recorded in `submission/submission.json`.

## Test and submit

```sh
cargo test --locked
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

