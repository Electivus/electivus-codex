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
cargo_toml_owned=""
cargo_lock_owned=""
cargo_lock_owned_missing=""
cargo_option=""
version_state_dir=""
version_lock_file=""
version_lock_path=""
version_lock_kind=""
version_lock_owner_file=""
version_transaction_dir=""
version_transaction_owned=false
install_lockf_pid=""
install_lockf_ready=""
install_lockf_control=""
version_lockf_pid=""
version_lockf_ready=""
version_lockf_control=""
active_child_pid=""
active_child_role=""
activation_pending=false
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
  --upstream-version VER   On Unix, use an explicit bare SemVer Release baseline.
                           This overrides CODEX_UPSTREAM_VERSION and enables
                           versioning.

On Unix, CODEX_UPSTREAM_VERSION supplies a validated override and enables
versioning when no explicit version argument is present.

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

  if [ ! -f "$cargo_toml_owned" ] ||
    ! cmp -s "$cargo_toml_owned" "$CODEX_RS_DIR/Cargo.toml"; then
    echo "Concurrent workspace edit detected in Cargo.toml; preserving the current bytes." >&2
    restore_failed=1
  fi
  if [ -f "$cargo_lock_owned" ]; then
    if ! cmp -s "$cargo_lock_owned" "$CODEX_RS_DIR/Cargo.lock"; then
      echo "Concurrent workspace edit detected in Cargo.lock; preserving the current bytes." >&2
      restore_failed=1
    fi
  elif [ -f "$cargo_lock_owned_missing" ]; then
    if [ -e "$CODEX_RS_DIR/Cargo.lock" ] || [ -L "$CODEX_RS_DIR/Cargo.lock" ]; then
      echo "Concurrent workspace edit detected in Cargo.lock; preserving the current bytes." >&2
      restore_failed=1
    fi
  else
    echo "The installer-owned Cargo.lock state is missing; refusing automatic restoration." >&2
    restore_failed=1
  fi

  if [ "$restore_failed" -ne 0 ]; then
    echo "Failed to restore and verify the Cargo workspace byte for byte." >&2
    print_version_transaction_recovery "$version_transaction_dir"
    return 1
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
    cargo_toml_owned=""
    cargo_lock_owned=""
    cargo_lock_owned_missing=""
    version_transaction_dir=""
    version_transaction_owned=false
  else
    echo "Failed to restore and verify the Cargo workspace byte for byte." >&2
    print_version_transaction_recovery "$version_transaction_dir"
  fi

  return "$restore_failed"
}

record_installer_owned_manifest_files() {
  if [ "$version_transaction_owned" != true ]; then
    return 0
  fi

  cargo_toml_owned="$version_transaction_dir/Cargo.toml.installer"
  cargo_lock_owned="$version_transaction_dir/Cargo.lock.installer"
  cargo_lock_owned_missing="$version_transaction_dir/Cargo.lock.installer-missing"
  cp "$CODEX_RS_DIR/Cargo.toml" "$cargo_toml_owned" || return 1
  rm -f "$cargo_lock_owned" "$cargo_lock_owned_missing"
  if [ -f "$CODEX_RS_DIR/Cargo.lock" ]; then
    cp "$CODEX_RS_DIR/Cargo.lock" "$cargo_lock_owned" || return 1
  else
    : >"$cargo_lock_owned_missing" || return 1
  fi
}

verify_builder_owned_manifest_files() {
  if [ "$version_transaction_owned" != true ]; then
    return 0
  fi
  if [ ! -f "$cargo_toml_owned" ] ||
    ! cmp -s "$cargo_toml_owned" "$CODEX_RS_DIR/Cargo.toml"; then
    echo "Concurrent workspace edit detected in Cargo.toml; refusing automatic restoration." >&2
    return 1
  fi
  if [ -f "$cargo_lock_owned" ]; then
    if ! cmp -s "$cargo_lock_owned" "$CODEX_RS_DIR/Cargo.lock"; then
      echo "Concurrent workspace edit detected in Cargo.lock; refusing automatic restoration." >&2
      return 1
    fi
  elif [ -f "$cargo_lock_owned_missing" ]; then
    if [ -e "$CODEX_RS_DIR/Cargo.lock" ] || [ -L "$CODEX_RS_DIR/Cargo.lock" ]; then
      echo "Concurrent workspace edit detected in Cargo.lock; refusing automatic restoration." >&2
      return 1
    fi
  else
    echo "The installer-owned Cargo.lock state is missing; refusing automatic restoration." >&2
    return 1
  fi
}

