#!/bin/sh
# Runs the Oracle adapter's integration tests on Linux, inside a Rust
# container on the compose network, with the Oracle client library taken from
# the `oracle` test container (so no Instant Client is needed on this machine).
#
# Usage: npm run test:oracle [-- extra cargo test arguments]
set -eu
cd "$(dirname "$0")/.."
client="target/oracle-client"
if [ ! -f "$client/lib/libclntsh.so" ]; then
  echo "Copying the Oracle client library from the oracle container…"
  mkdir -p "$client"
  home=$(docker exec dataquery-oracle-1 bash -c 'echo $ORACLE_HOME')
  docker exec dataquery-oracle-1 bash -c "cd $home && tar cf - lib nls oracore rdbms/mesg network/mesg ldap/mesg crypto 2>/dev/null" \
    | tar xf - -C "$client"
  # `cat` follows the symlink, so the library itself is copied.
  docker exec dataquery-oracle-1 cat /lib64/libaio.so.1 > "$client/lib/libaio.so.1"
fi
exec docker run --rm --network dataquery_default \
  -v "$PWD":/src \
  -v "$PWD/$client":/oracle \
  -v "$HOME/.cargo/registry":/usr/local/cargo/registry \
  -v dataquery-linux-target:/target \
  -e CARGO_TARGET_DIR=/target -e CARGO_INCREMENTAL=0 \
  -e ORACLE_HOME=/oracle -e LD_LIBRARY_PATH=/oracle/lib -e ORACLE_BASE=/tmp/oracle \
  -e DATAQUERY_TEST_ORACLE=oracle:1521 \
  -w /src rust:1-bookworm cargo test -p dq-oracle "$@"
