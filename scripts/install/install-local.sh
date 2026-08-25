#!/bin/sh

set -eu

BIN_DIR="${CODEX_INSTALL_DIR:-$HOME/.local/bin}"
BIN_PATH="$BIN_DIR/codex"
CODEX_HOME_DIR="${CODEX_HOME:-$HOME/.codex}"
STANDALONE_ROOT="$CODEX_HOME_DIR/packages/standalone"
RELEASES_DIR="$STANDALONE_ROOT/releases"
CURRENT_LINK="$STANDALONE_ROOT/current"
LOCK_FILE="$STANDALONE_ROOT/install.lock"
LOCK_PATH="$STANDALONE_ROOT/install.lock.d"
LOCK_STALE_AFTER_SECS=600
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd)
CODEX_RS_DIR="$REPO_ROOT/codex-rs"
CODEX_REPO_ROOT="$REPO_ROOT"
export CODEX_REPO_ROOT

path_action="already"
path_profile=""
lock_kind=""
lock_owner_file=""
tmp_dir=""
python_bin=""
cargo_toml_backup=""
cargo_lock_backup=""
version_state_dir=""
version_lock_file=""
version_lock_path=""
version_lock_kind=""
version_lock_owner_file=""
version_transaction_dir=""
version_transaction_owned=false
active_reclaim_marker=""
active_reclaim_guard=""
use_upstream_version=false
upstream_version_override=""
upstream_version_override_set=false

step() {
  printf '==> %s\n' "$1"
}

warn() {
  printf 'WARNING: %s\n' "$1" >&2
}

usage() {
  cat <<'EOF'
Usage: install-local.sh [--use-upstream-version] [--upstream-version VERSION]

  --use-upstream-version   Discover the greatest upstream Release baseline in
                           the current commit's ancestry and use it for the build.
  --upstream-version VER   Use an explicit bare SemVer Release baseline. This
                           overrides CODEX_UPSTREAM_VERSION and enables versioning.

CODEX_UPSTREAM_VERSION supplies a validated override and enables versioning when
no explicit version argument is present.

On Windows Git Bash/MSYS/Cygwin, this delegates to install-local.ps1.
EOF
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --help | -h)
        usage
        exit 0
        ;;
      --use-upstream-version)
        use_upstream_version=true
        ;;
      --upstream-version)
        if [ "$#" -lt 2 ]; then
          echo "--upstream-version requires a bare SemVer argument." >&2
          exit 1
        fi
        upstream_version_override="$2"
        upstream_version_override_set=true
        use_upstream_version=true
        shift
        ;;
      --upstream-version=*)
        upstream_version_override=${1#*=}
        upstream_version_override_set=true
        use_upstream_version=true
        ;;
      *)
        echo "Unknown argument: $1" >&2
        exit 1
        ;;
    esac
    shift
  done
}

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "$1 is required to install a local Codex debug build." >&2
    exit 1
  fi
}

resolve_python_bin() {
  if command -v python3 >/dev/null 2>&1; then
    printf 'python3\n'
    return
  fi

  if command -v python >/dev/null 2>&1; then
    printf 'python\n'
    return
  fi

  echo "python3 or python is required to install a local Codex debug build." >&2
  exit 1
}

python_with_scripts_path() {
  PYTHONPATH="$REPO_ROOT/scripts${PYTHONPATH:+:$PYTHONPATH}" "$python_bin" "$@"
}

read_workspace_version() {
  python_with_scripts_path -c 'from codex_package.version import read_workspace_version; print(read_workspace_version())'
}

resolve_upstream_build_version() {
  if [ "$upstream_version_override_set" = true ]; then
    python_with_scripts_path -c \
      'import sys; from codex_package.version import resolve_upstream_build_version; print(resolve_upstream_build_version(sys.argv[1]))' \
      "$upstream_version_override"
  else
    python_with_scripts_path -c \
      'from codex_package.version import resolve_upstream_build_version; print(resolve_upstream_build_version())'
  fi
}

set_workspace_version() {
  version="$1"
  python_with_scripts_path - "$CODEX_RS_DIR/Cargo.toml" "$version" <<'PY'
import sys
from pathlib import Path

from codex_package.version import replace_workspace_version

replace_workspace_version(Path(sys.argv[1]), sys.argv[2])
PY
}