resolve_installer_owned_lockfile() {
  cargo metadata --format-version 1 --no-deps >/dev/null &
  active_child_pid=$!
  active_child_role="lockfile"
  lockfile_status=0
  wait_for_active_child || lockfile_status=$?
  active_child_role=""
  if [ "$lockfile_status" -ne 0 ]; then
    return "$lockfile_status"
  fi
  record_installer_owned_manifest_files
}

process_start_fingerprint() {
  identity_pid="$1"
  if [ -r "/proc/$identity_pid/stat" ]; then
    identity_start="$(sed 's/.*) //' "/proc/$identity_pid/stat" 2>/dev/null | awk '{ print $20 }')"
    identity_boot="$(cat /proc/sys/kernel/random/boot_id 2>/dev/null || true)"
    case "$identity_start" in
      '' | *[!0-9]*) return 1 ;;
    esac
    [ -n "$identity_boot" ] || return 1
    printf 'linux-proc:%s:%s\n' "$identity_boot" "$identity_start"
    return
  fi

  # A formatted ps start time is not a stable process identity: its precision
  # and format vary by platform, and it can match after PID reuse. Without the
  # Linux boot ID and /proc start time, fallback lock ownership is unprovable.
  return 1
}

report_unverifiable_lock() {
  unverifiable_lock="$1"
  unverifiable_description="$2"
  echo "Cannot safely verify the ownership metadata recorded by the $unverifiable_description lock at $unverifiable_lock: $fallback_lock_issue Refusing automatic deletion; manual recovery is required after confirming that no installer owns this path." >&2
}

report_lock_claim_error() {
  claim_error_lock="$1"
  claim_error_description="$2"
  echo "Cannot claim the $claim_error_description lock at $claim_error_lock: $fallback_lock_issue No lock ownership was established. Check filesystem hard-link support and permissions, remove only artifacts confirmed to be stale, and retry." >&2
}

fallback_lock_is_stale() {
  stale_lock="$1"
  stale_threshold="$2"
  fallback_lock_issue=""
  fallback_lock_retry_after=""
  stale_fingerprint=""
  if [ -L "$stale_lock" ]; then
    fallback_lock_issue="its metadata path is a symbolic link rather than an owned lock artifact."
    return 2
  elif [ -d "$stale_lock" ]; then
    stale_pid="$(cat "$stale_lock/pid" 2>/dev/null || true)"
    stale_started_at="$(cat "$stale_lock/started_at" 2>/dev/null || true)"
    stale_fingerprint="$(cat "$stale_lock/fingerprint" 2>/dev/null || true)"
  elif [ -f "$stale_lock" ]; then
    if ! stale_contents="$(cat "$stale_lock" 2>/dev/null)"; then
      if [ ! -e "$stale_lock" ] && [ ! -L "$stale_lock" ]; then
        return 1
      fi
      fallback_lock_issue="its metadata file cannot be read."
      return 2
    fi
    stale_pid="$(printf '%s\n' "$stale_contents" | sed -n '1p')"
    stale_started_at="$(printf '%s\n' "$stale_contents" | sed -n '2p')"
    stale_fingerprint="$(printf '%s\n' "$stale_contents" | sed -n 's/^fingerprint=//p' | head -n 1)"
  else
    if [ -e "$stale_lock" ]; then
      fallback_lock_issue="its metadata path is neither a regular file nor a legacy lock directory."
      return 2
    fi
    return 1
  fi
  case "$stale_pid" in
    '' | 0 | *[!0-9]* | ???????????*)
      if [ ! -e "$stale_lock" ] && [ ! -L "$stale_lock" ]; then
        return 1
      fi
      fallback_lock_issue="its PID metadata is missing or malformed."
      return 2
      ;;
  esac
  case "$stale_started_at" in
    '' | *[!0-9]* | ???????????????????*)
      if [ ! -e "$stale_lock" ] && [ ! -L "$stale_lock" ]; then
        return 1
      fi
      fallback_lock_issue="its started_at metadata is missing or malformed."
      return 2
      ;;
  esac
  stale_now="$(date +%s 2>/dev/null || printf '0')"
  case "$stale_now" in
    '' | 0 | *[!0-9]* | ???????????????????*)
      fallback_lock_issue="the current time cannot be verified against its started_at metadata."
      return 2
      ;;
  esac
  if [ "$stale_started_at" -gt "$stale_now" ]; then
    fallback_lock_issue="its started_at metadata is in the future."
    return 2
  fi
  if [ -n "$stale_pid" ] && kill -0 "$stale_pid" 2>/dev/null; then
    if [ -z "$stale_fingerprint" ]; then
      fallback_lock_issue="it has no process-start fingerprint, so PID $stale_pid cannot be proven to be the original owner."
      return 2
    fi
    current_fingerprint="$(process_start_fingerprint "$stale_pid" || true)"
    if [ -z "$current_fingerprint" ]; then
      fallback_lock_issue="this platform cannot prove the process-start identity of PID $stale_pid."
      return 2
    fi
    if [ "$current_fingerprint" != "$stale_fingerprint" ]; then
      fallback_lock_issue="PID $stale_pid is live but its process-start fingerprint does not match the recorded owner."
      return 2
    fi
    return 1
  fi
  stale_age=$((stale_now - stale_started_at))
  if [ "$stale_age" -ge "$stale_threshold" ]; then
    return 0
  fi
  fallback_lock_retry_after=$((stale_threshold - stale_age))
  return 3
}

