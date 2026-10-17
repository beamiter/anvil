#!/bin/sh
set -eu
[ "${1:-}" = --hosted-native-approved ] || {
  echo 'STOP: requires separately approved disposable hosted native runner.' >&2; exit 2;
}
shift
fixture=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# No inherited session/Python/loader settings; namespace refusal is terminal.
# No sockets are opened until this new user/network namespace is established.
exec /usr/bin/env -i PATH=/usr/bin:/bin LANG=C.UTF-8 \
  /usr/bin/unshare --user --map-root-user --net -- \
  /usr/bin/env ANVIL_HOSTED_FIXTURE=1 /usr/bin/python3 -s \
  "$fixture/run_suite.py" "${1:?absolute compiled probe path required}" "${2:?new private evidence directory required}"