backup_cargo_manifest_files() {
  cargo_toml="$CODEX_RS_DIR/Cargo.toml"
  cargo_lock="$CODEX_RS_DIR/Cargo.lock"

  cargo_toml_backup="$version_transaction_dir/Cargo.toml.original"
  cp "$cargo_toml" "$cargo_toml_backup" || return 1

  if [ -f "$cargo_lock" ]; then
    cargo_lock_backup="$version_transaction_dir/Cargo.lock.original"
    cp "$cargo_lock" "$cargo_lock_backup" || return 1
  else
    cargo_lock_backup="$version_transaction_dir/Cargo.lock.missing"
    : >"$cargo_lock_backup" || return 1
  fi
}

print_version_transaction_recovery() {
  transaction_dir="$1"

  echo "A local Release-baseline version transaction is present at:" >&2
  echo "  $transaction_dir" >&2
  echo "The retained backups are:" >&2
  echo "  $transaction_dir/Cargo.toml.original" >&2
  if [ -f "$transaction_dir/Cargo.lock.original" ]; then
    echo "  $transaction_dir/Cargo.lock.original" >&2
  else
    echo "  $transaction_dir/Cargo.lock.missing (Cargo.lock was originally absent)" >&2
  fi
  echo "Refusing to restore or mutate the workspace automatically." >&2
  echo "Recovery steps:" >&2
  echo "  1. Inspect the current Cargo.toml and Cargo.lock and the retained backups." >&2
  echo "  2. Restore Cargo.toml.original to $CODEX_RS_DIR/Cargo.toml." >&2
  if [ -f "$transaction_dir/Cargo.lock.original" ]; then
    echo "  3. Restore Cargo.lock.original to $CODEX_RS_DIR/Cargo.lock." >&2
  else
    echo "  3. Remove $CODEX_RS_DIR/Cargo.lock if it was created by the transaction." >&2
  fi
  echo "  4. Verify the restored files byte for byte, then remove $transaction_dir." >&2
}

restore_cargo_manifest_files() {
  restore_failed=0

  if [ "$version_transaction_owned" != true ]; then
    return 0
  fi

  if ! cp "$cargo_toml_backup" "$CODEX_RS_DIR/Cargo.toml"; then
    restore_failed=1
  elif ! cmp -s "$cargo_toml_backup" "$CODEX_RS_DIR/Cargo.toml"; then
    restore_failed=1
  fi

  if [ -f "$version_transaction_dir/Cargo.lock.original" ]; then
    if ! cp "$cargo_lock_backup" "$CODEX_RS_DIR/Cargo.lock"; then
      restore_failed=1
    elif ! cmp -s "$cargo_lock_backup" "$CODEX_RS_DIR/Cargo.lock"; then
      restore_failed=1
    fi
  else
    if ! rm -f "$CODEX_RS_DIR/Cargo.lock"; then
      restore_failed=1
    elif [ -e "$CODEX_RS_DIR/Cargo.lock" ] || [ -L "$CODEX_RS_DIR/Cargo.lock" ]; then
      restore_failed=1
    fi
  fi

  if [ "$restore_failed" -eq 0 ]; then
    if ! rm -rf "$version_transaction_dir"; then
      restore_failed=1
    fi
  fi

  if [ "$restore_failed" -eq 0 ]; then
    cargo_toml_backup=""
    cargo_lock_backup=""
    version_transaction_dir=""
    version_transaction_owned=false
  else
    echo "Failed to restore and verify the Cargo workspace byte for byte." >&2
    print_version_transaction_recovery "$version_transaction_dir"
  fi

  return "$restore_failed"
}

