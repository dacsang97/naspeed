//! Shared pieces of the GitHub Actions workflows written by `ns ci init`.
//!
//! The text is assembled from fixed snippets with `@NAME@` markers (GitHub's
//! own `${{ }}` syntax would fight Rust's `format!` braces). Only values
//! derived from the project (names, registry host, counts) are substituted,
//! and the caller validates them to contain no YAML or shell metacharacters.

/// Releases of naspeed the workflows download: `<base>/<archive>` and
/// `<base>/SHA256SUMS`, as produced by `ns release` on naspeed itself.
/// Generated workflows need a published GitHub Release `v${NS_VERSION}` there.
pub const NASPEED_RELEASE_REPO: &str = "dacsang97/naspeed";
pub const NASPEED_DOWNLOAD_URL: &str =
    "https://github.com/${NS_RELEASE_REPO}/releases/download/v${NS_VERSION}";

/// Pinned action majors (checked against each repo's latest release with
/// `gh api repos/<owner>/<repo>/releases/latest`).
pub const CHECKOUT: &str = "actions/checkout@v7";
pub const CACHE_RESTORE: &str = "actions/cache/restore@v6";
pub const CACHE_SAVE: &str = "actions/cache/save@v6";
pub const COSIGN_INSTALLER: &str = "sigstore/cosign-installer@v4";
pub const UPLOAD_ARTIFACT: &str = "actions/upload-artifact@v7";
pub const DOWNLOAD_ARTIFACT: &str = "actions/download-artifact@v8";
pub const DOCKER_LOGIN: &str = "docker/login-action@v4";

/// Where the exported store travels between runs (`actions/cache` path).
pub const CI_CACHE_DIR: &str = "~/.naspeed-ci";

/// Export caps. GitHub's default cache budget is 10 GB per repository and it
/// evicts least recently used entries beyond that. One push to main saves
/// three entries (test cache for Linux and macOS, release cache for macOS):
/// 3 + 3 + 4 = 10 GB, so the newest generation always fits and older ones
/// are what gets evicted. The release cache gets more room because it holds
/// optimized builds for every target and build variant.
pub const TEST_EXPORT_MAX_GB: u32 = 3;
pub const RELEASE_EXPORT_MAX_GB: u32 = 4;

/// The only condition under which a workflow may write the cache.
pub const SAVE_GUARD: &str = "github.event_name == 'push' && github.ref == 'refs/heads/main'";

/// Cache families: tests (debug + clippy artifacts, per OS) and release
/// (optimized cross builds, written by ns-build, read by ns-release).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    Test,
    Release,
}

impl CacheKind {
    pub fn prefix(self) -> &'static str {
        match self {
            CacheKind::Test => "ns-test",
            CacheKind::Release => "ns-release",
        }
    }
}

/// `actions/cache` key prefix: OS, arch and compiler keep incompatible
/// artifacts apart; `Cargo.lock` narrows to the same dependency set.
fn key(kind: CacheKind) -> String {
    format!(
        "{}-${{{{ runner.os }}}}-${{{{ runner.arch }}}}-${{{{ steps.toolchain.outputs.rustc }}}}-${{{{ hashFiles('**/Cargo.lock') }}}}",
        kind.prefix()
    )
}

/// What a job needs from the shared setup steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// Tests and lint: read-only checkout, clippy + rustfmt, test cache.
    Test,
    /// Snapshot release build: full history, cross tools, release cache.
    Snapshot,
    /// Tagged release: full history with credentials (tag check against the
    /// remote), cross tools, release cache.
    Release,
    /// Image push from a downloaded dist dir: no cache, no cross tools.
    Image,
}

/// Project facts the setup steps depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toolchain {
    /// `rust-toolchain.toml` / `rust-toolchain` exists at the workspace root.
    pub file: bool,
    /// The release matrix has targets linked through zig.
    pub zig: bool,
    /// The release matrix has `x86_64-pc-windows-msvc` (xwin + LLVM).
    pub msvc: bool,
}

const CHECKOUT_STEP: &str = r#"      - uses: @CHECKOUT@
        with:
@CHECKOUT_WITH@
      - name: Install Rust toolchain
        id: toolchain
        shell: bash
        run: |
          @TOOLCHAIN_INSTALL@
          echo "rustc=$(rustc --version | cut -d' ' -f2)" >> "$GITHUB_OUTPUT"
"#;