try_claim_fallback_lock() {
  try_owner="$1"
  try_lock="$2"
  fallback_lock_issue=""

  if ln "$try_owner" "$try_lock" 2>/dev/null; then
    if [ -f "$try_lock" ] && cmp -s "$try_lock" "$try_owner"; then
      return 0
    fi

    # POSIX ln treats an existing directory as a destination directory.
    # Remove the hard link it created there and report real contention.
    try_nested_lock="$try_lock/$(basename "$try_owner")"
    if [ -e "$try_nested_lock" ] || [ -L "$try_nested_lock" ]; then
      if ! rm -f "$try_nested_lock" 2>/dev/null; then
        fallback_lock_issue="the hard-link claim landed inside a legacy lock directory and could not be removed."
        return 2
      fi
    fi
    return 1
  fi

  if [ -e "$try_lock" ] || [ -L "$try_lock" ]; then
    return 1
  fi
  fallback_lock_issue="the hard-link operation failed even though no competing lock exists."
  return 2
}

cleanup_stale_reclaim_markers() {
  cleanup_lock="$1"
  for cleanup_marker in "$cleanup_lock".reclaim.*; do
    [ -f "$cleanup_marker" ] || continue
    [ "$cleanup_marker" = "$cleanup_lock.reclaim.guard" ] && continue
    if fallback_lock_is_stale "$cleanup_marker" 0; then
      if ! rm -f "$cleanup_marker" 2>/dev/null; then
        fallback_lock_issue="the stale reclaim marker could not be removed."
        cleanup_reclaim_issue_path="$cleanup_marker"
        return 1
      fi
    elif [ -n "$fallback_lock_issue" ]; then
      cleanup_reclaim_issue_path="$cleanup_marker"
      return 1
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
    cleanup_reclaim_issue_path=""
    if ! cleanup_stale_reclaim_markers "$barrier_lock"; then
      report_unverifiable_lock "$cleanup_reclaim_issue_path" "reclaim marker"
      return 1
    fi
    barrier_guard="$barrier_lock.reclaim.guard"
    if [ -f "$barrier_guard" ]; then
      if fallback_lock_is_stale "$barrier_guard" 0; then
        echo "Stale reclaim guard at $barrier_guard requires manual removal; refusing an unsafe automatic takeover." >&2
        return 1
      elif [ -n "$fallback_lock_issue" ]; then
        report_unverifiable_lock "$barrier_guard" "reclaim guard"
        return 1
      fi
    fi
    reclaim_barrier_exists "$barrier_lock" || return 0
    sleep 1
  done
}