fallback_lock_is_stale() {
  stale_lock="$1"
  stale_threshold="$2"
  if [ -d "$stale_lock" ]; then
    stale_pid="$(cat "$stale_lock/pid" 2>/dev/null || true)"
    stale_started_at="$(cat "$stale_lock/started_at" 2>/dev/null || true)"
  elif [ -f "$stale_lock" ]; then
    stale_pid="$(sed -n '1p' "$stale_lock" 2>/dev/null || true)"
    stale_started_at="$(sed -n '2p' "$stale_lock" 2>/dev/null || true)"
  else
    return 1
  fi
  case "$stale_pid" in
    '' | *[!0-9]*) return 1 ;;
  esac
  if [ -n "$stale_pid" ] && kill -0 "$stale_pid" 2>/dev/null; then
    return 1
  fi
  if [ "$stale_threshold" -eq 0 ]; then
    return 0
  fi
  case "$stale_started_at" in
    '' | *[!0-9]*) return 1 ;;
  esac
  stale_now="$(date +%s 2>/dev/null || printf '0')"
  if [ "$stale_now" -eq 0 ]; then
    return 1
  fi
  [ $((stale_now - stale_started_at)) -ge "$stale_threshold" ]
}

try_claim_fallback_lock() {
  try_owner="$1"
  try_lock="$2"

  ln "$try_owner" "$try_lock" 2>/dev/null || return 1
  if [ -f "$try_lock" ] && cmp -s "$try_lock" "$try_owner"; then
    return 0
  fi

  # POSIX ln treats an existing directory as a destination directory. Remove
  # the hard link it created there and keep waiting for that legacy lock.
  rm -f "$try_lock/$(basename "$try_owner")" 2>/dev/null || true
  return 1
}

cleanup_stale_reclaim_markers() {
  cleanup_lock="$1"
  for cleanup_marker in "$cleanup_lock".reclaim.*; do
    [ -f "$cleanup_marker" ] || continue
    [ "$cleanup_marker" = "$cleanup_lock.reclaim.guard" ] && continue
    if fallback_lock_is_stale "$cleanup_marker" 0; then
      rm -f "$cleanup_marker" 2>/dev/null || true
    fi
  done
}

reclaim_barrier_exists() {
  barrier_candidate_lock="$1"
  for barrier_candidate in "$barrier_candidate_lock".reclaim.*; do
    [ -f "$barrier_candidate" ] && return 0
  done
  return 1
}

wait_for_reclaim_barrier() {
  barrier_lock="$1"
  while :; do
    cleanup_stale_reclaim_markers "$barrier_lock"
    barrier_guard="$barrier_lock.reclaim.guard"
    if [ -f "$barrier_guard" ] && fallback_lock_is_stale "$barrier_guard" 0; then
      echo "Stale reclaim guard at $barrier_guard requires manual removal; refusing an unsafe automatic takeover." >&2
      return 1
    fi
    reclaim_barrier_exists "$barrier_lock" || return 0
    sleep 1
  done
}

publish_reclaim_marker() {
  publish_lock="$1"
  publish_prepare="$(mktemp "$publish_lock.reclaim-prepare.XXXXXX")"
  publish_suffix="${publish_prepare##*.}"
  published_marker="$publish_lock.reclaim.$publish_suffix"
  {
    printf '%s\n' "$$"
    date +%s 2>/dev/null || printf '0\n'
  } >"$publish_prepare"
  mv "$publish_prepare" "$published_marker"
  printf '%s\n' "$published_marker"
}

acquire_reclaim_guard() {
  guard_lock="$1"
  guard_marker="$2"
  reclaim_guard="$guard_lock.reclaim.guard"
  while ! ln "$guard_marker" "$reclaim_guard" 2>/dev/null; do
    if [ -f "$reclaim_guard" ] && fallback_lock_is_stale "$reclaim_guard" 0; then
      echo "Stale reclaim guard at $reclaim_guard requires manual removal; refusing an unsafe automatic takeover." >&2
      return 1
    fi
    sleep 1
  done
  if [ ! -f "$reclaim_guard" ] || ! cmp -s "$reclaim_guard" "$guard_marker"; then
    return 1
  fi
  active_reclaim_guard="$reclaim_guard"
}

release_reclaim_guard() {
  guard_marker="$1"
  if [ -n "$active_reclaim_guard" ] &&
    [ -f "$active_reclaim_guard" ] &&
    cmp -s "$active_reclaim_guard" "$guard_marker"; then
    rm -f "$active_reclaim_guard" 2>/dev/null || true
  fi
  active_reclaim_guard=""
}

