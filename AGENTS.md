## Cisco IOL Docker Build
This project builds the Cisco IOL container images for the Sherpa project, and nothing else.

- Never commit Cisco images, binaries, licence files or anything extracted from them. Check
  `git status` before every commit.
- The image is built FROM the user's CML image and keeps Cisco's `/iol-runner` launcher.
- `bridge/` is a Rust program with no dependencies. Before committing, in `bridge/`, run
  `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test`.
- Run `shellcheck` on every shell script.