const INSTALL_NASPEED_STEP: &str = r#"      - name: Install naspeed ${{ env.NS_VERSION }}
        shell: bash
        run: |
          case "${RUNNER_OS}-${RUNNER_ARCH}" in
            Linux-X64) target=x86_64-unknown-linux-musl ;;
            Linux-ARM64) target=aarch64-unknown-linux-musl ;;
            macOS-*) target=universal2-apple-darwin ;;
            *) echo "::error::no naspeed build for ${RUNNER_OS}-${RUNNER_ARCH}"; exit 1 ;;
          esac
          archive="naspeed-${NS_VERSION}-${target}.tar.gz"
          base="@DOWNLOAD_URL@"
          dir="${RUNNER_TEMP}/naspeed"
          mkdir -p "$dir"
          cd "$dir"
          curl --proto '=https' --tlsv1.2 -fsSL -o "$archive" "${base}/${archive}"
          curl --proto '=https' --tlsv1.2 -fsSL -o SHA256SUMS "${base}/SHA256SUMS"
          grep "  ${archive}\$" SHA256SUMS | shasum -a 256 -c -
          tar -xzf "$archive"
          bin="$(find "$dir" -type f -name naspeed -perm -u+x | head -n 1)"
          test -n "$bin"
@MSVC_COMMENT@          "$bin" setup --store ~/.naspeed@SETUP_FLAGS@
          naspeed --version
"#;

const RESTORE_STEPS: &str = r#"      - name: Restore naspeed store
        uses: @CACHE_RESTORE@
        with:
          path: @CACHE_DIR@
          key: @KEY@-${{ github.run_id }}
          restore-keys: |
            @KEY@-
            @KIND@-${{ runner.os }}-${{ runner.arch }}-${{ steps.toolchain.outputs.rustc }}-
      - name: Import naspeed store
        shell: bash
        run: |
          if [ -d @CACHE_DIR@/entries ]; then ns cache import @CACHE_DIR@; fi
"#;

const SAVE_STEPS: &str = r#"      # Only pushes to main write the cache (see the security model above).
      - name: Export naspeed store
        if: @SAVE_GUARD@
        run: ns cache export @CACHE_DIR@ --max-gb @MAX_GB@
      - name: Save naspeed store
        if: @SAVE_GUARD@
        uses: @CACHE_SAVE@
        with:
          path: @CACHE_DIR@
          # Cache keys are immutable: the run id makes every save a new entry;
          # restore-keys pick the newest one with the same prefix.
          key: @KEY@-${{ github.run_id }}
"#;

const MSVC_COMMENT: &str = "          # x86_64-pc-windows-msvc needs the Microsoft CRT/SDK (downloaded by xwin).\n          # After reviewing https://go.microsoft.com/fwlink/?LinkId=2086102, append\n          # --accept-msvc-license to the setup line below.\n";

/// Checkout, toolchain, cross tools, naspeed and (except for image jobs)
/// cache restore + import. Never writes the cache.
pub fn setup_steps(job: Job, tc: Toolchain) -> String {
    let checkout_with = match job {
        // Tests and image pushes never need the token after checkout.
        Job::Test | Job::Image => "          persist-credentials: false",
        // The snapshot version comes from the latest tag; nothing is pushed.
        Job::Snapshot => "          fetch-depth: 0\n          persist-credentials: false",
        // The changelog needs every commit since the previous tag, and the
        // tag check talks to the remote with the checkout credentials.
        Job::Release => "          fetch-depth: 0",
    };
    let toolchain_install = match (tc.file, job == Job::Test) {
        // No argument: rustup installs what rust-toolchain.toml pins.
        (true, true) => "rustup toolchain install && rustup component add clippy rustfmt",
        (true, false) => "rustup toolchain install",
        (false, true) => {
            "rustup toolchain install stable --profile minimal --component clippy,rustfmt && rustup default stable"
        }
        (false, false) => {
            "rustup toolchain install stable --profile minimal && rustup default stable"
        }
    };
    let cross = matches!(job, Job::Snapshot | Job::Release);
    let mut out = CHECKOUT_STEP
        .replace("@CHECKOUT_WITH@", checkout_with)
        .replace("@TOOLCHAIN_INSTALL@", toolchain_install);
    if cross {
        out.push_str(&cross_tools_step(tc));
    }
    // `setup --cross` writes zig/msvc linker shims; `--install-targets` adds
    // the rustup targets, including x86_64-apple-darwin for universal2.
    let (flags, msvc_comment) = match (cross, tc.msvc) {
        (true, true) => (" --cross --install-targets", MSVC_COMMENT),
        (true, false) => (" --cross --install-targets", ""),
        _ => ("", ""),
    };
    out.push_str(
        &INSTALL_NASPEED_STEP
            .replace("@MSVC_COMMENT@", msvc_comment)
            .replace("@SETUP_FLAGS@", flags),
    );
    let kind = match job {
        Job::Test => Some(CacheKind::Test),
        Job::Snapshot | Job::Release => Some(CacheKind::Release),
        Job::Image => None,
    };
    if let Some(kind) = kind {
        out.push_str(
            &RESTORE_STEPS
                .replace("@KEY@", &key(kind))
                .replace("@KIND@", kind.prefix()),
        );
    }
    out
}