reclaim_fallback_lock() {
  reclaim_lock="$1"
  reclaim_owner_prefix="$2"
  reclaim_stale_threshold="$3"
  active_reclaim_marker="$(publish_reclaim_marker "$reclaim_lock")" || return 1
  reclaim_suffix="${active_reclaim_marker##*.}"
  if ! acquire_reclaim_guard "$reclaim_lock" "$active_reclaim_marker"; then
    rm -f "$active_reclaim_marker" 2>/dev/null || true
    active_reclaim_marker=""
    return 1
  fi

  if [ -d "$reclaim_lock" ]; then
    if fallback_lock_is_stale "$reclaim_lock" "$reclaim_stale_threshold"; then
      reclaimed_lock="$reclaim_lock.stale.$reclaim_suffix"
      if mv "$reclaim_lock" "$reclaimed_lock" 2>/dev/null; then
        rm -rf "$reclaimed_lock"
      fi
    fi
  elif [ -f "$reclaim_lock" ]; then
    reclaimed_lock="$reclaim_lock.snapshot.$reclaim_suffix"
    if ln "$reclaim_lock" "$reclaimed_lock" 2>/dev/null; then
      if fallback_lock_is_stale "$reclaimed_lock" "$reclaim_stale_threshold"; then
        reclaimed_owner="$(sed -n '3p' "$reclaimed_lock" 2>/dev/null || true)"
        rm -f "$reclaim_lock" 2>/dev/null || true
        case "$reclaimed_owner" in
          "$reclaim_owner_prefix"*) rm -f "$reclaimed_owner" 2>/dev/null || true ;;
        esac
      fi
      rm -f "$reclaimed_lock" 2>/dev/null || true
    fi
  fi
  release_reclaim_guard "$active_reclaim_marker"
  rm -f "$active_reclaim_marker" 2>/dev/null || true
  active_reclaim_marker=""
}

acquire_fallback_lock() {
  claim_owner="$1"
  claim_lock="$2"
  claim_owner_prefix="$3"
  claim_stale_threshold="$4"
  claim_description="$5"

  while :; do
    wait_for_reclaim_barrier "$claim_lock"
    if try_claim_fallback_lock "$claim_owner" "$claim_lock"; then
      wait_for_reclaim_barrier "$claim_lock"
      if [ -f "$claim_lock" ] && cmp -s "$claim_lock" "$claim_owner"; then
        return
      fi
      continue
    fi
    if fallback_lock_is_stale "$claim_lock" "$claim_stale_threshold"; then
      warn "Removing stale $claim_description lock at $claim_lock"
      reclaim_fallback_lock \
        "$claim_lock" "$claim_owner_prefix" "$claim_stale_threshold" || true
      continue
    fi
    sleep 1
  done
}

acquire_version_lock() {
  version_state_dir="$(
    git -C "$REPO_ROOT" rev-parse \
      --path-format=absolute \
      --git-path codex-local-version
  )"
  version_lock_file="$version_state_dir/version.lock"
  version_lock_path="$version_state_dir/version.lock.d"
  mkdir -p "$version_state_dir"

  if command -v flock >/dev/null 2>&1; then
    exec 8>"$version_lock_file"
    flock 8
    version_lock_kind="flock"
    return
  fi

  version_lock_owner_file="$(mktemp "$version_state_dir/version.lock.owner.XXXXXX")"
  {
    printf '%s\n' "$$"
    date +%s 2>/dev/null || printf '0\n'
    printf '%s\n' "$version_lock_owner_file"
  } >"$version_lock_owner_file"

  acquire_fallback_lock \
    "$version_lock_owner_file" \
    "$version_lock_path" \
    "$version_state_dir/version.lock.owner." \
    0 \
    local-version
  version_lock_kind="hardlink"
}

