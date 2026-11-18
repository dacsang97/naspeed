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

const GO_PRIVATE_STEP: &str = r#"      - name: Configure private Go modules
        shell: bash
        env:
          PRIVATE_PATTERNS: ${{ vars.GOPRIVATE }}
          PRIVATE_NOSUMDB: ${{ vars.GONOSUMDB }}
          PRIVATE_PROXY: ${{ vars.GOPROXY }}
          GIT_TOKEN: ${{ secrets.GO_PRIVATE_TOKEN }}
          PROXY_USERNAME: ${{ secrets.GO_PROXY_USERNAME }}
          PROXY_PASSWORD: ${{ secrets.GO_PROXY_PASSWORD }}
        run: |
          # Process environments cannot represent NUL. Reject the remaining
          # line/protocol delimiters before writing any supplied value.
          reject_line_breaks() {
            case "$2" in
              *$'\r'*|*$'\n'*) echo "::error::$1 must not contain CR or LF"; exit 1 ;;
            esac
          }
          reject_line_breaks GOPRIVATE "$PRIVATE_PATTERNS"
          reject_line_breaks GONOSUMDB "$PRIVATE_NOSUMDB"
          reject_line_breaks GOPROXY "$PRIVATE_PROXY"
          reject_line_breaks GO_PRIVATE_TOKEN "$GIT_TOKEN"
          reject_line_breaks GO_PROXY_USERNAME "$PROXY_USERNAME"
          reject_line_breaks GO_PROXY_PASSWORD "$PROXY_PASSWORD"
          case "$PROXY_USERNAME$PROXY_PASSWORD" in
            *[[:space:]]*)
              echo "::error::proxy credentials must not contain whitespace"
              exit 1
              ;;
          esac
          if [ -n "$GIT_TOKEN" ]; then
            if [ "${GIT_CONFIG_COUNT+x}" = x ]; then
              git_config_count=$GIT_CONFIG_COUNT
              case "$git_config_count" in
                ''|*[!0-9]*|0[0-9]*)
                  echo "::error::GIT_CONFIG_COUNT must be a non-negative decimal integer"
                  exit 1
                  ;;
              esac
              if [ ${#git_config_count} -gt 16 ] ||
                 [ "$git_config_count" -gt 9007199254740988 ] 2>/dev/null; then
                echo "::error::GIT_CONFIG_COUNT is too large to append private Git configuration"
                exit 1
              fi
            else
              git_config_count=0
            fi
          fi
          if [ -n "$PRIVATE_PATTERNS" ]; then
            printf 'GOPRIVATE=%s\n' "$PRIVATE_PATTERNS" >> "$GITHUB_ENV"
          fi
          if [ -n "$PRIVATE_NOSUMDB" ]; then
            printf 'GONOSUMDB=%s\n' "$PRIVATE_NOSUMDB" >> "$GITHUB_ENV"
          fi
          if [ -n "$PRIVATE_PROXY" ]; then
            case "$PRIVATE_PROXY" in
              *://*@*) echo "::error::GOPROXY must not contain credentials"; exit 1 ;;
            esac
            printf 'GOPROXY=%s\n' "$PRIVATE_PROXY" >> "$GITHUB_ENV"
          fi
          if { [ -n "$PROXY_USERNAME" ] && [ -z "$PROXY_PASSWORD" ]; } ||
             { [ -z "$PROXY_USERNAME" ] && [ -n "$PROXY_PASSWORD" ]; }; then
            echo "::error::GO_PROXY_USERNAME and GO_PROXY_PASSWORD must be set together"
            exit 1
          fi
          if [ -n "$GIT_TOKEN" ] || [ -n "$PROXY_USERNAME" ]; then
            private_dir="$(mktemp -d "$RUNNER_TEMP/ns-go-private.XXXXXX")"
            cleanup_private_dir() { rm -rf -- "$private_dir"; }
            trap cleanup_private_dir EXIT
            chmod 700 "$private_dir"
            printf 'NS_GO_PRIVATE_DIR=%s\n' "$private_dir" >> "$GITHUB_ENV"
            trap - EXIT
          fi
          if [ -n "$GIT_TOKEN" ]; then
            token_file="$private_dir/git-token"
            printf '%s' "$GIT_TOKEN" > "$token_file"
            chmod 600 "$token_file"
            helper="$private_dir/git-credential"
            cat > "$helper" <<'EOF'
          #!/bin/sh
          protocol=
          host=
          while IFS='=' read -r key value; do
            [ "$key" = protocol ] && protocol=$value
            [ "$key" = host ] && host=$value
          done
          if [ "$1" = get ] && [ "$protocol" = https ] && [ "$host" = github.com ]; then
            printf 'username=x-access-token\npassword='
            cat "$NS_GO_GIT_TOKEN_FILE"
            printf '\n'
          fi
          EOF
            chmod 700 "$helper"
            printf 'NS_GO_GIT_TOKEN_FILE=%s\n' "$token_file" >> "$GITHUB_ENV"
            git_key_0=$git_config_count
            git_key_1=$((git_config_count + 1))
            git_key_2=$((git_config_count + 2))
            git_config_count=$((git_config_count + 3))
            printf '%s\n' \
              "GIT_CONFIG_KEY_${git_key_0}=credential.https://github.com.helper" \
              "GIT_CONFIG_VALUE_${git_key_0}=" \
              "GIT_CONFIG_KEY_${git_key_1}=credential.https://github.com.helper" \
              "GIT_CONFIG_VALUE_${git_key_1}=$helper" \
              "GIT_CONFIG_KEY_${git_key_2}=credential.https://github.com.useHttpPath" \
              "GIT_CONFIG_VALUE_${git_key_2}=false" \
              "GIT_CONFIG_COUNT=$git_config_count" >> "$GITHUB_ENV"
          fi
          if [ -n "$PROXY_USERNAME" ]; then
            [ -n "$PRIVATE_PROXY" ] || {
              echo "::error::GOPROXY is required with proxy credentials"
              exit 1
            }
            netrc="$private_dir/netrc"
            : > "$netrc"
            old_ifs=$IFS
            IFS=',|'
            proxy_hosts=0
            case $- in
              *f*) glob_was_disabled=1 ;;
              *) glob_was_disabled=0; set -f ;;
            esac
            for proxy in $PRIVATE_PROXY; do
              case "$proxy" in
                direct|off|'') continue ;;
                https://*) ;;
                http://*)
                  echo "::error::authenticated GOPROXY entries must use HTTPS"
                  exit 1
                  ;;
                *)
                  echo "::error::authenticated GOPROXY entries must be valid HTTPS URLs"
                  exit 1
                  ;;
              esac
              location=${proxy#https://}
              authority=${location%%/*}
              case "$authority" in
                ''|*@*|*\?*|*\#*|*[[:space:]]*|*\[*|*\]*)
                  echo "::error::authenticated GOPROXY contains an invalid HTTPS authority (IPv6 is not supported)"
                  exit 1
                  ;;
              esac
              case "$authority" in
                *:*)
                  host=${authority%:*}
                  port=${authority##*:}
                  case "$host" in *:*) host= ;; esac
                  case "$port" in ''|*[!0-9]*) port=0 ;; esac
                  [ "$port" -ge 1 ] 2>/dev/null && [ "$port" -le 65535 ] 2>/dev/null || {
                    echo "::error::authenticated GOPROXY contains an invalid port"
                    exit 1
                  }
                  ;;
                *) host=$authority ;;
              esac
              case "$host" in ''|.*|*.|*..*) host= ;; esac
              case "$host" in *[!0-9.]*) numeric_host=0 ;; *) numeric_host=1 ;; esac
              host_parts=0
              [ ${#host} -le 253 ] || host=
              old_host_ifs=$IFS
              IFS=.
              for label in $host; do
                host_parts=$((host_parts + 1))
                case "$label" in
                  ''|*[!A-Za-z0-9-]*|-*|*-*) host=; break ;;
                esac
                [ ${#label} -le 63 ] || { host=; break; }
                if [ "$numeric_host" -eq 1 ] && [ "$label" -gt 255 ] 2>/dev/null; then
                  host=
                  break
                fi
              done
              IFS=$old_host_ifs
              if [ "$numeric_host" -eq 1 ] && [ "$host_parts" -ne 4 ]; then host=; fi
              [ -n "$host" ] || {
                echo "::error::authenticated GOPROXY contains an invalid DNS/IPv4 hostname"
                exit 1
              }
              machine=$authority
              printf 'machine %s login %s password %s\n' "$machine" "$PROXY_USERNAME" "$PROXY_PASSWORD" >> "$netrc"
              proxy_hosts=$((proxy_hosts + 1))
            done
            [ "$glob_was_disabled" -eq 1 ] || set +f
            IFS=$old_ifs
            [ "$proxy_hosts" -gt 0 ] || {
              echo "::error::authenticated GOPROXY requires at least one HTTPS host"
              exit 1
            }
            chmod 600 "$netrc"
            printf 'NETRC=%s\n' "$netrc" >> "$GITHUB_ENV"
          fi
"#;

const GO_PRIVATE_CLEANUP_STEP: &str = r#"      - name: Remove private Go credentials
        if: always()
        shell: bash
        run: |
          case "${NS_GO_PRIVATE_DIR:-}" in
            "$RUNNER_TEMP"/ns-go-private.*) rm -rf -- "$NS_GO_PRIVATE_DIR" ;;
          esac
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
    if job != Job::Image {
        out.push_str(GO_PRIVATE_STEP);
    }
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

/// Remove ephemeral Go credential files. Append after every cache, report, or
/// publishing step in a job whose setup includes `GO_PRIVATE_STEP`.
pub fn go_private_cleanup_step() -> &'static str {
    GO_PRIVATE_CLEANUP_STEP
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
        assert!(text.contains("Configure private Go modules"));
        assert!(text.contains("secrets.GO_PRIVATE_TOKEN"));
        assert!(text.contains("[ \"$protocol\" = https ] && [ \"$host\" = github.com ]"));
        assert!(text.contains("GIT_CONFIG_COUNT must be a non-negative decimal integer"));
        assert!(text.contains("9007199254740988"));
        assert!(text.contains("git_key_0=$git_config_count"));
        assert!(text.contains("GIT_CONFIG_KEY_${git_key_0}=credential.https://github.com.helper"));
        assert!(text.contains("GIT_CONFIG_VALUE_${git_key_0}="));
        assert!(text.contains("GIT_CONFIG_KEY_${git_key_1}=credential.https://github.com.helper"));
        assert!(
            text.contains("GIT_CONFIG_KEY_${git_key_2}=credential.https://github.com.useHttpPath")
        );
        assert!(text.contains("GIT_CONFIG_COUNT=$git_config_count"));
        assert!(!text.contains("GIT_CONFIG_KEY_0=credential.helper\n"));
        assert!(text.contains("secrets.GO_PROXY_USERNAME"));
        assert!(text.contains("secrets.GO_PROXY_PASSWORD"));
        assert!(text.contains("mktemp -d \"$RUNNER_TEMP/ns-go-private.XXXXXX\""));
        assert!(text.contains("chmod 600 \"$token_file\""));
        assert!(text.contains("cat \"$NS_GO_GIT_TOKEN_FILE\""));
        assert!(text.contains("chmod 600 \"$netrc\""));
        assert!(text.contains("IFS=',|'"));
        assert!(text.contains("authenticated GOPROXY contains an invalid port"));
        assert!(text.contains("invalid DNS/IPv4 hostname"));
        assert!(text.contains("IPv6 is not supported"));
        let mktemp = text.find("private_dir=\"$(mktemp").unwrap();
        let trap = text.find("trap cleanup_private_dir EXIT").unwrap();
        let exported = text.find("NS_GO_PRIVATE_DIR=%s").unwrap();
        let clear_trap = text.find("trap - EXIT").unwrap();
        assert!(mktemp < trap && trap < exported && exported < clear_trap);
        assert!(text.contains("GOPROXY must not contain credentials"));
        assert!(!text.contains("printf 'GO_PRIVATE_TOKEN="));
        assert!(!text.contains("git config --global"));
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
        assert!(!text.contains("GO_PRIVATE_TOKEN"));
    }

    #[test]
    fn save_steps_are_guarded_and_capped() {
        let text = fill(&save_steps(CacheKind::Release), "0.1.0");
        assert_eq!(text.matches(&format!("if: {SAVE_GUARD}")).count(), 2);
        assert!(text.contains("--max-gb 4"));
        assert!(save_steps(CacheKind::Test).contains("--max-gb 3"));
    }

    #[test]
    fn private_cleanup_is_guarded_and_unconditional() {
        let text = go_private_cleanup_step();
        assert!(text.contains("if: always()"));
        assert!(text.contains("\"$RUNNER_TEMP\"/ns-go-private.*"));
        assert!(text.contains("rm -rf -- \"$NS_GO_PRIVATE_DIR\""));
    }
}
