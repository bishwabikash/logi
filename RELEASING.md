# Releasing

Releases are built and published by `.github/workflows/release.yml`. Nothing is
uploaded by hand.

1. Bump `version` in `Cargo.toml` and commit it. `Cargo.lock` follows when
   `cargo build` next runs.
2. Tag that commit and push the tag:

   ```
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. The workflow runs the tests, builds a release binary, and publishes a GitHub
   release with `logi-<version>-x86_64-linux.tar.gz` and its `.sha256`. The
   release notes are generated from the commits since the last tag.

The workflow refuses a tag that does not match the version in `Cargo.toml`
(`v0.2.0` over `0.1.0` fails before anything is published), so a mistake costs
a deleted tag, not a mislabelled release.

To check the packaging without a tag, run the workflow by hand from the Actions
tab ("Run workflow"). It does everything except publish and leaves the tarball
as a downloadable artifact.

The binary is built on Ubuntu 22.04 so that it runs on distributions with an
older glibc. If GitHub retires that runner image, change `runs-on` in both jobs
of `release.yml`.