release_version_lock() {
  if [ "$version_lock_kind" = "flock" ]; then
    exec 8>&- 2>/dev/null || true
  fi
  if [ -n "$version_lock_owner_file" ]; then
    if [ -f "$version_lock_path" ] && cmp -s "$version_lock_path" "$version_lock_owner_file"; then
      rm -f "$version_lock_path" 2>/dev/null || true
    fi
    rm -f "$version_lock_owner_file" 2>/dev/null || true
  fi
  if [ -n "$active_reclaim_guard" ]; then
    rm -f "$active_reclaim_guard" 2>/dev/null || true
    active_reclaim_guard=""
  fi
  if [ -n "$active_reclaim_marker" ]; then
    rm -f "$active_reclaim_marker" 2>/dev/null || true
    active_reclaim_marker=""
  fi
  version_lock_kind=""
  version_lock_owner_file=""
}

begin_version_transaction() {
  final_transaction_dir="$version_state_dir/transaction"
  pending_transaction_dir="$version_state_dir/.transaction.prepare.$$"

  rm -rf "$pending_transaction_dir"
  if ! mkdir "$pending_transaction_dir"; then
    return 1
  fi

  version_transaction_dir="$pending_transaction_dir"
  if ! backup_cargo_manifest_files; then
    rm -rf "$pending_transaction_dir"
    version_transaction_dir=""
    cargo_toml_backup=""
    cargo_lock_backup=""
    return 1
  fi
  {
    printf 'pid=%s\n' "$$"
    printf 'worktree=%s\n' "$REPO_ROOT"
  } >"$pending_transaction_dir/transaction.info"

  if ! mv "$pending_transaction_dir" "$final_transaction_dir"; then
    rm -rf "$pending_transaction_dir"
    version_transaction_dir=""
    cargo_toml_backup=""
    cargo_lock_backup=""
    return 1
  fi

  version_transaction_dir="$final_transaction_dir"
  cargo_toml_backup="$version_transaction_dir/Cargo.toml.original"
  if [ -f "$version_transaction_dir/Cargo.lock.original" ]; then
    cargo_lock_backup="$version_transaction_dir/Cargo.lock.original"
  else
    cargo_lock_backup="$version_transaction_dir/Cargo.lock.missing"
  fi
  version_transaction_owned=true
}

prepare_upstream_build_version() {
  require_command git
  require_command cmp
  acquire_version_lock

  version_transaction_dir="$version_state_dir/transaction"
  if [ -d "$version_transaction_dir" ]; then
    print_version_transaction_recovery "$version_transaction_dir"
    return 1
  fi
  find "$version_state_dir" -mindepth 1 -maxdepth 1 \
    -name '.transaction.prepare.*' -exec rm -rf {} +

  current_workspace_version="$(read_workspace_version)"
  upstream_build_version="$(resolve_upstream_build_version)"

  if [ "$upstream_build_version" = "$current_workspace_version" ]; then
    version_transaction_dir=""
    release_version_lock
    return
  fi

  step "Using upstream Release-baseline version $upstream_build_version for local build"
  begin_version_transaction
  set_workspace_version "$upstream_build_version"
}

is_windows_uname() {
  case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

windows_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
    return
  fi

  printf '%s\n' "$1"
}

run_windows_local_installer() {
  ps_script="$(windows_path "$SCRIPT_DIR/install-local.ps1")"

  if command -v pwsh >/dev/null 2>&1; then
    powershell_cmd="pwsh"
  elif command -v powershell.exe >/dev/null 2>&1; then
    powershell_cmd="powershell.exe"
  elif command -v powershell >/dev/null 2>&1; then
    powershell_cmd="powershell"
  else
    echo "PowerShell is required to install a local Codex debug build on Windows." >&2
    echo "Run scripts/install/install-local.ps1 from PowerShell, or install PowerShell and retry." >&2
    exit 1
  fi

  step "Detected Windows Git Bash/MSYS/Cygwin; using install-local.ps1"
  "$powershell_cmd" -NoProfile -ExecutionPolicy Bypass -File "$ps_script" "$@"
}

pick_profile() {
  case "$(uname -s):${SHELL:-}" in
    Darwin:*/zsh)
      printf '%s\n' "$HOME/.zprofile"
      ;;
    Darwin:*/bash)
      printf '%s\n' "$HOME/.bash_profile"
      ;;
    Linux:*/zsh)
      printf '%s\n' "$HOME/.zshrc"
      ;;
    Linux:*/bash)
      printf '%s\n' "$HOME/.bashrc"
      ;;
    *)
      printf '%s\n' "$HOME/.profile"
      ;;
  esac
}