publish_reclaim_marker() {
  publish_lock="$1"
  if ! publish_prepare="$(mktemp "$publish_lock.reclaim-prepare.XXXXXX")"; then
    return 1
  fi
  publish_suffix="${publish_prepare##*.}"
  published_marker="$publish_lock.reclaim.$publish_suffix"
  publish_fingerprint="$(process_start_fingerprint "$$" || true)"
  if [ -z "$publish_fingerprint" ]; then
    rm -f "$publish_prepare" 2>/dev/null || true
    fallback_lock_issue="this platform cannot prove the reclaiming process identity without Linux /proc."
    return 1
  fi
  if ! {
    printf '%s\n' "$$"
    date +%s 2>/dev/null || printf '0\n'
    printf 'marker=%s\n' "$publish_suffix"
    printf 'fingerprint=%s\n' "$publish_fingerprint"
  } >"$publish_prepare"; then
    rm -f "$publish_prepare" 2>/dev/null || true
    return 1
  fi
  if ! mv "$publish_prepare" "$published_marker"; then
    rm -f "$publish_prepare" 2>/dev/null || true
    return 1
  fi
  printf '%s\n' "$published_marker"
}

acquire_reclaim_guard() {
  guard_lock="$1"
  guard_marker="$2"
  reclaim_guard="$guard_lock.reclaim.guard"
  active_reclaim_guard="$reclaim_guard"
  while :; do
    if ln "$guard_marker" "$reclaim_guard" 2>/dev/null; then
      break
    fi
    if [ -e "$reclaim_guard" ] || [ -L "$reclaim_guard" ]; then
      if fallback_lock_is_stale "$reclaim_guard" 0; then
        echo "Stale reclaim guard at $reclaim_guard requires manual removal; refusing an unsafe automatic takeover." >&2
        return 1
      elif [ -n "$fallback_lock_issue" ]; then
        report_unverifiable_lock "$reclaim_guard" "reclaim guard"
        return 1
      fi
      sleep 1
      continue
    fi
    fallback_lock_issue="the reclaim-guard hard-link operation failed even though no competing guard exists."
    report_lock_claim_error "$reclaim_guard" "reclaim guard"
    return 1
  done
  if [ ! -f "$reclaim_guard" ] || ! cmp -s "$reclaim_guard" "$guard_marker"; then
    remove_reclaim_guard_if_owned "$reclaim_guard" "$guard_marker"
    active_reclaim_guard=""
    return 1
  fi
}

remove_reclaim_guard_if_owned() {
  owned_guard="$1"
  owned_marker="$2"
  if [ -n "$owned_guard" ] &&
    [ -n "$owned_marker" ] &&
    [ -f "$owned_guard" ] &&
    [ -f "$owned_marker" ] &&
    cmp -s "$owned_guard" "$owned_marker"; then
    rm -f "$owned_guard" 2>/dev/null || true
  fi
}

release_reclaim_guard() {
  guard_marker="$1"
  remove_reclaim_guard_if_owned "$active_reclaim_guard" "$guard_marker"
  active_reclaim_guard=""
}

