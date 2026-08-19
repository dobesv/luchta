#!/bin/sh

# Nextest deliberately inherits its own environment into every test process.
# Strip that environment at the process boundary so developer- or CI-specific
# Luchta settings cannot silently change test behavior.
set -eu

# This canary proves that this wrapper both ran and removed a non-allowlisted
# variable. The luchta-test-support test checks the result inside nextest.
LUCHTA_HERMETIC_TEST_CANARY=present
export LUCHTA_HERMETIC_TEST_CANARY

HERMETIC_SAVED_IFS=$IFS
IFS='
'
for HERMETIC_ENV_ENTRY in $(env); do
    HERMETIC_ENV_NAME=${HERMETIC_ENV_ENTRY%%=*}

    # Only pass names that can safely be given to the shell's unset builtin.
    case "$HERMETIC_ENV_NAME" in
        [A-Za-z_]*)
            case "$HERMETIC_ENV_NAME" in
                *[!A-Za-z0-9_]*) continue ;;
            esac
            ;;
        *) continue ;;
    esac

    case "$HERMETIC_ENV_NAME" in
        # Executable lookup, user-directory discovery, temporary files, and
        # platform process startup need these host values.
        PATH | HOME | USERPROFILE | SYSTEMROOT | SystemRoot | WINDIR | COMSPEC | PATHEXT | TMPDIR | TMP | TEMP | PWD)
            ;;

        # Cargo and nextest inject per-test metadata and binary paths. Dynamic
        # loader variables are required for linked test artifacts.
        CARGO | CARGO_MANIFEST_DIR | CARGO_PKG_* | CARGO_BIN_EXE_* | CARGO_TARGET_TMPDIR | NEXTEST | NEXTEST_RUN_ID | NEXTEST_PROFILE | NEXTEST_VERSION | NEXTEST_WORKSPACE_ROOT | NEXTEST_BIN_EXE_* | NEXTEST_LD_* | NEXTEST_DYLD_* | LD_* | DYLD_*)
            ;;

        # Coverage and the real-rclone suite are the two intentional ambient
        # customizations supported by this repository's test commands.
        LLVM_PROFILE_FILE | LUCHTA_TEST_RCLONE)
            ;;
        *)
            unset "$HERMETIC_ENV_NAME"
            ;;
    esac
done
IFS=$HERMETIC_SAVED_IFS
unset HERMETIC_ENV_ENTRY HERMETIC_ENV_NAME HERMETIC_SAVED_IFS

# A positive marker makes a missing/misconfigured wrapper fail loudly.
LUCHTA_TEST_HERMETIC_WRAPPER=1
export LUCHTA_TEST_HERMETIC_WRAPPER

exec "$@"