append_path_block() {
  profile="$1"
  begin_marker="$2"
  end_marker="$3"
  path_line="$4"

  {
    printf '\n%s\n' "$begin_marker"
    printf '%s\n' "$path_line"
    printf '%s\n' "$end_marker"
  } >>"$profile"
}

rewrite_path_block() {
  profile="$1"
  begin_marker="$2"
  end_marker="$3"
  path_line="$4"
  tmp_profile="$tmp_dir/profile.$$.tmp"

  awk -v begin="$begin_marker" -v end="$end_marker" -v line="$path_line" '
    BEGIN {
      in_block = 0
      replaced = 0
    }
    $0 == begin {
      if (!replaced) {
        print begin
        print line
        print end
        replaced = 1
      }
      in_block = 1
      next
    }
    in_block {
      if ($0 == end) {
        in_block = 0
      }
      next
    }
    {
      print
    }
    END {
      if (in_block != 0) {
        exit 1
      }
    }
  ' "$profile" >"$tmp_profile"
  mv "$tmp_profile" "$profile"
}

add_to_path() {
  path_action="already"
  path_profile=""

  case ":$PATH:" in
    *":$BIN_DIR:"*)
      return
      ;;
  esac

  profile="$(pick_profile)"
  path_profile="$profile"
  begin_marker="# >>> Codex installer >>>"
  end_marker="# <<< Codex installer <<<"
  path_line="export PATH=\"$BIN_DIR:\$PATH\""

  if [ -f "$profile" ] && grep -F "$begin_marker" "$profile" >/dev/null 2>&1; then
    if grep -F "$path_line" "$profile" >/dev/null 2>&1; then
      path_action="configured"
      return
    fi

    if grep -F "$end_marker" "$profile" >/dev/null 2>&1; then
      rewrite_path_block "$profile" "$begin_marker" "$end_marker" "$path_line"
      path_action="updated"
      return
    fi
  fi

  append_path_block "$profile" "$begin_marker" "$end_marker" "$path_line"
  path_action="added"
}

print_launch_instructions() {
  case "$path_action" in
    added | updated | configured)
      step "Current terminal: export PATH=\"$BIN_DIR:\$PATH\" && codex"
      step "Future terminals: open a new terminal and run: codex"
      step "PATH was configured in $path_profile"
      ;;
    *)
      step "Current terminal: codex"
      step "Future terminals: open a new terminal and run: codex"
      ;;
  esac
}

acquire_install_lock() {
  mkdir -p "$STANDALONE_ROOT"

  if command -v flock >/dev/null 2>&1; then
    exec 9>"$LOCK_FILE"
    flock 9
    lock_kind="flock"
    return
  fi

  lock_owner_file="$(mktemp "$STANDALONE_ROOT/install.lock.owner.XXXXXX")"
  {
    printf '%s\n' "$$"
    date +%s 2>/dev/null || printf '0\n'
    printf '%s\n' "$lock_owner_file"
  } >"$lock_owner_file"

  acquire_fallback_lock \
    "$lock_owner_file" \
    "$LOCK_PATH" \
    "$STANDALONE_ROOT/install.lock.owner." \
    "$LOCK_STALE_AFTER_SECS" \
    installer

  lock_kind="hardlink"
}

release_install_lock() {
  if [ "$lock_kind" = "flock" ]; then
    exec 9>&- 2>/dev/null || true
  fi
  if [ -n "$lock_owner_file" ]; then
    if [ -f "$LOCK_PATH" ] && cmp -s "$LOCK_PATH" "$lock_owner_file"; then
      rm -f "$LOCK_PATH" 2>/dev/null || true
    fi
    rm -f "$lock_owner_file" 2>/dev/null || true
  fi
  if [ -n "$active_reclaim_guard" ]; then
    rm -f "$active_reclaim_guard" 2>/dev/null || true
    active_reclaim_guard=""
  fi
  if [ -n "$active_reclaim_marker" ]; then
    rm -f "$active_reclaim_marker" 2>/dev/null || true
    active_reclaim_marker=""
  fi
  lock_kind=""
  lock_owner_file=""
}