reclaim_fallback_lock() {
  reclaim_lock="$1"
  reclaim_owner_prefix="$2"
  reclaim_stale_threshold="$3"
  if ! active_reclaim_marker="$(publish_reclaim_marker "$reclaim_lock")"; then
    fallback_lock_issue="the reclaim marker could not be published safely."
    return 1
  fi
  reclaim_suffix="${active_reclaim_marker##*.}"
  if ! acquire_reclaim_guard "$reclaim_lock" "$active_reclaim_marker"; then
    rm -f "$active_reclaim_marker" 2>/dev/null || true
    active_reclaim_marker=""
    active_reclaim_guard=""
    return 1
  fi

  reclaim_failed=0
  if [ -d "$reclaim_lock" ]; then
    if fallback_lock_is_stale "$reclaim_lock" "$reclaim_stale_threshold"; then
      reclaimed_lock="$reclaim_lock.stale.$reclaim_suffix"
      if mv "$reclaim_lock" "$reclaimed_lock" 2>/dev/null; then
        if ! rm -rf "$reclaimed_lock"; then
          fallback_lock_issue="the stale legacy lock was moved aside but could not be removed from $reclaimed_lock."
          reclaim_failed=1
        fi
      elif [ -e "$reclaim_lock" ]; then
        fallback_lock_issue="the stale legacy lock directory could not be moved aside."
        reclaim_failed=1
      fi
    fi
  elif [ -f "$reclaim_lock" ]; then
    reclaimed_lock="$reclaim_lock.snapshot.$reclaim_suffix"
    if ln "$reclaim_lock" "$reclaimed_lock" 2>/dev/null; then
      if fallback_lock_is_stale "$reclaimed_lock" "$reclaim_stale_threshold"; then
        reclaimed_owner="$(sed -n '3p' "$reclaimed_lock" 2>/dev/null || true)"
        if ! rm -f "$reclaim_lock" 2>/dev/null ||
          [ -e "$reclaim_lock" ] || [ -L "$reclaim_lock" ]; then
          fallback_lock_issue="the stale lock was verified but could not be removed."
          reclaim_failed=1
        else
          case "$reclaimed_owner" in
            "$reclaim_owner_prefix"*) rm -f "$reclaimed_owner" 2>/dev/null || true ;;
          esac
        fi
      fi
      rm -f "$reclaimed_lock" 2>/dev/null || true
    elif [ -e "$reclaim_lock" ] || [ -L "$reclaim_lock" ]; then
      fallback_lock_issue="the stale-lock snapshot hard-link operation failed."
      reclaim_failed=1
    fi
  fi
  release_reclaim_guard "$active_reclaim_marker"
  rm -f "$active_reclaim_marker" 2>/dev/null || true
  active_reclaim_marker=""
  return "$reclaim_failed"
}

acquire_fallback_lock() {
  claim_owner="$1"
  claim_lock="$2"
  claim_owner_prefix="$3"
  claim_stale_threshold="$4"
  claim_description="$5"

  while :; do
    wait_for_reclaim_barrier "$claim_lock"
    claim_status=0
    try_claim_fallback_lock "$claim_owner" "$claim_lock" || claim_status=$?
    case "$claim_status" in
      0)
        wait_for_reclaim_barrier "$claim_lock"
        if [ -f "$claim_lock" ] && cmp -s "$claim_lock" "$claim_owner"; then
          return
        fi
        fallback_lock_issue="the published hard-link was replaced before ownership verification."
        report_lock_claim_error "$claim_lock" "$claim_description"
        return 1
        ;;
      1) ;;
      2)
        report_lock_claim_error "$claim_lock" "$claim_description"
        return 1
        ;;
      *)
        fallback_lock_issue="the hard-link claim returned an unknown state."
        report_lock_claim_error "$claim_lock" "$claim_description"
        return 1
        ;;
    esac

    stale_status=0
    fallback_lock_is_stale "$claim_lock" "$claim_stale_threshold" || stale_status=$?
    case "$stale_status" in
      0)
        warn "Removing stale $claim_description lock at $claim_lock"
        if ! reclaim_fallback_lock \
          "$claim_lock" "$claim_owner_prefix" "$claim_stale_threshold"; then
          report_lock_claim_error "$claim_lock" "$claim_description"
          return 1
        fi
        ;;
      1) sleep 1 ;;
      2)
        report_unverifiable_lock "$claim_lock" "$claim_description"
        return 1
        ;;
      3)
        echo "The $claim_description lock at $claim_lock records PID $stale_pid, which is no longer live, but the lock has not reached its stale threshold. Retry after $fallback_lock_retry_after seconds, or manually recover it only after confirming that no installer owns this path." >&2
        return 1
        ;;
      *)
        fallback_lock_issue="the existing lock returned an unknown ownership state."
        report_unverifiable_lock "$claim_lock" "$claim_description"
        return 1
        ;;
    esac
  done
}