/// Homebrew packages for cross linking on the macOS release runner: zig for
/// Linux/windows-gnu targets, LLVM + lld + xwin for msvc.
fn cross_tools_step(tc: Toolchain) -> String {
    let mut pkgs = Vec::new();
    if tc.zig {
        pkgs.push("zig");
    }
    if tc.msvc {
        pkgs.extend(["llvm", "lld", "xwin"]);
    }
    if pkgs.is_empty() {
        return String::new();
    }
    format!(
        "      # Installed before naspeed so `setup --cross` finds them.\n      - name: Install cross linkers\n        run: brew install {}\n",
        pkgs.join(" ")
    )
}

/// Export + save, guarded so only pushes to main write the cache.
pub fn save_steps(kind: CacheKind) -> String {
    let max_gb = match kind {
        CacheKind::Test => TEST_EXPORT_MAX_GB,
        CacheKind::Release => RELEASE_EXPORT_MAX_GB,
    };
    SAVE_STEPS
        .replace("@KEY@", &key(kind))
        .replace("@MAX_GB@", &max_gb.to_string())
}

/// The `env:` block every workflow shares.
pub const WORKFLOW_ENV: &str = r#"env:
  CARGO_TERM_COLOR: always
  # naspeed release used by this workflow (needs a published GitHub Release).
  NS_VERSION: "@NS_VERSION@"
  NS_RELEASE_REPO: @RELEASE_REPO@
"#;

/// Replace the markers shared by every snippet.
pub fn fill(text: &str, ns_version: &str) -> String {
    text.replace("@NS_VERSION@", ns_version)
        .replace("@RELEASE_REPO@", NASPEED_RELEASE_REPO)
        .replace("@DOWNLOAD_URL@", NASPEED_DOWNLOAD_URL)
        .replace("@CHECKOUT@", CHECKOUT)
        .replace("@CACHE_RESTORE@", CACHE_RESTORE)
        .replace("@CACHE_SAVE@", CACHE_SAVE)
        .replace("@COSIGN_INSTALLER@", COSIGN_INSTALLER)
        .replace("@UPLOAD_ARTIFACT@", UPLOAD_ARTIFACT)
        .replace("@DOWNLOAD_ARTIFACT@", DOWNLOAD_ARTIFACT)
        .replace("@DOCKER_LOGIN@", DOCKER_LOGIN)
        .replace("@CACHE_DIR@", CI_CACHE_DIR)
        .replace("@SAVE_GUARD@", SAVE_GUARD)
}

/// First `@UPPERCASE` marker left in `text`, if any (template self-check).
pub fn unreplaced_marker(text: &str) -> Option<&str> {
    text.split('@')
        .skip(1)
        .find(|rest| rest.starts_with(|c: char| c.is_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TC: Toolchain = Toolchain {
        file: false,
        zig: true,
        msvc: true,
    };

    #[test]
    fn test_jobs_get_lint_components_and_no_cross_tools() {
        let text = fill(&setup_steps(Job::Test, TC), "0.1.0");
        assert!(unreplaced_marker(&text).is_none(), "{text}");
        assert!(text.contains("--component clippy,rustfmt"));
        assert!(text.contains("persist-credentials: false"));
        assert!(!text.contains("brew install"));
        assert!(!text.contains("--cross"));
        assert!(text.contains("ns-test-${{ runner.os }}"));
        assert!(!text.contains(CACHE_SAVE));
    }

    #[test]
    fn release_jobs_install_cross_tools_and_flag_the_msvc_license() {
        let text = fill(&setup_steps(Job::Release, TC), "0.1.0");
        assert!(text.contains("brew install zig llvm lld xwin"));
        assert!(text.contains("setup --store ~/.naspeed --cross --install-targets\n"));
        assert!(text.contains("append\n          # --accept-msvc-license"));
        assert!(text.contains("fetch-depth: 0\n"));
        assert!(text.contains("ns-release-${{ runner.os }}"));
        let no_msvc = setup_steps(Job::Snapshot, Toolchain { msvc: false, ..TC });
        assert!(no_msvc.contains("brew install zig\n"));
        assert!(!no_msvc.contains("accept-msvc-license"));
    }

    #[test]
    fn image_jobs_skip_the_cache() {
        let text = setup_steps(Job::Image, TC);
        assert!(!text.contains("cache/restore"));
        assert!(!text.contains("cache import"));
    }

    #[test]
    fn save_steps_are_guarded_and_capped() {
        let text = fill(&save_steps(CacheKind::Release), "0.1.0");
        assert_eq!(text.matches(&format!("if: {SAVE_GUARD}")).count(), 2);
        assert!(text.contains("--max-gb 4"));
        assert!(save_steps(CacheKind::Test).contains("--max-gb 3"));
    }
}