cleanup_stale_install_artifacts() {
  mkdir -p "$RELEASES_DIR" "$STANDALONE_ROOT"

  find "$RELEASES_DIR" -mindepth 1 -maxdepth 1 -name '.staging.*' -exec rm -rf {} +
  find "$STANDALONE_ROOT" -mindepth 1 -maxdepth 1 -name '.current.*' -exec rm -f {} +
  find "$STANDALONE_ROOT" -mindepth 1 -maxdepth 1 -name '.swap-backup.*' -exec rm -rf {} +

  if [ -d "$BIN_DIR" ]; then
    find "$BIN_DIR" -mindepth 1 -maxdepth 1 -name '.codex.*' -exec rm -f {} +
    find "$BIN_DIR" -mindepth 1 -maxdepth 1 -name '.swap-backup.*' -exec rm -rf {} +
  fi
}

replace_path_with_symlink() {
  link_path="$1"
  link_target="$2"
  tmp_link="$3"
  backup_path="$(dirname "$link_path")/.swap-backup.$(basename "$link_path").$$"

  rm -rf "$tmp_link" "$backup_path"
  ln -s "$link_target" "$tmp_link"

  if mv -Tf "$tmp_link" "$link_path" 2>/dev/null; then
    return 0
  fi

  if mv -hf "$tmp_link" "$link_path" 2>/dev/null; then
    return 0
  fi

  if [ -L "$link_path" ] || [ -e "$link_path" ]; then
    if ! mv -f "$link_path" "$backup_path"; then
      rm -f "$tmp_link"
      echo "Failed to prepare replacement for $link_path." >&2
      return 1
    fi
  fi

  if mv -f "$tmp_link" "$link_path"; then
    rm -rf "$backup_path"
    return 0
  fi

  if [ -L "$backup_path" ] || [ -e "$backup_path" ]; then
    mv -f "$backup_path" "$link_path" 2>/dev/null || true
  fi
  rm -f "$tmp_link"
  echo "Failed to replace $link_path." >&2
  return 1
}

resolve_platform_target() {
  case "$(uname -s):$(uname -m)" in
    Linux:x86_64 | Linux:amd64)
      printf 'x86_64-unknown-linux-gnu\n'
      ;;
    Linux:arm64 | Linux:aarch64)
      printf 'aarch64-unknown-linux-gnu\n'
      ;;
    Darwin:x86_64 | Darwin:amd64)
      printf 'x86_64-apple-darwin\n'
      ;;
    Darwin:arm64 | Darwin:aarch64)
      printf 'aarch64-apple-darwin\n'
      ;;
    *)
      echo "Unsupported platform: $(uname -s) $(uname -m)" >&2
      exit 1
      ;;
  esac
}

generate_release_name() {
  release_prefix="$1"
  timestamp="$(date +%Y%m%d%H%M%S 2>/dev/null || date +%s)"

  printf "%s-%s-%s\n" "$release_prefix" "$timestamp" "$$"
}

build_local_package() {
  package_dir="$1"
  target="$2"

  step "Building local Codex debug package"
  rm -rf "$package_dir"
  # Keep fast dev builds while matching release behavior for recoverable
  # session-history invariants.
  if [ -n "${CODEX_LOCAL_RG:-}" ]; then
    if [ ! -x "$CODEX_LOCAL_RG" ]; then
      echo "CODEX_LOCAL_RG must point to an executable rg." >&2
      return 1
    fi

    CARGO_PROFILE_DEV_DEBUG_ASSERTIONS=false \
      "$python_bin" "$REPO_ROOT/scripts/build_codex_package.py" \
      --target "$target" \
      --variant codex \
      --cargo-profile dev \
      --package-dir "$package_dir" \
      --rg-bin "$CODEX_LOCAL_RG" \
      --force
  else
    CARGO_PROFILE_DEV_DEBUG_ASSERTIONS=false \
      "$python_bin" "$REPO_ROOT/scripts/build_codex_package.py" \
      --target "$target" \
      --variant codex \
      --cargo-profile dev \
      --package-dir "$package_dir" \
      --force
  fi

  ln -sf "bin/codex" "$package_dir/codex"
}