start_lockf_holder() {
  holder_lock_file="$1"
  holder_ready_file="$2"
  holder_control_fifo="$3"
  lockf_holder_pid=""

  require_command mkfifo
  rm -f "$holder_ready_file" "$holder_control_fifo"
  mkfifo "$holder_control_fifo"
  # shellcheck disable=SC2016 # $1 and $2 belong to the lockf-held child shell.
  lockf -k "$holder_lock_file" sh -c '
    : >"$1"
    cat "$2" >/dev/null
  ' sh "$holder_ready_file" "$holder_control_fifo" &
  lockf_holder_pid=$!

  while [ ! -e "$holder_ready_file" ]; do
    if ! kill -0 "$lockf_holder_pid" 2>/dev/null; then
      lockf_status=0
      wait "$lockf_holder_pid" || lockf_status=$?
      rm -f "$holder_control_fifo"
      echo "lockf failed to acquire $holder_lock_file (exit $lockf_status)." >&2
      lockf_holder_pid=""
      return 1
    fi
    sleep 0.1
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

  if command -v lockf >/dev/null 2>&1; then
    version_lockf_ready="$version_state_dir/version.lockf.ready.$$"
    version_lockf_control="$version_state_dir/version.lockf.control.$$"
    start_lockf_holder "$version_lock_file" "$version_lockf_ready" "$version_lockf_control"
    version_lockf_pid="$lockf_holder_pid"
    lockf_holder_pid=""
    exec 6>"$version_lockf_control"
    version_lock_kind="lockf"
    return
  fi

  version_lock_owner_file="$(mktemp "$version_state_dir/version.lock.owner.XXXXXX")"
  owner_fingerprint="$(process_start_fingerprint "$$" || true)"
  if [ -z "$owner_fingerprint" ]; then
    echo "Cannot safely create the local-version fallback lock: this platform cannot prove process identity without Linux /proc. Install flock, use stock macOS lockf, or use an environment with a provable process start identity, then retry." >&2
    rm -f "$version_lock_owner_file" 2>/dev/null || true
    version_lock_owner_file=""
    return 1
  fi
  {
    printf '%s\n' "$$"
    date +%s 2>/dev/null || printf '0\n'
    printf '%s\n' "$version_lock_owner_file"
    printf 'fingerprint=%s\n' "$owner_fingerprint"
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
  if [ "$version_lock_kind" = "lockf" ]; then
    exec 6>&- 2>/dev/null || true
    if [ -n "$version_lockf_pid" ]; then
      wait "$version_lockf_pid" 2>/dev/null || true
    fi
    rm -f "$version_lockf_ready" "$version_lockf_control" 2>/dev/null || true
  fi
  if [ -n "$version_lock_owner_file" ]; then
    if [ -f "$version_lock_path" ] && cmp -s "$version_lock_path" "$version_lock_owner_file"; then
      rm -f "$version_lock_path" 2>/dev/null || true
    fi
    rm -f "$version_lock_owner_file" 2>/dev/null || true
  fi
  if [ -n "$active_reclaim_guard" ]; then
    remove_reclaim_guard_if_owned "$active_reclaim_guard" "$active_reclaim_marker"
    active_reclaim_guard=""
  fi
  if [ -n "$active_reclaim_marker" ]; then
    rm -f "$active_reclaim_marker" 2>/dev/null || true
    active_reclaim_marker=""
  fi
  version_lock_kind=""
  version_lock_owner_file=""
  version_lockf_pid=""
  version_lockf_ready=""
  version_lockf_control=""
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
  cargo_toml_owned="$version_transaction_dir/Cargo.toml.installer"
  cargo_lock_owned="$version_transaction_dir/Cargo.lock.installer"
  cargo_lock_owned_missing="$version_transaction_dir/Cargo.lock.installer-missing"
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
  record_installer_owned_manifest_files
  resolve_installer_owned_lockfile
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

  if command -v lockf >/dev/null 2>&1; then
    install_lockf_ready="$STANDALONE_ROOT/install.lockf.ready"
    install_lockf_control="$STANDALONE_ROOT/install.lockf.control.$$"
    start_lockf_holder "$LOCK_FILE" "$install_lockf_ready" "$install_lockf_control"
    install_lockf_pid="$lockf_holder_pid"
    lockf_holder_pid=""
    exec 7>"$install_lockf_control"
    lock_kind="lockf"
    return
  fi

  lock_owner_file="$(mktemp "$STANDALONE_ROOT/install.lock.owner.XXXXXX")"
  owner_fingerprint="$(process_start_fingerprint "$$" || true)"
  if [ -z "$owner_fingerprint" ]; then
    echo "Cannot safely create the installer fallback lock: this platform cannot prove process identity without Linux /proc. Install flock, use stock macOS lockf, or use an environment with a provable process start identity, then retry." >&2
    rm -f "$lock_owner_file" 2>/dev/null || true
    lock_owner_file=""
    return 1
  fi
  {
    printf '%s\n' "$$"
    date +%s 2>/dev/null || printf '0\n'
    printf '%s\n' "$lock_owner_file"
    printf 'fingerprint=%s\n' "$owner_fingerprint"
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
  if [ "$lock_kind" = "lockf" ]; then
    exec 7>&- 2>/dev/null || true
    if [ -n "$install_lockf_pid" ]; then
      wait "$install_lockf_pid" 2>/dev/null || true
    fi
    rm -f "$install_lockf_ready" "$install_lockf_control" 2>/dev/null || true
  fi
  if [ -n "$lock_owner_file" ]; then
    if [ -f "$LOCK_PATH" ] && cmp -s "$LOCK_PATH" "$lock_owner_file"; then
      rm -f "$LOCK_PATH" 2>/dev/null || true
    fi
    rm -f "$lock_owner_file" 2>/dev/null || true
  fi
  if [ -n "$active_reclaim_guard" ]; then
    remove_reclaim_guard_if_owned "$active_reclaim_guard" "$active_reclaim_marker"
    active_reclaim_guard=""
  fi
  if [ -n "$active_reclaim_marker" ]; then
    rm -f "$active_reclaim_marker" 2>/dev/null || true
    active_reclaim_marker=""
  fi
  lock_kind=""
  lock_owner_file=""
  install_lockf_pid=""
  install_lockf_ready=""
  install_lockf_control=""
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

wait_for_active_child() {
  child_status=0
  wait "$active_child_pid" || child_status=$?
  active_child_pid=""
  return "$child_status"
}

terminate_active_child() {
  if [ -n "$active_child_pid" ]; then
    kill -KILL "$active_child_pid" 2>/dev/null || true
    wait "$active_child_pid" 2>/dev/null || true
  fi
  active_child_pid=""
  active_child_role=""
}

run_package_builder() {
  package_dir="$1"
  target="$2"
  CARGO_PROFILE_DEV_DEBUG_ASSERTIONS=false
  export CARGO_PROFILE_DEV_DEBUG_ASSERTIONS

  if [ -n "${CODEX_LOCAL_RG:-}" ]; then
    exec "$python_bin" "$REPO_ROOT/scripts/build_codex_package.py" \
      --target "$target" \
      --variant codex \
      ${cargo_option:+"$cargo_option"} \
      --cargo-profile dev \
      --package-dir "$package_dir" \
      --rg-bin "$CODEX_LOCAL_RG" \
      --force
  else
    exec "$python_bin" "$REPO_ROOT/scripts/build_codex_package.py" \
      --target "$target" \
      --variant codex \
      ${cargo_option:+"$cargo_option"} \
      --cargo-profile dev \
      --package-dir "$package_dir" \
      --force
  fi
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
  fi

  cargo_option=""
  if [ "$version_transaction_owned" = true ]; then
    CODEX_LOCAL_REAL_CARGO="$(command -v cargo)"
    export CODEX_LOCAL_REAL_CARGO
    cargo_wrapper="$tmp_dir/cargo-locked"
    cat >"$cargo_wrapper" <<'EOF'
#!/bin/sh
exec "$CODEX_LOCAL_REAL_CARGO" "$@" --locked
EOF
    chmod +x "$cargo_wrapper"
    cargo_option="--cargo=$cargo_wrapper"
  fi

  run_package_builder "$package_dir" "$target" &
  active_child_pid=$!
  active_child_role="builder"
  builder_status=0
  wait_for_active_child || builder_status=$?
  if ! verify_builder_owned_manifest_files; then
    active_child_role=""
    return 1
  fi
  active_child_role=""
  if [ "$builder_status" -ne 0 ]; then
    return "$builder_status"
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

save_activation_path() {
  saved_path="$1"
  saved_name="$2"
  saved_type_path="$tmp_dir/$saved_name.type"
  saved_value_path="$tmp_dir/$saved_name.value"

  if [ -L "$saved_path" ]; then
    printf 'link\n' >"$saved_type_path"
    readlink "$saved_path" >"$saved_value_path"
  elif [ -f "$saved_path" ]; then
    printf 'file\n' >"$saved_type_path"
    cp -p "$saved_path" "$saved_value_path"
  else
    printf 'absent\n' >"$saved_type_path"
  fi
}

restore_activation_path() {
  restored_path="$1"
  restored_name="$2"
  restored_type="$(cat "$tmp_dir/$restored_name.type")"

  rm -f "$restored_path" || return 1
  case "$restored_type" in
    link)
      ln -s "$(cat "$tmp_dir/$restored_name.value")" "$restored_path" || return 1
      ;;
    file)
      cp -p "$tmp_dir/$restored_name.value" "$restored_path" || return 1
      ;;
    absent) ;;
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
  "$BIN_PATH" --version >/dev/null &
  active_child_pid=$!
  active_child_role="verifier"
  verifier_status=0
  wait_for_active_child || verifier_status=$?
  active_child_role=""
  return "$verifier_status"
}

rollback_pending_activation() {
  if [ "$activation_pending" != true ]; then
    return 0
  fi
  activation_pending=false

  warn "Activation failed; restoring the previous runnable installation."
  rollback_failed=0
  restore_activation_path "$CURRENT_LINK" current || rollback_failed=1
  restore_activation_path "$BIN_PATH" visible-codex || rollback_failed=1
  if [ "$rollback_failed" -ne 0 ]; then
    warn "Failed to restore every prior activation path; inspect $CURRENT_LINK and $BIN_PATH manually."
    return 1
  fi
}

activate_release() {
  release_dir="$1"
  save_activation_path "$CURRENT_LINK" current
  save_activation_path "$BIN_PATH" visible-codex
  activation_pending=true

  if update_current_link "$release_dir" &&
    update_visible_command &&
    verify_visible_command; then
    activation_pending=false
    return 0
  fi

  rollback_pending_activation || true
  return 1
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
  if [ "$upstream_version_override_set" = true ]; then
    echo "--upstream-version is Unix-only and cannot be delegated to install-local.ps1 on Windows Git Bash/MSYS/Cygwin." >&2
    echo "Windows support is deferred; see Electivus/electivus-codex issue #167." >&2
    exit 1
  fi
  if [ "${CODEX_UPSTREAM_VERSION+x}" = x ]; then
    echo "CODEX_UPSTREAM_VERSION is Unix-only and cannot be delegated to install-local.ps1 on Windows Git Bash/MSYS/Cygwin." >&2
    echo "Windows support is deferred; see Electivus/electivus-codex issue #167." >&2
    exit 1
  fi
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
handle_signal() {
  signal_status="$1"
  trap - INT TERM HUP
  signal_child_role="$active_child_role"
  terminate_active_child
  case "$signal_child_role" in
    builder) verify_builder_owned_manifest_files || true ;;
    lockfile) record_installer_owned_manifest_files || true ;;
  esac
  rollback_pending_activation || true
  exit "$signal_status"
}

cleanup() {
  cleanup_status=$?
  trap - EXIT INT TERM HUP
  cleanup_child_role="$active_child_role"
  terminate_active_child
  case "$cleanup_child_role" in
    builder) verify_builder_owned_manifest_files || true ;;
    lockfile) record_installer_owned_manifest_files || true ;;
  esac
  if ! rollback_pending_activation; then
    cleanup_status=1
  fi
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
trap 'handle_signal 130' INT
trap 'handle_signal 143' TERM
trap 'handle_signal 129' HUP

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
activate_release "$release_dir"
add_to_path
prune_old_releases "$release_dir"
release_install_lock
printf 'Activated local release: %s\n' "$release_name"
print_launch_instructions