release_dir_is_complete() {
  candidate_dir="$1"
  expected_target="$2"

  [ -f "$candidate_dir/codex-package.json" ] &&
    [ -x "$candidate_dir/bin/codex" ] &&
    [ -x "$candidate_dir/codex" ] &&
    [ -x "$candidate_dir/codex-path/rg" ] &&
    grep -E "\"target\"[[:space:]]*:[[:space:]]*\"$expected_target\"" "$candidate_dir/codex-package.json" >/dev/null 2>&1 ||
    return 1

  case "$expected_target" in
    *linux*) [ -x "$candidate_dir/codex-resources/bwrap" ] ;;
    *) true ;;
  esac
}

update_current_link() {
  release_dir="$1"
  tmp_link="$STANDALONE_ROOT/.current.$$"

  replace_path_with_symlink "$CURRENT_LINK" "$release_dir" "$tmp_link"
}

update_visible_command() {
  mkdir -p "$BIN_DIR"
  tmp_link="$BIN_DIR/.codex.$$"

  replace_path_with_symlink "$BIN_PATH" "$CURRENT_LINK/bin/codex" "$tmp_link"
}

verify_visible_command() {
  "$BIN_PATH" --version >/dev/null
}

prune_old_releases() {
  active_release="$1"

  "$python_bin" - "$RELEASES_DIR" "$active_release" <<'PY'
from pathlib import Path
import shutil
import sys


releases_dir = Path(sys.argv[1]).resolve()
active_release = Path(sys.argv[2]).resolve()
if active_release.parent != releases_dir or not active_release.is_dir():
    raise SystemExit(f"refusing to prune around invalid active release: {active_release}")

previous_releases = sorted(
    (
        path
        for path in releases_dir.iterdir()
        if path != active_release
        and not path.name.startswith(".")
        and path.is_dir()
        and not path.is_symlink()
    ),
    key=lambda path: (
        (
            (1, parts[-2], path.name)
            if len(parts := path.name.rsplit("-", 2)) == 3
            and len(parts[-2]) == 14
            and parts[-2].isdigit()
            else (0, f"{path.stat().st_mtime_ns:020d}", path.name)
        )
    ),
    reverse=True,
)
for old_release in previous_releases[2:]:
    shutil.rmtree(old_release)
    print(f"Removed old standalone release: {old_release.name}")
PY
}

parse_args "$@"

if [ "${CODEX_UPSTREAM_VERSION+x}" = x ]; then
  use_upstream_version=true
fi

if is_windows_uname; then
  run_windows_local_installer "$@"
  exit $?
fi

require_command cargo
require_command mktemp
python_bin="$(resolve_python_bin)"

platform_target="$(resolve_platform_target)"
release_prefix="local-debug-$platform_target"
release_name="$(generate_release_name "$release_prefix")"
release_dir="$RELEASES_DIR/$release_name"

tmp_dir="$(mktemp -d)"
cleanup() {
  cleanup_status=$?
  trap - EXIT INT TERM HUP
  if ! restore_cargo_manifest_files; then
    cleanup_status=1
  fi
  release_version_lock
  release_install_lock
  if [ -n "$tmp_dir" ]; then
    rm -rf "$tmp_dir"
  fi
  exit "$cleanup_status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

acquire_install_lock
cleanup_stale_install_artifacts
step "Installing local debug build to $release_dir"
stage_release="$RELEASES_DIR/.staging.$release_name.$$"
if [ "$use_upstream_version" = true ]; then
  prepare_upstream_build_version
fi
build_local_package "$stage_release" "$platform_target"
if ! restore_cargo_manifest_files; then
  exit 1
fi
release_version_lock
if ! release_dir_is_complete "$stage_release" "$platform_target"; then
  rm -rf "$stage_release"
  echo "Local release validation failed." >&2
  exit 1
fi
if [ -e "$release_dir" ] || [ -L "$release_dir" ]; then
  rm -rf "$release_dir"
fi
mv "$stage_release" "$release_dir"
update_current_link "$release_dir"
update_visible_command
add_to_path
verify_visible_command
prune_old_releases "$release_dir"
release_install_lock
printf 'Activated local release: %s\n' "$release_name"
print_launch_instructions
